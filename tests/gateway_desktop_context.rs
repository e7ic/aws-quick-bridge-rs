//! Claude Desktop discovers context support from completed Quick turns.

use aws_quick_bridge_rs::config::{AccountsFile, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::gateway::start_server;
use aws_quick_bridge_rs::pool::AccountPool;
use reqwest::{Client, RequestBuilder, Response};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::timeout;

#[path = "support/quick.rs"]
mod quick;
use quick::{account, complete, receive, send};

const KEY: &str = "desktop-context-test-key";

async fn models(client: &Client, base: &str) -> Vec<Value> {
    let response: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["first_id"], response["data"][0]["id"]);
    serde_json::from_value(response["data"].clone()).unwrap()
}

async fn reply(quick: &TcpListener, request: RequestBuilder, mode: &str, usage: Value) -> Response {
    let upstream = async {
        let (tcp, _) = quick.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let turn = loop {
            let message = receive(&mut ws).await.unwrap();
            if message["type"] == "user_message" {
                break message;
            }
        };
        let auto = mode == "auto";
        assert_eq!(
            turn["payload"]["agent_mode"],
            if auto { "smart" } else { mode }
        );
        assert_eq!(turn["payload"]["auto_routing"], auto);
        send(&mut ws, "text_delta", json!({"text": "fixture reply"})).await;
        send(&mut ws, "turn_usage", usage).await;
        complete(&mut ws).await;
    };
    let (response, ()) = timeout(Duration::from_secs(8), async {
        tokio::join!(request.send(), upstream)
    })
    .await
    .expect("the gateway should issue a Quick turn");
    response.unwrap().error_for_status().unwrap()
}

#[tokio::test]
async fn discovered_routes_use_1m_only_after_quick_confirms_the_window() {
    let dir = tempfile::tempdir().unwrap();
    // This integration test is the only test in the process.
    unsafe { std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path()) };
    let quick = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = quick.local_addr().unwrap().port();
    let primary = account("primary", port, port);
    let secondary = account("secondary", port, port);
    let secondary_key = secondary.key();
    save_accounts(&AccountsFile {
        primary_key: primary.key(),
        accounts: vec![primary, secondary],
    })
    .unwrap();
    save_config(&BridgeConfig {
        api_key: KEY.into(),
        host: "127.0.0.1".into(),
        port: 0,
        options: serde_json::from_value(json!({"stateful": false})).unwrap(),
    })
    .unwrap();

    let pool = AccountPool::load().unwrap();
    let gateway = start_server(pool.clone(), "127.0.0.1", 0).await.unwrap();
    let base = format!("http://{}/claude-desktop", gateway.addr);
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(6))
        .build()
        .unwrap();
    let probe = |model: &Value| {
        client
            .post(format!("{base}/v1/messages"))
            .bearer_auth(KEY)
            .json(&json!({
                "model": model, "max_tokens": 1,
                "messages": [{"role": "user", "content": "."}]
            }))
    };
    let no_1m = |models: &[Value]| models.iter().all(|m| m.get("supports_1m").is_none());
    for (mode, id, window) in [
        ("smart", "claude-opus-bridge", 1_000_000),
        ("balanced", "claude-sonnet-bridge", 320_000),
        ("fast", "claude-haiku-bridge", 200_000),
        ("auto", "claude-auto-bridge", 1_000_000),
    ] {
        let mut config = pool.config();
        config.options.insert("default_model".into(), json!(mode));
        pool.update_config(config).unwrap();
        let discovered = models(&client, &base).await;
        assert_eq!(discovered[0]["id"], id);
        assert!(no_1m(&discovered));
        let response: Value = reply(
            &quick,
            probe(&discovered[0]["id"]),
            mode,
            json!({"resolved_model_id": "sonnet-5", "context_window": window}),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(response["content"][0]["text"], "fixture reply");
    }
    let discovered = models(&client, &base).await;
    assert!(no_1m(&discovered));
    let smart_id = &discovered[1]["id"];
    assert_eq!(smart_id, "claude-opus-bridge");
    pool.set_primary(&secondary_key).unwrap();
    reply(
        &quick,
        probe(smart_id),
        "smart",
        json!({"resolved_model_id": "sonnet-5", "context_window": 1_000_000}),
    )
    .await
    .bytes()
    .await
    .unwrap();

    let discovered = models(&client, &base).await;
    for model in &discovered {
        assert_eq!(
            model.get("supports_1m"),
            (model["id"] == *smart_id).then_some(&Value::Bool(true))
        );
    }
    let smart = &discovered[1];
    let response = reply(
        &quick,
        client
            .post(format!("{base}/v1/messages"))
            .bearer_auth(KEY)
            .header("anthropic-beta", "context-1m-2025-08-07")
            .json(&json!({
                "model": format!("{}[1m]", smart["id"].as_str().unwrap()),
                "max_tokens": 1024, "stream": true,
                "messages": [{"role": "user", "content": "continue the long conversation"}]
            })),
        "smart",
        json!({
            "resolved_model_id": "sonnet-5", "contextWindow": 1_000_000,
            "input_tokens": 50_000, "cache_read_tokens": 170_000, "cache_write_tokens": 20_000,
            "output_tokens": 1
        }),
    )
    .await;
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let events = response.text().await.unwrap();
    let usage = events
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(usage["usage"]["input_tokens"], 70_000);
    assert_eq!(usage["usage"]["cache_read_input_tokens"], 170_000);
    assert!(events.contains("event: message_stop"));
    assert_eq!(models(&client, &base).await[1]["supports_1m"], true);

    reply(
        &quick,
        probe(smart_id),
        "smart",
        json!({"resolved_model_id": "sonnet-5-5", "context_window": 200_000}),
    )
    .await
    .bytes()
    .await
    .unwrap();
    assert!(no_1m(&models(&client, &base).await));
    pool.set_enabled(&secondary_key, false).unwrap();
    assert_eq!(models(&client, &base).await[1]["supports_1m"], true);
    gateway.stop_and_wait().await;
}
