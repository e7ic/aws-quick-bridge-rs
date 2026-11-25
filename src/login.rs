//! Amazon Quick 浏览器登录本地 OAuth 回调服务。

use crate::config::{AccountRecord, Settings};
use crate::oauth::LoginFlow;
use anyhow::{Result, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_page(ok: bool, title: &str, detail: &str) -> Html<String> {
    let color = if ok { "#1a1a1a" } else { "#8a1c1c" };
    Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title>\
        <style>body{{font-family:system-ui,sans-serif;max-width:36rem;margin:4rem auto;padding:0 1rem;color:{color}}}</style>\
        </head><body><h2>{t}</h2><p>{d}</p></body></html>",
        t = escape_html(title),
        d = escape_html(detail),
    ))
}

struct LoginState {
    flow: Mutex<LoginFlow>,
    cfg: Settings,
    busy: Mutex<bool>,
    done: Mutex<Option<oneshot::Sender<Result<AccountRecord>>>>,
}

async fn handle_start(State(st): State<Arc<LoginState>>) -> Response {
    let url = {
        let flow = st.flow.lock().await;
        flow.login_url(&st.cfg)
    };
    (
        StatusCode::FOUND,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::LOCATION, url.as_str()),
        ],
    )
        .into_response()
}

async fn handle_callback(
    State(st): State<Arc<LoginState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if st.done.lock().await.is_none() {
        return (
            StatusCode::GONE,
            [(header::CACHE_CONTROL, "no-store")],
            render_page(false, "Sign-in is over", "Start the sign-in again."),
        )
            .into_response();
    }
    if params.get("nonce").map(|s| s.is_empty()).unwrap_or(true) {
        return (
            StatusCode::NOT_FOUND,
            render_page(false, "Not found", "Missing nonce parameter."),
        )
            .into_response();
    }

    let mut flow = st.flow.lock().await;
    match flow.process_discovery(&params, &st.cfg).await {
        Ok(next) => {
            let ua = headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !ua.starts_with("Mozilla/") {
                eprintln!("Amazon Quick sign-in, next page to open: {next}");
            }
            (
                StatusCode::FOUND,
                [
                    (header::CACHE_CONTROL, "no-store"),
                    (header::LOCATION, next.as_str()),
                ],
            )
                .into_response()
        }
        Err(e) => {
            let status_code = StatusCode::from_u16(e.step_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let msg = e.to_string();
            if status_code != StatusCode::BAD_REQUEST || params.contains_key("error") {
                if let Some(tx) = st.done.lock().await.take() {
                    let _ = tx.send(Err(anyhow::anyhow!("{msg}")));
                }
            }
            (
                status_code,
                [(header::CACHE_CONTROL, "no-store")],
                render_page(false, "Sign-in failed", &msg),
            )
                .into_response()
        }
    }
}

async fn handle_root(
    State(st): State<Arc<LoginState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if st.done.lock().await.is_none() {
        return (
            StatusCode::GONE,
            [(header::CACHE_CONTROL, "no-store")],
            render_page(false, "Sign-in is over", "Start the sign-in again."),
        )
            .into_response();
    }
    let has_code_or_err = params.contains_key("code") || params.contains_key("error");
    if !has_code_or_err || !params.contains_key("state") {
        return (
            StatusCode::NOT_FOUND,
            render_page(false, "Not found", "This address is not part of the sign-in."),
        )
            .into_response();
    }

    {
        let mut busy = st.busy.lock().await;
        if *busy {
            return (
                StatusCode::CONFLICT,
                render_page(
                    false,
                    "Sign-in is finishing",
                    "This address is already being used to finish the sign-in.",
                ),
            )
                .into_response();
        }
        *busy = true;
    }

    let res = {
        let mut flow = st.flow.lock().await;
        flow.complete(&params, &st.cfg).await
    };

    match res {
        Ok(mut record) => {
            if record.refresh.is_empty() {
                record.refresh = "none".to_string();
            }
            let who = if record.account_id.is_empty() {
                "Your account".to_string()
            } else {
                record.account_id.clone()
            };
            if let Some(tx) = st.done.lock().await.take() {
                let _ = tx.send(Ok(record));
            }
            (
                StatusCode::OK,
                [(header::CACHE_CONTROL, "no-store")],
                render_page(
                    true,
                    "Login complete",
                    &format!("{who} is signed in. You can close this tab."),
                ),
            )
                .into_response()
        }
        Err(e) => {
            *st.busy.lock().await = false;
            let status_code = StatusCode::from_u16(e.step_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let msg = e.to_string();
            if status_code != StatusCode::BAD_REQUEST || params.contains_key("error") {
                if let Some(tx) = st.done.lock().await.take() {
                    let _ = tx.send(Err(anyhow::anyhow!("{msg}")));
                }
            }
            (
                status_code,
                [(header::CACHE_CONTROL, "no-store")],
                render_page(false, "Sign-in failed", &msg),
            )
                .into_response()
        }
    }
}

pub struct BrowserSignInHandle {
    pub start_url: String,
    pub login_url: String,
    pub port: u16,
    rx: oneshot::Receiver<Result<AccountRecord>>,
    shutdown: Option<oneshot::Sender<()>>,
    timeout_ms: u64,
}

impl BrowserSignInHandle {
    pub async fn wait(mut self) -> Result<AccountRecord> {
        let timeout = tokio::time::Duration::from_millis(self.timeout_ms.max(5_000));
        let res = match tokio::time::timeout(timeout, &mut self.rx).await {
            Ok(Ok(inner)) => inner,
            Ok(Err(_)) => Err(anyhow::anyhow!("sign-in was cancelled")),
            Err(_) => Err(anyhow::anyhow!("the sign-in timed out")),
        };
        if let Some(tx) = self.shutdown.take() {
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                let _ = tx.send(());
            });
        }
        res
    }

    pub fn cancel(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    /// 取出用于外部主动取消登录的关闭信号发送端。
    pub fn take_shutdown(&mut self) -> Option<oneshot::Sender<()>> {
        self.shutdown.take()
    }
}

pub async fn start_browser_sign_in(cfg: &Settings) -> Result<BrowserSignInHandle> {
    let first = cfg.callback_port;
    let ports: Vec<u16> = if first == 0 {
        vec![0]
    } else {
        vec![
            first,
            first.saturating_add(1),
            first.saturating_add(2),
            first.saturating_add(3),
            0,
        ]
    };

    let mut listener = None;
    for p in ports {
        let addr = SocketAddr::from(([127, 0, 0, 1], p));
        if let Ok(l) = TcpListener::bind(addr).await {
            listener = Some(l);
            break;
        }
    }
    let Some(listener) = listener else {
        bail!(
            "port {first} is busy; close what uses it or set callback_port, then sign in again"
        );
    };
    let port = listener.local_addr()?.port();
    let flow = LoginFlow::new(&cfg.stage, port, &cfg.app_version);
    let login_url = flow.login_url(cfg);

    let (done_tx, done_rx) = oneshot::channel();
    let (shut_tx, shut_rx) = oneshot::channel::<()>();

    let state = Arc::new(LoginState {
        flow: Mutex::new(flow),
        cfg: cfg.clone(),
        busy: Mutex::new(false),
        done: Mutex::new(Some(done_tx)),
    });

    let app = Router::new()
        .route("/start", get(handle_start))
        .route("/callback", get(handle_callback))
        .route("/", get(handle_root))
        .route("/index.html", get(handle_root))
        .with_state(state);

    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shut_rx.await;
            })
            .await;
    });

    let cb = format!("http://localhost:{port}/callback");
    let start_url = format!(
        "http://127.0.0.1:{port}/start?redirect_uri={}",
        urlencoding::encode(&cb)
    );
    Ok(BrowserSignInHandle {
        start_url,
        login_url,
        port,
        rx: done_rx,
        shutdown: Some(shut_tx),
        timeout_ms: cfg.login_timeout_ms,
    })
}
