#[path = "support/quick.rs"]
mod quick;

use aws_quick_bridge_rs::config::{AccountsFile, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::{gateway::start_server, pool::AccountPool};
use axum::{Json, Router, http::HeaderMap, routing::post};
use quick::{account, complete, receive, send};
use reqwest::RequestBuilder;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;

async fn accept_mode(
    listener: &TcpListener,
    expected_mode: &str,
) -> (WebSocketStream<TcpStream>, Value) {
    let (socket, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
    while let Some(msg) = receive(&mut ws).await {
        if msg["type"] == "user_message" {
            let frame = msg["payload"].clone();
            assert_eq!(frame["agent_mode"], expected_mode);
            return (ws, frame);
        }
    }
    panic!("Quick should receive a turn");
}

async fn reply_to_turn(
    request: RequestBuilder,
    listener: &TcpListener,
    expected_mode: &str,
    reply: &str,
) -> (Value, Value) {
    let upstream = async {
        let (mut ws, frame) = accept_mode(listener, expected_mode).await;
        send(&mut ws, "text_delta", json!({"text": reply})).await;
        complete(&mut ws).await;
        frame
    };
    let (response, frame) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(request.send(), upstream)
    })
    .await
    .expect("gateway should use the local Quick fixture");
    let json = response
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (frame, json)
}

#[tokio::test]
async fn advanced_checks_account_capability_and_keeps_its_fallback_thread() {
    let dir = tempfile::tempdir().unwrap();
    // This test binary owns its isolated config store.
    unsafe { std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path()) };
    let quick = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http.local_addr().unwrap().port();
    let quick_port = quick.local_addr().unwrap().port();
    let lookups = Arc::new(AtomicUsize::new(0));
    let requests = lookups.clone();
    let rechecks = Arc::new(AtomicUsize::new(0));
    let rechecks_received = rechecks.clone();
    let qbs = Router::new().route(
        "/integration/quick-work/qbs-event",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let response = if body["eventBody"]["invokeAgent"]["body"]["type"] == "get_settings" {
                requests.fetch_add(1, Ordering::Relaxed);
                let settings = match headers["authorization"].to_str().unwrap() {
                    "Bearer fixture-denied" => json!({"advanced_mode_enabled": false}),
                    "Bearer fixture-enabled" | "Bearer fixture-runtime-denied" => json!({
                        "model_slots": [{"id": "advanced"}],
                    }),
                    "Bearer fixture-rechecked" => {
                        match rechecks_received.fetch_add(1, Ordering::Relaxed) {
                            0 => json!({}),
                            _ => json!({"advancedModeEnabled":false}),
                        }
                    }
                    "Bearer fixture-unknown"
                    | "Bearer fixture-unconfirmed"
                    | "Bearer fixture-late" => json!({}),
                    _ => panic!("unexpected fixture account"),
                };
                json!({"frames": [{"type": "settings", "payload": settings}]})
            } else {
                json!({})
            };
            async move { Json(response) }
        }),
    );
    let qbs_task = tokio::spawn(async move { axum::serve(http, qbs).await.unwrap() });
    save_config(&BridgeConfig {
        api_key: "fixture-key".into(),
        options: serde_json::from_value(json!({"proxy": "direct"})).unwrap(),
        ..BridgeConfig::default()
    })
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    for (id, mode) in [
        ("denied", "smart"),
        ("enabled", "advanced"),
        ("unknown", "advanced"),
    ] {
        let account = account(id, quick_port, http_port);
        save_accounts(&AccountsFile {
            primary_key: account.key(),
            accounts: vec![account],
        })
        .unwrap();
        let pool = AccountPool::load().unwrap();
        let server = start_server(pool.clone(), "127.0.0.1", 0).await.unwrap();
        let request = |model: &str, messages: Value| {
            client
                .post(format!("http://{}/v1/chat/completions", server.addr))
                .bearer_auth("fixture-key")
                .header("session-id", format!("fixture-thread-{id}"))
                .json(&json!({"model": model, "messages": messages}))
        };
        let first = json!({"role": "user", "content": "First question"});
        let (frame, reply) = reply_to_turn(
            request("advanced", json!([first])),
            &quick,
            mode,
            "First reply",
        )
        .await;
        assert_eq!(reply["model"], mode);
        if id == "denied" {
            let next = json!([
                first,
                {"role": "assistant", "content": "First reply"},
                {"role": "user", "content": "Follow up"}
            ]);
            let (reused, _) = reply_to_turn(
                request("advanced", next.clone()),
                &quick,
                mode,
                "Second reply",
            )
            .await;
            assert_eq!(reused["thread_id"], frame["thread_id"]);
            assert_eq!(reused["text"], "Follow up");
            let (direct, _) =
                reply_to_turn(request("smart", next), &quick, mode, "Direct smart reply").await;
            assert_ne!(direct["thread_id"], frame["thread_id"]);
            assert!(pool.snapshot().logs.iter().any(|log| {
                log.requested_model == "advanced" && log.message.contains("advanced -> smart")
            }));
        }
        server.stop_and_wait().await;
    }

    assert_eq!(lookups.load(Ordering::Relaxed), 3);
    let denied = json!({"code":"AdvancedModeNotEnabled"});
    let generic = json!({"message":"Something went wrong — please try again."});
    for (id, rejection, has_output, falls_back) in [
        ("runtime-denied", denied.clone(), false, true),
        ("rechecked", generic.clone(), false, true),
        ("unconfirmed", generic, false, false),
        ("late", denied, true, false),
    ] {
        let account = account(id, quick_port, http_port);
        save_accounts(&AccountsFile {
            primary_key: account.key(),
            accounts: vec![account],
        })
        .unwrap();
        let server = start_server(AccountPool::load().unwrap(), "127.0.0.1", 0)
            .await
            .unwrap();
        let request = |messages: Value, stream: bool| {
            client.post(format!("http://{}/v1/messages", server.addr))
                .bearer_auth("fixture-key")
                .header("session-id", id)
                .json(&json!({"model":"advanced","messages":messages,"max_tokens":128,"stream":stream}))
        };
        let original = json!({"role":"user","content":"Try advanced"});
        let upstream = async {
            let (mut ws, advanced) = accept_mode(&quick, "advanced").await;
            if has_output {
                send(&mut ws, "text_delta", json!({"text":"Already streamed"})).await;
            }
            send(&mut ws, "error", rejection).await;
            if !falls_back {
                return None;
            }
            let (mut ws, smart) = accept_mode(&quick, "smart").await;
            assert_ne!(smart["thread_id"], advanced["thread_id"]);
            send(&mut ws, "text_delta", json!({"text":"Smart fallback"})).await;
            complete(&mut ws).await;
            Some(smart)
        };
        let (response, smart) = tokio::time::timeout(Duration::from_secs(6), async {
            tokio::join!(request(json!([original]), true).send(), upstream)
        })
        .await
        .unwrap();
        let response = response.unwrap();
        assert_eq!(
            response.status().as_u16(),
            if falls_back || has_output { 200 } else { 502 }
        );
        let body = response.text().await.unwrap();
        if let Some(smart) = smart {
            assert!(body.contains("Smart fallback") && body.contains("event: message_stop"));
            let next = json!([
                original,
                {"role":"assistant","content":"Smart fallback"},
                {"role":"user","content":"Keep going"}
            ]);
            let (resumed, _) =
                reply_to_turn(request(next, false), &quick, "smart", "Continued").await;
            assert_eq!(resumed["thread_id"], smart["thread_id"]);
            assert_eq!(resumed["text"], "Keep going");
        } else if has_output {
            assert!(body.contains("Already streamed") && body.contains("event: error"));
            assert!(!body.contains("event: message_stop"));
        } else {
            assert!(body.contains("Something went wrong"));
        }
        server.stop_and_wait().await;
    }
    assert_eq!(rechecks.load(Ordering::Relaxed), 2);
    qbs_task.abort();
}
