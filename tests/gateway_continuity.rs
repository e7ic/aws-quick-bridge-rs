//! Exercises the real HTTP gateway with a local Quick server and fixture credentials.

use aws_quick_bridge_rs::config::{AccountsFile, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::gateway::{RunningServer, start_server};
use aws_quick_bridge_rs::pool::{AccountHealth, AccountPool};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::Request;

#[path = "support/quick.rs"]
mod quick;
use quick::{account, complete, receive, send};

const SIGN_IN: &str = "test-gateway-key";
const LIMIT_NOTICE: &str = "You've reached your usage limit. Check your profile for details.";
const DESKTOP_COMMAND: &str = r#"cd "$TEMP" && printf '%s' '“示例”'"#;

#[expect(
    clippy::result_large_err,
    reason = "Tungstenite defines the handshake callback response type"
)]
async fn quick_turn(
    stream: TcpStream,
    turns: mpsc::UnboundedSender<(String, Value)>,
    stopped: mpsc::UnboundedSender<Value>,
) {
    let mut account = String::new();
    let mut ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, response| {
        account = req.headers()["authorization"].to_str().unwrap().to_owned();
        Ok(response)
    })
    .await
    .unwrap();
    while let Some(msg) = receive(&mut ws).await {
        if msg["type"] != "user_message" {
            continue;
        }
        let input = &msg["payload"];
        let text = input["text"].as_str().unwrap();
        turns.send((account.clone(), input.clone())).unwrap();
        if text.contains("wait for cancellation") {
            send(&mut ws, "text_delta", json!({"text":"stream started"})).await;
            while let Some(event) = receive(&mut ws).await {
                if event["type"] == "stop_generation" {
                    stopped.send(event["payload"]["thread_id"].clone()).unwrap();
                    send(
                        &mut ws,
                        "turn_usage",
                        json!({
                            "input_tokens": 75, "cache_read_tokens": 0, "cache_write_tokens": 0,
                            "context_window": 320_000
                        }),
                    )
                    .await;
                    send(&mut ws, "turn_aborted", json!({})).await;
                    return;
                }
            }
        } else {
            if text.contains("rebuild old thread") && !text.contains("remember this question") {
                send(
                    &mut ws,
                    "session_error",
                    json!({"message":"Quick thread no longer exists"}),
                )
                .await;
                return;
            }
            if text == "rebuild full context" {
                send(
                    &mut ws,
                    "session_error",
                    json!({"error_code":"context_window_exceeded"}),
                )
                .await;
                return;
            }
            if text.contains("interrupted upstream") {
                send(&mut ws, "text_delta", json!({"text":"partial reply"})).await;
                send(
                    &mut ws,
                    "turn_usage",
                    json!({
                        "input_tokens": 20, "cache_read_tokens": 40, "cache_write_tokens": 40,
                        "context_window": 200_000, "resolved_model_id": "fixture-model"
                    }),
                )
                .await;
                send(
                    &mut ws,
                    "stream_error",
                    json!({"message":"upstream disconnected"}),
                )
                .await;
                return;
            }
            if text.contains("all allowances spent") {
                if account == "Bearer fixture-a" {
                    send(&mut ws, "error", json!({"failure_type":"usage_limit"})).await;
                } else {
                    send(&mut ws, "message_complete", json!({"text":LIMIT_NOTICE})).await;
                }
                return;
            }
            if text.contains("spend allowance") && account == "Bearer fixture-a" {
                send(
                    &mut ws,
                    "text_delta",
                    json!({"text":"You've reached your usage limit."}),
                )
                .await;
                send(
                    &mut ws,
                    "text_delta",
                    json!({"text":" Check your profile for details."}),
                )
                .await;
            } else if text.starts_with("read Desktop fixture") {
                for delta in [
                    "Reading the document. ",
                    "<client_tool",
                    r#"_call>{“name”:“Bash”,“arguments”:{“command”:“cd \"$TEMP\" && printf '%s' '“示例”'”}}</client_tool_call>"#,
                ] {
                    send(&mut ws, "text_delta", json!({"text":delta})).await;
                }
            } else if text.starts_with("blocked Desktop tool") {
                send(&mut ws, "text_delta", json!({
                    "text":r#"<client_tool_call>{"name":"private_tool","arguments":{}}</client_tool_call>"#
                })).await;
            } else if text.starts_with("invalid Desktop tool") {
                send(
                    &mut ws,
                    "text_delta",
                    json!({"text":"Checking the document. "}),
                )
                .await;
                send(
                    &mut ws,
                    "text_delta",
                    json!({"text":"<client_tool_call>{invalid}</client_tool_call>"}),
                )
                .await;
            } else if text.contains("quote allowance") {
                send(&mut ws, "text_delta", json!({"text":LIMIT_NOTICE})).await;
                send(&mut ws, "text_delta", json!({"text":" is an example."})).await;
            } else {
                send(&mut ws, "text_delta", json!({"text":"fixture answer"})).await;
                if text == "continue" {
                    send(&mut ws, "turn_usage", json!({
                        "input_tokens": 4, "cache_read_tokens": 32_910, "cache_write_tokens": 37_233,
                        "output_tokens": 4, "context_window": 262_144
                    })).await;
                    send(&mut ws, "turn_usage", json!({"output_tokens": 1})).await;
                }
            }
            complete(&mut ws).await;
        }
        return;
    }
}

fn request(
    client: &reqwest::Client,
    gateway: &RunningServer,
    messages: Value,
    stream: bool,
) -> reqwest::RequestBuilder {
    client
        .post(format!("http://{}/v1/chat/completions", gateway.addr))
        .bearer_auth(SIGN_IN)
        .header("x-session-id", "fixture-conversation")
        .json(&json!({ "model": "smart", "messages": messages, "stream": stream }))
}

async fn gateway(pool: Arc<AccountPool>) -> RunningServer {
    start_server(pool, "127.0.0.1", 0).await.unwrap()
}

async fn wait_for_log(pool: &AccountPool, api: &str, status: u16, count: usize) {
    let mut changes = pool.subscribe();
    timeout(Duration::from_secs(2), async {
        while pool
            .snapshot()
            .logs
            .iter()
            .filter(|log| log.api == api && log.status == status)
            .count()
            < count
        {
            changes.recv().await.unwrap();
        }
    })
    .await
    .unwrap();
}

async fn check_desktop_tool_bridge(
    client: &reqwest::Client,
    gateway: &RunningServer,
    turns: &mut mpsc::UnboundedReceiver<(String, Value)>,
) {
    let tools = json!([
        {"name":"Bash","defer_loading":true,"input_schema":{
            "type":"object","required":["command"],"properties":{"command":{"type":"string"}}
        }},
        {"name":"private_tool","allowed_callers":["code_execution_20260120"],"input_schema":{"type":"object"}}
    ]);
    let request = |prompt: &str, stream: bool, tools: &Value| {
        client
            .post(format!(
                "http://{}/claude-desktop/v1/messages",
                gateway.addr
            ))
            .bearer_auth(SIGN_IN)
            .json(&json!({
                "model":"claude-opus-bridge", "stream":stream, "max_tokens":128,
                "tools":tools, "messages":[{"role":"user","content":prompt}]
            }))
    };

    for stream in [false, true] {
        let response = request("read Desktop fixture", stream, &tools)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!response.contains("<client_tool_call>"));
        assert!(response.contains("Reading the document."));
        if stream {
            let events: Vec<Value> = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|data| serde_json::from_str(data).unwrap())
                .collect();
            let call = events
                .iter()
                .find(|event| event["content_block"]["type"] == "tool_use")
                .unwrap();
            assert_eq!(call["content_block"]["name"], "Bash");
            let args = events
                .iter()
                .find_map(|event| event["delta"]["partial_json"].as_str())
                .unwrap();
            let args: Value = serde_json::from_str(args).unwrap();
            assert_eq!(args["command"], DESKTOP_COMMAND);
            assert!(
                events
                    .iter()
                    .any(|event| event["delta"]["stop_reason"] == "tool_use")
            );
        } else {
            let response: Value = serde_json::from_str(&response).unwrap();
            let call = &response["content"][1];
            assert_eq!(call["type"], "tool_use");
            assert_eq!(call["name"], "Bash");
            assert_eq!(call["input"]["command"], DESKTOP_COMMAND);
            assert_eq!(response["stop_reason"], "tool_use");
        }
        let (_, sent) = turns.recv().await.unwrap();
        assert!(
            !sent["custom_prompt"]
                .as_str()
                .unwrap()
                .contains("private_tool")
        );
    }

    let blocked = request("blocked Desktop tool", false, &tools)
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), 502);
    let error: Value = blocked.json().await.unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("current tool list")
    );
    turns.recv().await.unwrap();
    assert!(
        turns.is_empty(),
        "invalid tool output must not try another account"
    );

    let invalid = request("invalid Desktop tool", true, &json!([]))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    turns.recv().await.unwrap();
    assert!(invalid.contains("Checking the document."));
    assert!(invalid.contains("event: error"));
    assert!(invalid.contains("invalid tool call data"));
    assert!(!invalid.contains("<client_tool_call>"));
    assert!(!invalid.contains("event: message_stop"));
}

#[tokio::test]
async fn resumes_after_restart_fails_over_on_quota_and_stops_on_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    // This integration test is the only test in its process. All workers use this fixture home.
    unsafe { std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path()) };
    let quick = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let quick_port = quick.local_addr().unwrap().port();
    let (turns_tx, mut turns) = mpsc::unbounded_channel();
    let (stopped_tx, mut stopped) = mpsc::unbounded_channel();
    let quick_task = tokio::spawn(async move {
        while let Ok((tcp, _)) = quick.accept().await {
            tokio::spawn(quick_turn(tcp, turns_tx.clone(), stopped_tx.clone()));
        }
    });
    let usage = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let usage_port = usage.local_addr().unwrap().port();
    let usage_task = tokio::spawn(async {
        let app = axum::Router::new().fallback(|| async {
            axum::Json(json!({"session":{"used":100,"limit":100,"resume_in_minutes":4}}))
        });
        axum::serve(usage, app).await.unwrap();
    });

    save_config(&BridgeConfig {
        api_key: SIGN_IN.into(),
        port: 0,
        host: "127.0.0.1".into(),
        options: serde_json::from_value(json!({"header_wait":"off","stateful":true})).unwrap(),
    })
    .unwrap();
    let primary = account("a", quick_port, usage_port);
    save_accounts(&AccountsFile {
        primary_key: primary.key(),
        accounts: vec![primary, account("b", quick_port, usage_port)],
    })
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(6))
        .build()
        .unwrap();
    let first = gateway(AccountPool::load().unwrap()).await;
    check_desktop_tool_bridge(&client, &first, &mut turns).await;
    request(
        &client,
        &first,
        json!([{"role":"user","content":"remember this question"}]),
        false,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let (_, original) = turns.recv().await.unwrap();
    first.stop_and_wait().await;

    let pool = AccountPool::load().unwrap();
    let running = gateway(pool.clone()).await;
    request(
        &client,
        &running,
        json!([
            {"role":"user","content":"remember this question"},
            {"role":"assistant","content":"fixture answer"},
            {"role":"user","content":"continue"}
        ]),
        false,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let (_, continuation) = turns.recv().await.unwrap();
    let context = pool
        .snapshot()
        .logs
        .into_iter()
        .find(|entry| entry.api == "chat" && entry.status == 200)
        .and_then(|entry| entry.context)
        .unwrap();
    assert_eq!(context.used_tokens, Some(70_147));
    assert_eq!(
        context.window_tokens.map(|value| value.get()),
        Some(262_144)
    );

    let mut history = vec![
        json!({"role":"user","content":"remember this question"}),
        json!({"role":"assistant","content":"fixture answer"}),
        json!({"role":"user","content":"continue"}),
        json!({"role":"assistant","content":"fixture answer"}),
    ];
    let mut previous = continuation["thread_id"].clone();
    for next in ["rebuild old thread", "rebuild full context"] {
        history.push(json!({"role":"user","content":next}));
        request(&client, &running, json!(history), false)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let (_, stale) = turns.recv().await.unwrap();
        let (_, rebuilt) = turns.recv().await.unwrap();
        assert_eq!(stale["thread_id"], previous);
        assert_ne!(rebuilt["thread_id"], stale["thread_id"]);
        let text = rebuilt["text"].as_str().unwrap();
        assert!(text.contains("remember this question") && text.contains(next));
        previous = rebuilt["thread_id"].clone();
        history.push(json!({"role":"assistant","content":"fixture answer"}));
    }

    let quote = request(
        &client,
        &running,
        json!([{"role":"user","content":"quote allowance"}]),
        true,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .text()
    .await
    .unwrap();
    turns.recv().await.unwrap();
    let failover = request(
        &client,
        &running,
        json!([{"role":"user","content":"spend allowance"}]),
        true,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .text()
    .await
    .unwrap();
    let (quota_account, _) = turns.recv().await.unwrap();
    let fallback = turns.try_recv().ok();
    let primary_health = pool.snapshot().accounts[0].health.clone();

    let mut streaming = request(
        &client,
        &running,
        json!([{"role":"user","content":"wait for cancellation"}]),
        true,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let (_, interrupted) = turns.recv().await.unwrap();
    while let Some(chunk) = streaming.chunk().await.unwrap() {
        if String::from_utf8_lossy(&chunk).contains("stream started") {
            break;
        }
    }
    drop(streaming);
    let stop = timeout(Duration::from_secs(2), stopped.recv())
        .await
        .ok()
        .flatten();
    wait_for_log(&pool, "chat", 499, 1).await;

    let pending = request(
        &client,
        &running,
        json!([{"role":"user","content":"wait for cancellation"}]),
        false,
    );
    let pending = tokio::spawn(pending.send());
    let (_, before_headers) = turns.recv().await.unwrap();
    pending.abort();
    let stopped_before_headers = timeout(Duration::from_secs(2), stopped.recv())
        .await
        .unwrap();
    assert_eq!(
        stopped_before_headers,
        Some(before_headers["thread_id"].clone())
    );
    wait_for_log(&pool, "chat", 499, 2).await;
    for entry in pool
        .snapshot()
        .logs
        .iter()
        .filter(|entry| entry.api == "chat" && entry.status == 499)
    {
        let context = entry.context.unwrap();
        assert_eq!(context.used_tokens, Some(75));
        assert_eq!(
            context.window_tokens.map(|value| value.get()),
            Some(320_000)
        );
    }

    for (path, api, body, event) in [
        (
            "/v1/messages",
            "messages",
            json!({"model":"smart","stream":true,"max_tokens":128,"messages":[{"role":"user","content":"interrupted upstream"}]}),
            "event: error",
        ),
        (
            "/v1/responses",
            "responses",
            json!({"model":"smart","stream":true,"input":"interrupted upstream"}),
            "event: response.failed",
        ),
    ] {
        let response = client
            .post(format!("http://{}{path}", running.addr))
            .bearer_auth(SIGN_IN)
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        turns.recv().await.unwrap();
        assert!(response.contains("partial reply"));
        assert!(response.contains(event));
        assert!(!response.contains("response.completed"));
        assert!(!response.contains("event: message_stop"));
        wait_for_log(&pool, api, 502, 1).await;
        let error = pool
            .snapshot()
            .logs
            .into_iter()
            .find(|entry| entry.api == api && entry.status == 502)
            .unwrap();
        assert_eq!(error.resolved_model, "fixture-model");
        let context = error.context.unwrap();
        assert_eq!(context.used_tokens, Some(100));
        assert_eq!(
            context.window_tokens.map(|value| value.get()),
            Some(200_000)
        );
    }

    running.stop_and_wait().await;
    let pool = AccountPool::load().unwrap();
    let running = gateway(pool.clone()).await;
    request(
        &client,
        &running,
        json!([
            {"role":"user","content":"spend allowance"},
            {"role":"assistant","content":"fixture answer"},
            {"role":"user","content":"continue after the fallback"}
        ]),
        false,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let (restored_account, restored_turn) = turns.recv().await.unwrap();
    assert_eq!(restored_account, "Bearer fixture-b");
    assert_eq!(
        restored_turn["thread_id"],
        fallback.as_ref().unwrap().1["thread_id"]
    );
    assert!(
        !restored_turn["text"]
            .as_str()
            .unwrap()
            .contains("spend allowance")
    );

    for _ in 0..2 {
        let limited = request(
            &client,
            &running,
            json!([{"role":"user","content":"all allowances spent"}]),
            false,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(limited.status(), 429);
        assert!(limited.headers().contains_key(reqwest::header::RETRY_AFTER));
    }
    assert_eq!(turns.len(), 2, "cooling accounts were tried again");
    running.stop_and_wait().await;
    usage_task.abort();
    quick_task.abort();

    let resumed = continuation["thread_id"] == original["thread_id"]
        && !continuation["text"]
            .as_str()
            .unwrap()
            .contains("remember this question");
    let failover_ok = quota_account == "Bearer fixture-a"
        && fallback
            .as_ref()
            .is_some_and(|(account, _)| account == "Bearer fixture-b")
        && matches!(primary_health, AccountHealth::CoolingDown { .. })
        && failover.contains("fixture answer")
        && !failover.contains("reached your usage limit");
    assert_eq!(
        (
            resumed,
            quote.contains("is an example."),
            failover_ok,
            stop == Some(interrupted["thread_id"].clone()),
        ),
        (true, true, true, true),
        "respectively: resume, quote, failover, stop"
    );
}
