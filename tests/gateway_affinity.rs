#[path = "support/quick.rs"]
mod quick;

use aws_quick_bridge_rs::config::{AccountsFile, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::{gateway::start_server, pool::AccountPool};
use quick::{account, complete, receive, send};
use reqwest::{RequestBuilder, Response};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;

async fn accept_turn(listener: &TcpListener) -> (WebSocketStream<TcpStream>, Value) {
    timeout(Duration::from_secs(3), async {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(event) = receive(&mut socket).await {
            if event["type"] == "user_message" {
                return (socket, event["payload"].clone());
            }
        }
        panic!("Quick connection closed before user_message");
    })
    .await
    .expect("expected a request on this account")
}

async fn start_stream(
    request: RequestBuilder,
    listener: &TcpListener,
) -> (Response, WebSocketStream<TcpStream>, Value) {
    let upstream = async {
        let (mut socket, frame) = accept_turn(listener).await;
        send(&mut socket, "text_delta", json!({"text":"Fixture answer"})).await;
        (socket, frame)
    };
    let (response, (socket, frame)) = tokio::join!(request.send(), upstream);
    (response.unwrap().error_for_status().unwrap(), socket, frame)
}

async fn finish_stream(response: Response, socket: &mut WebSocketStream<TcpStream>) {
    complete(socket).await;
    let stream = response.text().await.unwrap();
    assert!(stream.contains("event: message_stop"));
    assert!(!stream.contains("event: error"));
}

async fn assert_no_immediate_failover(backup: &TcpListener) {
    assert!(
        timeout(Duration::from_millis(120), backup.accept())
            .await
            .is_err(),
        "the backup received a request while the preferred slot could still free up"
    );
}

#[tokio::test]
async fn waits_for_a_busy_conversation_and_binds_only_after_a_successful_stream() {
    let dir = tempfile::tempdir().unwrap();
    // This integration binary uses its own fixture home.
    unsafe { std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path()) };
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = account("a", primary.local_addr().unwrap().port(), 1);
    let b = account("b", backup.local_addr().unwrap().port(), 1);
    save_accounts(&AccountsFile {
        primary_key: a.key(),
        accounts: vec![a, b],
    })
    .unwrap();
    save_config(&BridgeConfig {
        api_key: "fixture-key".into(),
        options: serde_json::from_value(json!({
            "max_concurrency":1, "proxy":"direct", "stateful":true, "header_wait":"off",
        }))
        .unwrap(),
        ..BridgeConfig::default()
    })
    .unwrap();
    let pool = AccountPool::load().unwrap();
    let server = start_server(pool.clone(), "127.0.0.1", 0).await.unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    let request = |session: &str, messages: Value| {
        client
            .post(format!("http://{}/v1/messages", server.addr))
            .bearer_auth("fixture-key")
            .header("x-session-id", session)
            .json(&json!({"model":"smart","max_tokens":128,"stream":true,"messages":messages}))
    };
    let single = |session, prompt| request(session, json!([{"role":"user","content":prompt}]));
    let continuation = |prompt| {
        request(
            "",
            json!([
                {"role":"user","content":"Start the thread"},
                {"role":"assistant","content":"Fixture answer"},
                {"role":"user","content":prompt},
            ]),
        )
    };

    let (response, mut socket, first) =
        start_stream(single("", "Start the thread"), &primary).await;
    finish_stream(response, &mut socket).await;
    let (mut holding, mut holder, active) =
        start_stream(continuation("Hold this turn"), &primary).await;
    assert_eq!(active["thread_id"], first["thread_id"]);
    assert!(holding.chunk().await.unwrap().is_some());

    let waiting = tokio::spawn(continuation("Continue after the stop").send());
    assert_no_immediate_failover(&backup).await;
    assert_eq!(pool.snapshot().active_requests, 1);
    drop(holding);
    let stop = timeout(Duration::from_secs(2), receive(&mut holder))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop["type"], "stop_generation");
    assert_eq!(stop["payload"]["thread_id"], active["thread_id"]);
    send(
        &mut holder,
        "turn_aborted",
        json!({
            "thread_id":active["thread_id"], "turn_message_id":active["message_id"],
        }),
    )
    .await;
    let (mut socket, resumed) = accept_turn(&primary).await;
    assert_eq!(resumed["thread_id"], active["thread_id"]);
    assert_eq!(resumed["text"], "Continue after the stop");
    send(&mut socket, "text_delta", json!({"text":"Resumed"})).await;
    finish_stream(waiting.await.unwrap().unwrap(), &mut socket).await;

    let (response, mut socket, _) =
        start_stream(single("sticky", "Bind the primary"), &primary).await;
    finish_stream(response, &mut socket).await;
    for succeeds in [false, true] {
        let (holding, mut holder, _) =
            start_stream(single("", "Fill the primary slot"), &primary).await;
        let pending = tokio::spawn(single("sticky", "Use the backup if needed").send());
        assert_no_immediate_failover(&backup).await;
        let (mut alternate, _) = accept_turn(&backup).await;
        send(
            &mut alternate,
            "text_delta",
            json!({"text":"Backup started"}),
        )
        .await;
        let response = pending.await.unwrap().unwrap().error_for_status().unwrap();
        if succeeds {
            finish_stream(response, &mut alternate).await;
        } else {
            send(
                &mut alternate,
                "stream_error",
                json!({"message":"Backup failed mid-stream"}),
            )
            .await;
            let stream = response.text().await.unwrap();
            assert!(stream.contains("Backup started") && stream.contains("event: error"));
            assert!(!stream.contains("event: message_stop"));
        }
        finish_stream(holding, &mut holder).await;

        let expected = if succeeds { &backup } else { &primary };
        let (response, mut socket, _) =
            start_stream(single("sticky", "Follow the binding"), expected).await;
        finish_stream(response, &mut socket).await;
    }
    server.stop_and_wait().await;
}
