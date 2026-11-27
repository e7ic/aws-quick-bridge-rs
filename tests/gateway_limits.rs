#[path = "support/quick.rs"]
mod quick;

use aws_quick_bridge_rs::config::{AccountsFile, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::{gateway::start_server, pool::AccountPool};
use quick::{account, complete, receive, send};
use reqwest::{Client, RequestBuilder, Response};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;

async fn accept_turn(listener: &TcpListener) -> (WebSocketStream<TcpStream>, Value) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    while let Some(event) = receive(&mut ws).await {
        if event["type"] == "user_message" {
            return (ws, event["payload"].clone());
        }
    }
    panic!("Quick connection ended without receiving a prompt");
}

async fn reject_without_calling_quick(request: RequestBuilder, quick: &TcpListener) -> Response {
    tokio::select! {
        response = request.send() => response.unwrap(),
        incoming = quick.accept() => panic!("request opened a Quick connection: {incoming:?}"),
    }
}

async fn check_stream_frame_retry(
    quick: &TcpListener,
    pool: &AccountPool,
    request: impl Fn(Value) -> RequestBuilder,
) {
    let prompt = "文".repeat(17_000);
    let body = json!({
        "model":"smart",
        "stream":true,
        "messages":[{"role":"user","content":prompt}],
    });
    let frame_size = |payload: &Value| {
        serde_json::to_vec(&json!({"type":"user_message","payload":payload}))
            .unwrap()
            .len()
    };
    let error = json!({"code":"FrameTooLarge"});
    for (has_output, retry_succeeds) in [(false, true), (false, false), (true, false)] {
        let upstream = async {
            let (mut socket, first) = accept_turn(quick).await;
            assert_eq!(first["text"], prompt);
            assert!((44_101..=63_000).contains(&frame_size(&first)));
            if has_output {
                send(
                    &mut socket,
                    "text_delta",
                    json!({"text":"Already streamed"}),
                )
                .await;
            }
            send(&mut socket, "error", error.clone()).await;
            if has_output {
                return;
            }
            let (mut retry, resized) = accept_turn(quick).await;
            assert_eq!(resized["thread_id"], first["thread_id"]);
            assert_ne!(resized["message_id"], first["message_id"]);
            assert!(frame_size(&resized) <= 44_100);
            if retry_succeeds {
                send(
                    &mut retry,
                    "text_delta",
                    json!({"text":"Resized successfully"}),
                )
                .await;
                complete(&mut retry).await;
            } else {
                send(&mut retry, "error", error.clone()).await;
            }
        };
        let client = async {
            let response = request(body.clone()).send().await.unwrap();
            assert_eq!(
                response.status().as_u16(),
                if has_output || retry_succeeds {
                    200
                } else {
                    400
                }
            );
            let response = response.text().await.unwrap();
            if has_output {
                assert!(response.contains("Already streamed") && response.contains("event: error"));
            } else if retry_succeeds {
                assert!(
                    response.contains("Resized successfully")
                        && response.contains("event: message_stop")
                );
                assert!(!response.contains("event: error"));
            } else {
                assert!(response.contains("FrameTooLarge"));
            }
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(client, upstream)
        })
        .await
        .unwrap();
    }

    // Reaching the header deadline commits SSE even if Quick has not sent a token.
    let mut config = pool.config();
    config.options.insert("header_wait".into(), json!("50ms"));
    pool.update_config(config).unwrap();
    let (response, (mut socket, _)) = tokio::join!(request(body).send(), accept_turn(quick));
    let response = response.unwrap().error_for_status().unwrap();
    send(&mut socket, "error", error).await;
    let body = response.text().await.unwrap();
    assert!(body.contains("event: error") && body.contains("FrameTooLarge"));
    assert!(!body.contains("event: message_stop"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), quick.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn gateway_keeps_the_only_slot_until_a_stream_stops_then_allows_another_turn() {
    let dir = tempfile::tempdir().unwrap();
    // This integration binary has one test and an isolated account/config store.
    unsafe { std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path()) };
    let quick = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = account("only-account", quick.local_addr().unwrap().port(), 1);
    save_accounts(&AccountsFile {
        primary_key: account.key(),
        accounts: vec![account],
    })
    .unwrap();
    save_config(&BridgeConfig {
        api_key: "fixture-key".into(),
        options: serde_json::from_value(json!({
            "max_concurrency": 1,
            "proxy": "direct",
            "thinking_effort": "off",
            "attachments": false,
            "header_wait": "off",
        }))
        .unwrap(),
        ..BridgeConfig::default()
    })
    .unwrap();

    let pool = AccountPool::load().unwrap();
    let server = start_server(pool.clone(), "127.0.0.1", 0).await.unwrap();
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    let request = |route, body: Value| {
        client
            .post(format!("http://{}/v1/{route}", server.addr))
            .bearer_auth("fixture-key")
            .json(&body)
    };
    let responses = || request("responses", json!({"model":"smart","input":"Next turn"}));

    let stream = request(
        "chat/completions",
        json!({
            "model": "smart",
            "stream": true,
            "reasoning_effort": "max",
            "messages": [{"role": "user", "content": "Hold this turn open"}],
        }),
    );
    let first_reply = async {
        let (mut ws, frame) = accept_turn(&quick).await;
        assert_eq!(frame["thinking_effort"], "max");
        send(&mut ws, "text_delta", json!({"text": "First turn started"})).await;
        (ws, frame)
    };
    let (response, (mut socket, first_frame)) = tokio::join!(stream.send(), first_reply);
    let mut response = response.unwrap().error_for_status().unwrap();
    assert!(response.chunk().await.unwrap().is_some());
    assert_eq!(pool.snapshot().active_requests, 1);

    let assert_busy = |response: Response| async move {
        assert_eq!(response.status().as_u16(), 429);
        assert_eq!(response.headers()["retry-after"], "1");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["code"], "rate_limit_exceeded");
        assert!(body["error"]["message"].as_str().unwrap().contains("busy"));
    };
    assert_busy(reject_without_calling_quick(responses(), &quick).await).await;
    assert_eq!(pool.snapshot().total_requests, 1);

    drop(response);
    let stop = tokio::time::timeout(Duration::from_secs(4), receive(&mut socket))
        .await
        .expect("client disconnect should request a stop")
        .unwrap();
    assert_eq!(stop["type"], "stop_generation");
    assert_eq!(stop["payload"]["thread_id"], first_frame["thread_id"]);
    assert_busy(reject_without_calling_quick(responses(), &quick).await).await;

    let waiting = tokio::spawn(responses().send());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), quick.accept())
            .await
            .is_err()
    );
    assert!(
        !waiting.is_finished(),
        "a new conversation should wait briefly for capacity"
    );
    assert_eq!(pool.snapshot().active_requests, 1);
    let next_reply = async {
        send(
            &mut socket,
            "turn_aborted",
            json!({"thread_id": first_frame["thread_id"], "turn_message_id": first_frame["message_id"]}),
        )
        .await;
        let (mut ws, frame) = accept_turn(&quick).await;
        assert_eq!(frame["thinking_effort"], "off");
        send(
            &mut ws,
            "text_delta",
            json!({"text": "Next turn succeeded"}),
        )
        .await;
        complete(&mut ws).await;
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(waiting, next_reply)
    })
    .await
    .expect("the queued request should wake when the stopped turn releases its slot");
    assert!(response.unwrap().unwrap().status().is_success());
    assert_eq!(pool.snapshot().total_requests, 2);

    check_stream_frame_retry(&quick, &pool, |mut body| {
        body["max_tokens"] = json!(128);
        request("messages", body)
    })
    .await;

    let exhausted = async {
        let (mut ws, _) = accept_turn(&quick).await;
        send(&mut ws, "error", json!({"failure_type": "usage_limit"})).await;
    };
    let (response, ()) = tokio::join!(responses().send(), exhausted);
    let response = response.unwrap();
    assert_eq!(response.status().as_u16(), 429);
    assert!(response.headers().contains_key("retry-after"));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "insufficient_quota");
    assert_eq!(body["error"]["code"], "insufficient_quota");

    let response = reject_without_calling_quick(responses(), &quick).await;
    assert_eq!(response.status().as_u16(), 429);
    let retry: u64 = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=300).contains(&retry));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cooling down")
    );
    server.stop_and_wait().await;

    let mut probe = tokio::process::Command::new(env!("CARGO_BIN_EXE_aws-quick-bridge-rs"));
    probe.args(["probe", "smart", "--thinking-effort", "high,max,off,auto"]);
    let upstream = async {
        for effort in [Some("high"), Some("max"), Some("off"), None] {
            let (mut ws, frame) = accept_turn(&quick).await;
            assert_eq!(frame["thinking_effort"].as_str(), effort);
            assert_eq!(frame["agent_mode"], "smart");
            send(&mut ws, "text_delta", json!({"text":"Probe succeeded"})).await;
            complete(&mut ws).await;
        }
    };
    let (output, ()) = tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(probe.kill_on_drop(true).output(), upstream)
    })
    .await
    .unwrap();
    let output = output.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
