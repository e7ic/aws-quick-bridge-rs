//! 网关路由、多协议适配、多账号池故障切换与 CC Switch 导入链接集成测试。

use aws_quick_bridge_rs::config::{AccountRecord, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::gateway::ccswitch::{CLAUDE_DESKTOP_APP, endpoint_for, import_link};
use aws_quick_bridge_rs::gateway::start_server;
use aws_quick_bridge_rs::pool::{AccountHealth, AccountPool};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

fn make_account(id: &str) -> AccountRecord {
    AccountRecord {
        auth_type: "oauth".into(),
        access: format!("tok-{id}"),
        refresh: format!("ref-{id}"),
        expires: chrono::Utc::now().timestamp_millis() + 3_600_000,
        id_token: String::new(),
        account_id: id.into(),
        email: id.into(),
        display_name: id.into(),
        account_name: String::new(),
        actor_arn: String::new(),
        aws_account_id: "123456789012".into(),
        stage: "prod".into(),
        region: "us-east-1".into(),
        tenant_url: "https://example.dp.appintegrations.us-east-1.prod.plato.ai.aws.dev".into(),
        ws_host: "example.dp.appintegrations.us-east-1.prod.plato.ai.aws.dev".into(),
        client_id: "cid".into(),
        client_secret: "csec".into(),
        issuer_url: String::new(),
        idc_region: "us-east-1".into(),
        token_endpoint: String::new(),
        redirect_uri: String::new(),
        device_id: "11111111-1111-4111-8111-111111111111".into(),
        enabled: true,
    }
}

#[tokio::test]
async fn test_gateway_http_routes_and_pool_failover() {
    let dir = tempfile::tempdir().unwrap();
    // 安全性说明：独立集成测试进程内设置临时主目录，无并发竞争。
    unsafe {
        std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path());
    }
    let cfg = BridgeConfig {
        api_key: "sk-quick-test-key-123".into(),
        port: 0,
        host: "127.0.0.1".into(),
        options: serde_json::Map::new(),
    };
    save_config(&cfg).unwrap();
    let acc1 = make_account("a@example.com");
    let acc2 = make_account("b@example.com");
    save_accounts(&aws_quick_bridge_rs::config::AccountsFile {
        primary_key: acc1.key(),
        accounts: vec![acc1.clone(), acc2.clone()],
    })
    .unwrap();

    let pool = AccountPool::load().unwrap();

    // 验证多账号冷却与自动故障切换
    let picked1 = pool.pick_account("conv-1", &HashSet::new()).unwrap();
    assert_eq!(picked1.email, "a@example.com");
    pool.mark_cooldown(&acc1.key(), Some(300), "quota spent");
    let picked2 = pool.pick_account("conv-1", &HashSet::new()).unwrap();
    assert_eq!(picked2.email, "b@example.com");
    let snap = pool.snapshot();
    assert!(matches!(
        snap.accounts[0].health,
        AccountHealth::CoolingDown { .. }
    ));

    // 在随机端口启动本地测试网关
    let srv = start_server(pool.clone(), "127.0.0.1", 0).await.unwrap();
    let base = format!("http://{}", srv.addr);
    let client = reqwest::Client::new();

    // 1. /health 健康检查（无需鉴权）
    let health: Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["name"], "aws-quick-bridge-rs");

    // 2. 未携带密钥请求应返回 401
    let unauth = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);

    // 3. 携带 API Key 请求 /v1/models
    let models_resp = client
        .get(format!("{base}/v1/models"))
        .header("x-api-key", "sk-quick-test-key-123")
        .send()
        .await
        .unwrap();
    assert_eq!(models_resp.status().as_u16(), 200);
    let models_json: Value = models_resp.json().await.unwrap();
    assert_eq!(models_json["object"], "list");
    assert!(models_json["data"].as_array().unwrap().len() >= 4);

    // 4. Anthropic count_tokens 估算接口
    let cnt_resp = client
        .post(format!("{base}/v1/messages/count_tokens"))
        .header("x-api-key", "sk-quick-test-key-123")
        .json(&json!({
            "model": "smart[1m]",
            "messages": [{"role": "user", "content": "hello world from rust"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(cnt_resp.status().as_u16(), 200);
    let cnt_json: Value = cnt_resp.json().await.unwrap();
    assert!(cnt_json["input_tokens"].as_u64().unwrap() > 0);

    // 5. Responses 接口传入不支持的 previous_response_id 应返回 400
    let bad_resp = client
        .post(format!("{base}/v1/responses"))
        .header("authorization", "Bearer sk-quick-test-key-123")
        .json(&json!({
            "model": "smart",
            "previous_response_id": "resp_123",
            "input": "hi"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_resp.status().as_u16(), 400);

    let desktop_base = endpoint_for(CLAUDE_DESKTOP_APP, &base);
    let desktop_models = format!("{desktop_base}/v1/models");
    let unauth = client.get(&desktop_models).send().await.unwrap();
    assert_eq!(unauth.status(), 401);
    let models: Value = client
        .get(&desktop_models)
        .bearer_auth(&cfg.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed = models["data"].as_array().unwrap();
    assert_eq!(listed.len(), 3);
    for (model, (id, display_name, tier, versioned_id, legacy_id)) in listed.iter().zip([
        (
            "claude-sonnet-bridge",
            "Quick Balanced",
            "sonnet",
            "claude-sonnet-5-5",
            "claude-sonnet-quick-balanced",
        ),
        (
            "claude-opus-bridge",
            "Quick Smart",
            "opus",
            "claude-opus-5",
            "claude-opus-quick-smart",
        ),
        (
            "claude-haiku-bridge",
            "Quick Fast",
            "haiku",
            "claude-haiku-4-5-20251001",
            "claude-haiku-quick-fast",
        ),
    ]) {
        assert_eq!(model["id"], id);
        assert_eq!(model["display_name"], display_name);
        assert_eq!(model["anthropic_family_tier"], tier);
        assert_eq!(model["is_family_default"], true);

        for requested in [id, tier, versioned_id, legacy_id] {
            let count: Value = client
                .post(format!("{desktop_base}/v1/messages/count_tokens"))
                .bearer_auth(&cfg.api_key)
                .json(&json!({"model": requested, "messages": [{"role": "user", "content": "hello"}]}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(count["input_tokens"].as_u64().unwrap() > 0);
        }
    }
    assert_eq!(models["first_id"], listed[0]["id"]);
    assert_eq!(models["last_id"], listed[2]["id"]);

    let bad_model = client
        .post(format!("{desktop_base}/v1/messages"))
        .bearer_auth(&cfg.api_key)
        .json(&json!({"model": "unknown"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_model.status(), 404);
    let error: Value = bad_model.json().await.unwrap();
    assert_eq!(error["error"]["type"], "not_found_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("model")
    );

    let port = srv.addr.port();
    srv.stop_and_wait().await;
    let restarted = start_server(pool, "127.0.0.1", port).await.unwrap();
    restarted.stop_and_wait().await;
}

#[test]
fn test_ccswitch_import_link() {
    for (app, api_format) in [
        ("claude", "anthropic"),
        ("codex", "openai_responses"),
        ("opencode", "openai_chat"),
    ] {
        let link = import_link(
            app,
            "http://127.0.0.1:18765",
            "sk-quick-test-key-123",
            "smart[1m]",
            "Amazon Quick",
        )
        .unwrap();
        assert!(link.starts_with("ccswitch://v1/import?"));
        assert!(link.contains("smart%5B1m%5D"));

        let parsed = url::Url::parse(&link).unwrap();
        let fields: HashMap<_, _> = parsed.query_pairs().collect();
        assert_eq!(
            fields.get("apiFormat").map(|value| value.as_ref()),
            Some(api_format),
        );
        assert_eq!(fields.get("icon").map(|value| value.as_ref()), Some("aws"));
        assert!(!fields.contains_key("homepage"));
        assert!(!fields.contains_key("iconUrl"));
    }
}

#[test]
fn test_ccswitch_desktop_import_uses_direct_model_discovery() {
    let link = import_link(
        CLAUDE_DESKTOP_APP,
        "http://127.0.0.1:18765/",
        "test-key+&",
        "smart[1m]",
        "Amazon Quick",
    )
    .unwrap();
    let url = url::Url::parse(&link).unwrap();
    assert_eq!(url.scheme(), "ccswitch");
    assert_eq!(url.host_str(), Some("v1"));
    assert_eq!(url.path(), "/import");
    let fields: HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(fields["resource"], "provider");
    assert_eq!(fields["app"], "claude-desktop");
    assert_eq!(fields["name"], "Amazon Quick");
    assert_eq!(fields["apiFormat"], "anthropic");
    assert_eq!(fields["endpoint"], "http://127.0.0.1:18765/claude-desktop");
    assert_eq!(fields["apiKey"], "test-key+&");
    assert_eq!(fields["enabled"], "true");
    assert_eq!(fields["icon"], "aws");
    for key in [
        "model",
        "sonnetModel",
        "opusModel",
        "haikuModel",
        "homepage",
        "iconUrl",
    ] {
        assert!(!fields.contains_key(key));
    }
}
