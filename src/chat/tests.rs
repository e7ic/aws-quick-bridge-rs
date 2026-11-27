use super::*;
use axum::{
    Json, Router,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[test]
fn thinking_effort_survives_protocol_conversion_and_the_quick_frame() {
    use crate::gateway::{anthropic, responses};
    use axum::http::HeaderMap;

    let auth: AccountRecord = serde_json::from_value(json!({ "access": "fixture" })).unwrap();
    let messages = json!([{ "role": "user", "content": "Solve this." }]);
    let settings =
        |effort| Settings::from_options(json!({ "thinking_effort": effort }).as_object().unwrap());
    let frame_effort = |body: &Value, cfg: &Settings| {
        let input = prepare(body, cfg).unwrap().input;
        let session = UpstreamSession::from_auth(&auth, cfg);
        user_message(&session, cfg, &input)["payload"]
            .get("thinking_effort")
            .cloned()
    };
    let cfg = settings("high");

    for (requested, expected) in [
        ("high", "high"),
        ("max", "max"),
        ("xhigh", "max"),
        ("off", "off"),
        ("none", "off"),
    ] {
        let chat = json!({ "messages": messages, "reasoning_effort": requested });
        let anthropic = anthropic::to_chat(&json!({
            "messages": messages,
            "thinking": { "type": "adaptive", "display": "omitted" },
            "output_config": { "effort": requested },
        }));
        let responses = responses::to_chat(
            &json!({ "input": "Solve this.", "reasoning": { "effort": requested } }),
            &HeaderMap::new(),
        );
        assert!(!anthropic.thinking);
        for (protocol, body) in [
            ("Chat Completions", chat),
            ("Messages", anthropic.chat),
            ("Responses", responses.chat),
        ] {
            assert_eq!(
                frame_effort(&body, &cfg),
                Some(json!(expected)),
                "{protocol} {requested}"
            );
        }
    }

    let disabled = anthropic::to_chat(&json!({
        "messages": messages,
        "thinking": { "type": "disabled" },
        "output_config": { "effort": "max" },
    }));
    assert!(!disabled.thinking);
    assert_eq!(frame_effort(&disabled.chat, &cfg), Some(json!("off")));

    for (budget, expected) in [
        (3_999, "low"),
        (4_000, "medium"),
        (16_000, "high"),
        (31_999, "high"),
        (32_000, "max"),
    ] {
        let legacy = anthropic::to_chat(&json!({
            "messages": messages,
            "thinking": { "type": "enabled", "budget_tokens": budget },
        }));
        assert_eq!(
            frame_effort(&legacy.chat, &cfg),
            Some(json!(expected)),
            "budget {budget}"
        );
    }

    let adaptive = anthropic::to_chat(&json!({
        "messages": messages,
        "thinking": { "type": "adaptive" },
    }));
    let responses = responses::to_chat(&json!({ "input": "Solve this." }), &HeaderMap::new());
    for body in [
        json!({ "messages": messages }),
        adaptive.chat,
        responses.chat,
    ] {
        assert_eq!(frame_effort(&body, &settings("max")), Some(json!("max")));
        assert_eq!(frame_effort(&body, &settings("off")), Some(json!("off")));
        assert_eq!(frame_effort(&body, &settings("")), None);
    }
}

#[tokio::test]
async fn reduced_tool_definitions_upgrade_when_the_full_schema_fits() {
    let mut cfg = Settings::from_options(&serde_json::Map::new());
    cfg.attachments = false;
    cfg.max_frame_bytes = 15_000;
    let auth: AccountRecord = serde_json::from_value(json!({ "access": "fixture" })).unwrap();
    let session = UpstreamSession::from_auth(&auth, &cfg);
    let mut body = json!({
        "model": "smart",
        "messages": [{"role": "user", "content": "Find the document."}],
        "tools": [{"type": "function", "function": {
            "name": "Read",
            "description": "Long description. ".repeat(1_500),
            "parameters": {"type": "object", "properties": {"file": {"type": "string"}}},
        }}],
    });
    let turn = prepare(&body, &cfg).unwrap();
    let initial = compose(
        &turn,
        "smart",
        &auth,
        &cfg,
        &session,
        ComposeContext::initial("thread", None),
    )
    .await
    .unwrap();
    assert!(!initial.prompt_state.tool_anchor.unwrap().full);
    assert!(initial.input.system_prompt.contains("Read"));
    let mut thread = ThreadMatch {
        id: initial.input.thread_id,
        prompt: initial.prompt_state,
        from: 0,
        attachment: None,
    };

    let reused = compose(
        &turn,
        "smart",
        &auth,
        &cfg,
        &session,
        ComposeContext::initial("thread", Some(&thread)),
    )
    .await
    .unwrap();
    assert!(reused.input.system_prompt.is_empty());
    assert_eq!(reused.prompt_state.tool_anchor.unwrap().reused_turns, 1);
    thread.prompt = reused.prompt_state;

    cfg.max_frame_bytes = 63_000;
    let upgraded = compose(
        &turn,
        "smart",
        &auth,
        &cfg,
        &session,
        ComposeContext::initial("thread", Some(&thread)),
    )
    .await
    .unwrap();
    assert!(upgraded.input.system_prompt.contains("\"parameters\""));
    let anchor = upgraded.prompt_state.tool_anchor.unwrap();
    assert!(anchor.full);
    assert_eq!(anchor.reused_turns, 0);
    thread.prompt = upgraded.prompt_state;

    body["tools"][0]["function"]["parameters"]["properties"] =
        json!({"folder": {"type": "string"}});
    let changed = prepare(&body, &cfg).unwrap();
    let replay = compose(
        &changed,
        "smart",
        &auth,
        &cfg,
        &session,
        ComposeContext::initial("thread", Some(&thread)),
    )
    .await
    .unwrap();
    assert!(replay.input.system_prompt.contains("\"folder\""));
}

async fn compose_with_attachment(messages: Value, limit: usize, path: String) -> TurnInput {
    let cfg = Settings::from_options(
        json!({"max_frame_bytes": limit, "proxy": "direct"})
            .as_object()
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth: AccountRecord = serde_json::from_value(json!({
        "access": "fixture-token",
        "tenantUrl": format!("http://{}", listener.local_addr().unwrap()),
    }))
    .unwrap();
    let mock = Router::new()
        .route(
            "/integration/quick-work/upload-attachment",
            post(|| async {
                Json(json!({"attachmentId": "00000000-0000-4000-8000-000000000001"}))
            }),
        )
        .route(
            "/integration/quick-work/attachment-dlp-status",
            get(|| async { Json(json!({"dlpJobStatus": "COMPLETED", "policyAction": "ALLOW"})) }),
        )
        .route(
            "/integration/quick-work/qbs-event",
            post(move || {
                let path = path.clone();
                async move { Json(json!({"path": path})) }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let session = UpstreamSession::from_auth(&auth, &cfg);
    let turn = prepare(&json!({"messages": messages}), &cfg).unwrap();
    let result = compose(
        &turn,
        &turn.model,
        &auth,
        &cfg,
        &session,
        ComposeContext::initial("00000000-0000-4000-8000-000000000002", None),
    )
    .await;
    server.abort();
    let input = result.expect("the complete message should fit").input;
    assert!(frame_bytes(&user_message(&session, &cfg, &input)) <= limit);
    input
}

#[tokio::test]
async fn image_reference_is_budgeted_with_the_system_prompt() {
    const IMAGE: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEgQGAHgzD2gAAAABJRU5ErkJggg==";
    let input = compose_with_attachment(
        json!([
            {"role": "system", "content": "s".repeat(80_000)},
            {"role": "user", "content": [
                {"type": "text", "text": "Describe this image. "},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{IMAGE}")}}
            ]}
        ]),
        63_000,
        format!("/fixture/{}", "p".repeat(134)),
    )
    .await;

    assert_eq!(input.attachments.len(), 1);
    assert_eq!(input.attachments[0].name, "image_1.png");
    assert_eq!(
        input.prompt,
        "Describe this image. [image: attached file image_1.png]"
    );
}

#[tokio::test]
async fn text_attachment_is_budgeted_after_the_history_is_uploaded() {
    let input = compose_with_attachment(
        json!([
            {"role": "system", "content": "s".repeat(12_000)},
            {"role": "user", "content": "h".repeat(16_000)},
            {"role": "assistant", "content": "I read your notes."},
            {"role": "user", "content": "Summarize the history."}
        ]),
        8_000,
        format!("/fixture/{}", "p".repeat(4_200)),
    )
    .await;

    assert_eq!(input.attachments.len(), 1);
    assert_eq!(input.attachments[0].kind, "file");
    assert!(input.prompt.contains(&input.attachments[0].name));
    assert!(input.prompt.ends_with("Summarize the history."));
}

#[tokio::test]
async fn failed_large_tool_upload_does_not_send_a_partial_document() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth = serde_json::from_value(json!({
        "access":"fixture", "tenantUrl":format!("http://{}", listener.local_addr().unwrap())
    }))
    .unwrap();
    let server = tokio::spawn(async move {
        let app = Router::new().fallback(|| async { http::StatusCode::SERVICE_UNAVAILABLE });
        axum::serve(listener, app).await.unwrap();
    });
    let cfg = Settings::from_options(json!({"proxy":"direct"}).as_object().unwrap());
    let turn = prepare(&json!({"messages":[{"role":"tool","tool_call_id":"read-1","content":"文".repeat(31_360)}]}), &cfg).unwrap();
    let result = compose(
        &turn,
        &turn.model,
        &auth,
        &cfg,
        &UpstreamSession::from_auth(&auth, &cfg),
        ComposeContext::initial("thread", None),
    )
    .await;
    server.abort();
    let error = result
        .err()
        .expect("an upload failure must not send truncated tool output");
    assert_eq!(
        failure_from_error(&error, None).code,
        "attachment_upload_failed"
    );
}

async fn run_attached_tool_reply(
    cfg: &Settings,
    input: &TurnInput,
    progress: Vec<Value>,
) -> (Result<crate::upstream::UpstreamResult, UpstreamError>, bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth = serde_json::from_value(json!({
        "access": "fixture-token", "wsHost": format!("ws://{}", listener.local_addr().unwrap())
    }))
    .unwrap();
    let session = UpstreamSession::from_auth(&auth, cfg);
    let expected_attachment = input.attachments[0].content_sha256.clone();
    let mock = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let request = loop {
            let Message::Binary(frame) = socket.next().await.unwrap().unwrap() else {
                continue;
            };
            let envelope: Value = serde_json::from_slice(&frame).unwrap();
            let inner = B64
                .decode(envelope["payload"]["payload"].as_str().unwrap())
                .unwrap();
            let frame: Value = serde_json::from_slice(&inner).unwrap();
            if frame["type"] == "user_message" {
                break frame;
            }
        };
        assert_eq!(
            request["payload"]["attachments"][0]["contentSha256"],
            expected_attachment
        );
        for frame in progress {
            socket.send(Message::text(frame.to_string())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for frame in [
            json!({"type":"text_delta", "payload":{"text":"Finished reading."}}),
            json!({"type":"turn_completed"}),
        ] {
            socket.send(Message::text(frame.to_string())).await.unwrap();
        }
    });
    let (_cancel, cancel_rx) = watch::channel(false);
    let (events, mut responses) = mpsc::unbounded_channel();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        chat(&session, cfg, input, cancel_rx, Some(events)),
    )
    .await
    .expect("fixture finished");
    mock.await.unwrap();
    (
        result,
        matches!(responses.try_recv(), Ok(UpstreamEvent::Reasoning(_))),
    )
}

#[tokio::test]
async fn active_reply_can_finish_after_large_tool_result() {
    let content = "文".repeat(31_360);
    let input = compose_with_attachment(
        json!([{"role":"tool", "name":"Read", "tool_call_id":"read-1", "content":content}]),
        63_000,
        "/fixture/s1.txt".into(),
    )
    .await;
    assert_eq!(input.attachments.len(), 1);
    let expected = format!("Tool result (call_id=read-1, name=Read): {content}");
    assert_eq!(input.attachments[0].size, expected.len());
    assert_eq!(
        input.attachments[0].content_sha256,
        hex::encode(Sha256::digest(expected.as_bytes()))
    );
    assert!(
        input
            .prompt
            .contains("Consult the parts needed for the current task")
    );

    let mut cfg = Settings::from_options(json!({"proxy":"direct"}).as_object().unwrap());
    cfg.request_timeout_ms = 400;
    let explicit = Settings::from_options(
        json!({"proxy":"direct", "request_timeout":"400ms"})
            .as_object()
            .unwrap(),
    );
    let thinking =
        |count, delta: &str| vec![json!({"type":"thinking_delta","payload":{"text":delta}}); count];
    let (fast, _) = run_attached_tool_reply(&cfg, &input, vec![]).await;
    assert_eq!(
        fast.expect("large result should be accepted").text,
        "Finished reading."
    );

    let (active, streamed) = run_attached_tool_reply(&cfg, &input, thinking(6, "Reading. ")).await;
    assert!(streamed);
    assert_eq!(
        active.expect("active reply should finish").text,
        "Finished reading."
    );

    for (settings, chunks, delta) in [
        (&cfg, 6, ""),
        (&cfg, 10, "Still reading. "),
        (&explicit, 6, "Reading. "),
    ] {
        let (result, _) = run_attached_tool_reply(settings, &input, thinking(chunks, delta)).await;
        let error = result.expect_err("reply must respect the applicable deadline");
        assert_eq!(error.kind, UpstreamErrorKind::Timeout);
        assert_eq!(error.status, 504);
    }
}

#[tokio::test]
async fn native_tool_work_extends_stream_budget_only_when_it_advances() {
    let input = compose_with_attachment(
        json!([{"role":"user","content":"x".repeat(65_000)}]),
        63_000,
        "/fixture/input".into(),
    )
    .await;
    let mut cfg = Settings::from_options(
        json!({"proxy":"direct","show_quick_tools":false})
            .as_object()
            .unwrap(),
    );
    cfg.request_timeout_ms = 400;
    let progress = |delta: &str| json!({"type":"tool_progress","payload":{"tool_id":"read","progress":{"input_delta":delta}}});
    let snapshot =
        json!({"type":"tool_arguments","payload":{"tool_id":"read","arguments":{"file":"notes"}}});
    let start = json!({"type":"tool_start","payload":{"tool_id":"read","tool_name":"Read"}});
    let complete = json!({"type":"tool_complete","payload":{"tool_id":"read"}});
    let work = vec![
        start.clone(),
        progress("{"),
        progress("\"file\""),
        progress(":"),
        snapshot.clone(),
        complete.clone(),
    ];
    let (result, _) = run_attached_tool_reply(&cfg, &input, work).await;
    assert_eq!(
        result.expect("upstream tool kept making progress").text,
        "Finished reading."
    );

    for noise in [
        start,
        progress(""),
        snapshot,
        complete,
        json!({"type":"tool_progress","thread_id":"different-thread","payload":{"tool_id":"read","input_delta":"x"}}),
        json!({"type":"keepalive"}),
    ] {
        let (result, _) = run_attached_tool_reply(&cfg, &input, vec![noise.clone(); 6]).await;
        assert_eq!(
            result.expect_err(&format!("repeated noise {noise}")).kind,
            UpstreamErrorKind::Timeout
        );
    }
}
