#[path = "support/quick.rs"]
mod quick;

use aws_quick_bridge_rs::config::{AccountsFile, BridgeConfig, save_accounts, save_config};
use aws_quick_bridge_rs::gateway::start_server;
use aws_quick_bridge_rs::pool::{AccountHealth, AccountPool};
use axum::{Json, Router, http::StatusCode, routing::post};
use quick::{account, complete, receive, send};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::handshake::server::Request};

async fn answer(socket: &mut WebSocketStream<TcpStream>) {
    while let Some(event) = receive(socket).await {
        if event["type"] == "user_message" {
            send(socket, "text_delta", json!({"text":"Recovered"})).await;
            complete(socket).await;
            return;
        }
    }
    panic!("expected a Quick turn");
}

#[tokio::test]
#[expect(
    clippy::result_large_err,
    reason = "Tungstenite defines the handshake callback response type"
)]
async fn gateway_can_try_a_ninth_account_and_skips_revoked_credentials() {
    let dir = tempfile::tempdir().unwrap();
    // This integration binary owns its isolated config and credentials.
    unsafe { std::env::set_var("AWS_QUICK_BRIDGE_HOME", dir.path()) };
    let quick = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let quick_port = quick.local_addr().unwrap().port();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let request = |addr: SocketAddr| {
        client
            .post(format!("http://{addr}/v1/chat/completions"))
            .bearer_auth("fixture-key")
            .json(&json!({"model":"smart","messages":[{"role":"user","content":"Hello"}]}))
    };
    save_config(&BridgeConfig {
        api_key: "fixture-key".into(),
        options: serde_json::from_value(json!({"proxy":"direct"})).unwrap(),
        ..BridgeConfig::default()
    })
    .unwrap();
    let first_pool: Vec<_> = (1..=9)
        .map(|index| account(&format!("account-{index}"), quick_port, 1))
        .collect();
    save_accounts(&AccountsFile {
        primary_key: first_pool[0].key(),
        accounts: first_pool,
    })
    .unwrap();
    let pool = AccountPool::load().unwrap();
    assert_eq!(pool.settings().max_concurrency, 2);
    let gateway = start_server(pool, "127.0.0.1", 0).await.unwrap();
    let upstream = async {
        let mut seen = HashSet::new();
        for attempt in 1..=9 {
            let (stream, _) = quick.accept().await.unwrap();
            let upgraded =
                tokio_tungstenite::accept_hdr_async(stream, |req: &Request, response| {
                    let auth = req.headers()["authorization"].to_str().unwrap();
                    assert!(
                        seen.insert(auth.to_owned()),
                        "each account may be attempted once"
                    );
                    if attempt < 9 {
                        Err(axum::http::Response::builder()
                            .status(StatusCode::FORBIDDEN)
                            .body(Some("Fixture refuses this account".to_owned()))
                            .unwrap())
                    } else {
                        Ok(response)
                    }
                })
                .await;
            if attempt == 9 {
                answer(&mut upgraded.unwrap()).await;
            } else {
                assert!(upgraded.is_err());
            }
        }
    };
    let client_request = async {
        let result: Value = request(gateway.addr)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["choices"][0]["message"]["content"], "Recovered");
    };
    tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(client_request, upstream)
    })
    .await
    .unwrap();
    gateway.stop_and_wait().await;

    let oidc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_endpoint = format!("http://{}/token", oidc.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = calls.clone();
    let oidc_task = tokio::spawn(async move {
        let app = Router::new().route(
            "/token",
            post(move || {
                attempts.fetch_add(1, Ordering::Relaxed);
                async {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error":"invalid_grant"})),
                    )
                }
            }),
        );
        axum::serve(oidc, app).await.unwrap();
    });
    let mut revoked = account("revoked", quick_port, 1);
    revoked.expires = chrono::Utc::now().timestamp_millis() - 1;
    revoked.token_endpoint = token_endpoint;
    revoked.refresh = "fixture-revoked-refresh".into();
    let revoked_key = revoked.key();
    let backup = account("available", quick_port, 1);
    save_accounts(&AccountsFile {
        primary_key: revoked_key.clone(),
        accounts: vec![revoked, backup],
    })
    .unwrap();
    let pool = AccountPool::load().unwrap();
    let (first, queued) = tokio::join!(
        pool.ensure_fresh(&revoked_key, false),
        pool.ensure_fresh(&revoked_key, false),
    );
    assert!(first.is_err_and(|error| error.is_expired()));
    assert!(queued.is_err_and(|error| error.is_expired()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(matches!(
        pool.snapshot().accounts[0].health,
        AccountHealth::Expired { .. }
    ));

    let gateway = start_server(pool, "127.0.0.1", 0).await.unwrap();
    let upstream = async {
        let (stream, _) = quick.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, response| {
            assert_eq!(req.headers()["authorization"], "Bearer fixture-available");
            Ok(response)
        })
        .await
        .unwrap();
        answer(&mut socket).await;
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(request(gateway.addr).send(), upstream)
    })
    .await
    .unwrap();
    assert!(response.unwrap().status().is_success());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    gateway.stop_and_wait().await;
    oidc_task.abort();
}
