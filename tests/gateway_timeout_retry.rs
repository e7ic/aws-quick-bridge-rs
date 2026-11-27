//! Replay a Desktop tool result through HTTP, the attachment service and the Quick socket.
#[path = "support/quick.rs"]
mod quick;

use aws_quick_bridge_rs::config::{AccountsFile, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::{gateway::start_server, pool::AccountPool};
use axum::{
    Json, Router,
    body::Bytes,
    routing::{get, post},
};
use quick::{account, complete, receive, send};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;

#[derive(Debug, PartialEq)]
enum Replay {
    SameRequest,
    MissingConfirmation,
    EditedResult,
    BeforeHeaders,
    BeforeHeadersMissingConfirmation,
}

async fn accept_turn(listener: &TcpListener) -> (WebSocketStream<TcpStream>, Value) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    while let Some(event) = receive(&mut ws).await {
        if event["type"] == "user_message" {
            return (ws, event["payload"].clone());
        }
    }
    panic!("socket closed before user_message");
}

#[tokio::test]
async fn desktop_retries_large_tool_result_on_original_thread_only_after_confirmed_stop() {
    let dir = tempfile::tempdir().unwrap();
    // This integration binary has its own fixture home.
    unsafe { std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path()) };
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = |name| {
        account(
            name,
            ws.local_addr().unwrap().port(),
            http.local_addr().unwrap().port(),
        )
    };
    let primary = fixture("retry");
    let backup = fixture("backup");
    let uploads = Arc::new(Mutex::new(Vec::<Bytes>::new()));
    let received = uploads.clone();
    let attachment_service = Router::new()
        .route(
            "/integration/quick-work/upload-attachment",
            post(move |body: Bytes| {
                received.lock().unwrap().push(body);
                async { Json(json!({"attachmentId":"00000000-0000-4000-8000-000000000001"})) }
            }),
        )
        .route(
            "/integration/quick-work/attachment-dlp-status",
            get(|| async { Json(json!({"dlpJobStatus":"COMPLETED","policyAction":"ALLOW"})) }),
        )
        .route(
            "/integration/quick-work/qbs-event",
            post(|Json(body): Json<Value>| async move {
                if body["eventBody"]["invokeAgent"]["body"]["type"] == "get_settings" {
                    return Json(json!({
                        "frames": [{"type":"settings", "payload":{"advanced_mode_enabled":false}}]
                    }));
                }
                Json(json!({"path":"/fixture/document.txt"}))
            }),
        );
    let http_task =
        tokio::spawn(async move { axum::serve(http, attachment_service).await.unwrap() });
    save_config(&BridgeConfig {
        host: "127.0.0.1".into(),
        port: 0,
        api_key: "fixture-key".into(),
        options: serde_json::from_value(json!({"request_timeout":"400ms","proxy":"direct"}))
            .unwrap(),
    })
    .unwrap();
    save_accounts(&AccountsFile {
        primary_key: primary.key(),
        accounts: vec![primary, backup],
    })
    .unwrap();
    let server = start_server(AccountPool::load().unwrap(), "127.0.0.1", 0)
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    let url = format!("http://{}/claude-desktop/v1/messages", server.addr);
    let document = "文".repeat(31_360);
    assert_eq!(document.len(), 94_080);

    for case in [
        Replay::SameRequest,
        Replay::MissingConfirmation,
        Replay::EditedResult,
        Replay::BeforeHeaders,
        Replay::BeforeHeadersMissingConfirmation,
    ] {
        let confirm_stop = !matches!(
            case,
            Replay::MissingConfirmation | Replay::BeforeHeadersMissingConfirmation
        );
        let before_headers = matches!(
            case,
            Replay::BeforeHeaders | Replay::BeforeHeadersMissingConfirmation
        );
        // The attachment arrives on the sixth reused turn, when the tool schema is due again.
        let warmups = if case == Replay::SameRequest { 5 } else { 0 };
        let model = if warmups > 0 { "advanced" } else { "smart" };
        uploads.lock().unwrap().clear();
        let prompt = format!("Look up the middle of document {case:?}");
        let conversation = async {
            let post = |messages: Value, stream: bool| {
                client.post(&url).bearer_auth("fixture-key")
                .json(&json!({"model":model,"max_tokens":64_000,"thinking":{"type":"adaptive"},"stream":stream,"messages":messages,
                    "tools":[{"name":"Read","input_schema":{"type":"object","properties":{"file":{"type":"string"}}}}]}))
            };
            let mut messages = Vec::new();
            for turn in 0..warmups {
                messages.push(json!({"role":"user","content":format!("Preparation {turn}")}));
                let reply: Value = post(json!(messages), false)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(reply["content"][0]["text"], "Ready");
                messages.push(json!({"role":"assistant","content":reply["content"]}));
            }
            messages.push(json!({"role":"user","content":prompt}));
            let reply: Value = post(json!(messages), false)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            let call = reply["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["type"] == "tool_use")
                .unwrap();
            let id = call["id"].as_str().unwrap();
            messages.extend([
                json!({"role":"assistant","content":reply["content"]}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":document}]}),
            ]);
            let mut messages = Value::Array(messages);
            let failed = post(messages.clone(), true).send().await.unwrap();
            assert_eq!(
                failed.status().as_u16(),
                if before_headers { 504 } else { 200 }
            );
            let failed = failed.text().await.unwrap();
            assert!(
                failed.contains("the upstream request timed out"),
                "{failed}"
            );
            assert!(
                before_headers
                    || (failed.contains("event: error")
                        && failed.contains("Checking the document"))
            );
            assert!(!failed.contains("<client_tool_ca"));
            if case == Replay::EditedResult {
                messages.as_array_mut().unwrap().last_mut().unwrap()["content"][0]["content"] =
                    json!(format!("Revised document\n{document}"));
            }
            let replay = post(messages.clone(), true)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(replay.contains("Document checked") && replay.contains("event: message_stop"));
            assert!(!replay.contains("event: error"));
            if case == Replay::SameRequest {
                messages.as_array_mut().unwrap().extend([
                    json!({"role":"assistant","content":"Document checked"}),
                    json!({"role":"user","content":"Continue with the next section"}),
                ]);
                let followup: Value = post(messages, false)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(followup["content"][0]["text"], "Follow-up complete");
            }
            id.to_owned()
        };
        let upstream = async {
            let mut anchored_thread = None;
            for turn in 0..warmups {
                let (mut ws, frame) = accept_turn(&ws).await;
                if turn == 0 {
                    assert!(
                        frame["custom_prompt"]
                            .as_str()
                            .unwrap()
                            .contains("\"parameters\"")
                    );
                    anchored_thread = Some(frame["thread_id"].clone());
                } else {
                    assert_eq!(Some(&frame["thread_id"]), anchored_thread.as_ref());
                    assert!(frame["custom_prompt"].is_null());
                }
                send(&mut ws, "text_delta", json!({"text":"Ready"})).await;
                complete(&mut ws).await;
            }
            let (mut socket, original) = accept_turn(&ws).await;
            if let Some(id) = anchored_thread {
                assert_eq!(original["thread_id"], id);
                assert!(original["custom_prompt"].is_null());
            }
            send(&mut socket,"text_delta",json!({"text":r#"<client_tool_call>{"name":"Read","arguments":{"file":"document.txt"}}</client_tool_call>"#})).await;
            complete(&mut socket).await;
            let (mut interrupted, mut first) = accept_turn(&ws).await;
            assert_eq!(original["thread_id"], first["thread_id"]);
            if warmups > 0 {
                let tool_spec = first["custom_prompt"].as_str().unwrap();
                assert!(tool_spec.contains("\"parameters\"") && tool_spec.contains("\"file\""));
                send(
                    &mut interrupted,
                    "text_delta",
                    json!({"text":"<client_tool_ca"}),
                )
                .await;
                send(&mut interrupted, "error", json!({"code":"FrameTooLarge"})).await;
                let (socket, resized) = accept_turn(&ws).await;
                assert_eq!(resized["thread_id"], first["thread_id"]);
                assert_eq!(resized["attachments"], first["attachments"]);
                assert_eq!(resized["custom_prompt"], first["custom_prompt"]);
                assert_ne!(resized["message_id"], first["message_id"]);
                assert!(
                    serde_json::to_vec(&json!({"type":"user_message","payload":resized}))
                        .unwrap()
                        .len()
                        <= 44_100
                );
                interrupted = socket;
                first = resized;
            }
            if !before_headers {
                send(
                    &mut interrupted,
                    "thinking_delta",
                    json!({"text":"Checking the document","turn_message_id":first["message_id"]}),
                )
                .await;
            }
            let stop = receive(&mut interrupted).await.unwrap();
            assert_eq!(stop["type"], "stop_generation");
            assert_eq!(stop["payload"]["thread_id"], first["thread_id"]);
            send(
                &mut interrupted,
                "message_complete",
                json!({"continuation_expected":true}),
            )
            .await;
            send(
                &mut interrupted,
                "turn_aborted",
                json!({"thread_id":"another-thread"}),
            )
            .await;
            send(
                &mut interrupted,
                "turn_aborted",
                json!({"thread_id":first["thread_id"],"turn_message_id":"an-earlier-turn"}),
            )
            .await;
            if confirm_stop {
                if before_headers {
                    send(
                        &mut interrupted,
                        "message_complete",
                        json!({"thread_id":first["thread_id"],"continuation_expected":false}),
                    )
                    .await;
                } else {
                    send(
                        &mut interrupted,
                        "turn_aborted",
                        json!({"thread_id":first["thread_id"]}),
                    )
                    .await;
                }
            }
            let (mut retried, second) = accept_turn(&ws).await;
            if warmups > 0 {
                let delta =
                    json!({"type":"text_delta","payload":{"text":"Document checked"}}).to_string();
                let (start, end) = delta.split_at(delta.len() / 2);
                for (index, data) in [(1, end), (0, start)] {
                    send(
                        &mut retried,
                        "_chunked_frame",
                        json!({"chunk_id":"reply","chunk_index":index,"total_chunks":2,"data":data}),
                    )
                    .await;
                }
            } else {
                send(
                    &mut retried,
                    "text_delta",
                    json!({"text":"Document checked"}),
                )
                .await;
            }
            complete(&mut retried).await;
            if case == Replay::SameRequest {
                let (mut continued, next) = accept_turn(&ws).await;
                assert_eq!(next["thread_id"], second["thread_id"]);
                assert!(
                    next["text"]
                        .as_str()
                        .unwrap()
                        .starts_with("Continue with the next section")
                );
                assert!(next["attachments"].is_null());
                assert!(next["custom_prompt"].is_null());
                send(
                    &mut continued,
                    "text_delta",
                    json!({"text":"Follow-up complete"}),
                )
                .await;
                complete(&mut continued).await;
            }
            (first, second)
        };
        let (call_id, (first, second)) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(conversation, upstream)
        })
        .await
        .unwrap();

        let resume = matches!(case, Replay::SameRequest | Replay::BeforeHeaders);
        assert_eq!(
            first["thread_id"] == second["thread_id"],
            resume,
            "resume only the confirmed, matching retry"
        );
        let expected = format!("Tool result (call_id={call_id}, name=Read): {document}");
        let uploaded = uploads.lock().unwrap();
        assert_eq!(uploaded[0].as_ref(), expected.as_bytes());
        for frame in [&first, &second] {
            assert_eq!(frame["agent_mode"], "smart");
            assert!(
                serde_json::to_vec(&json!({"type":"user_message","payload":frame}))
                    .unwrap()
                    .len()
                    <= 63_000
            );
            let file = &frame["attachments"][0];
            assert!(
                uploaded
                    .iter()
                    .any(|bytes| bytes.ends_with(document.as_bytes())
                        && file["contentSha256"] == hex::encode(Sha256::digest(bytes)))
            );
        }
        if resume {
            assert_eq!(
                uploaded.len(),
                1,
                "the existing full attachment belongs to this thread"
            );
            assert_eq!(first["attachments"], second["attachments"]);
            assert_eq!(first["custom_prompt"], second["custom_prompt"]);
            if warmups == 0 {
                assert!(second["custom_prompt"].is_null());
            }
        }
    }
    server.stop_and_wait().await;
    http_task.abort();
}
