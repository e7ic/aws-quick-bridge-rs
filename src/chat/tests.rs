use super::*;
use axum::{
    Json, Router,
    routing::{get, post},
};
use tokio::net::TcpListener;

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
        &auth,
        &cfg,
        &session,
        None,
        "00000000-0000-4000-8000-000000000002",
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
