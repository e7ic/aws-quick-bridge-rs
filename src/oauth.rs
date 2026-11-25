//! Amazon Quick 个人账号原生桌面端 OAuth 登录与令牌刷新（PKCE S256）。

use crate::config::{AccountRecord, Settings, normalize_stage};
use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use rand::Rng;
use regex::Regex;
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use subtle::ConstantTimeEq;
use thiserror::Error;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("{0}")]
    Expired(String),
    #[error("HTTP {status} from {host}: {body}")]
    Http {
        status: u16,
        host: String,
        body: String,
    },
    #[error("{1}")]
    Step(u16, String),
    #[error("{0}")]
    Other(String),
}

impl OAuthError {
    pub fn is_expired(&self) -> bool {
        matches!(self, OAuthError::Expired(_))
    }

    pub fn step_status(&self) -> u16 {
        match self {
            OAuthError::Step(s, _) => *s,
            _ => 500,
        }
    }
}

pub fn is_device_id(val: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
    });
    re.is_match(val)
}

fn social_discovery(stage: &str) -> &'static str {
    match stage {
        "beta" | "integ" | "dev" => "https://devtest.quicksuite.aws.dev/sn",
        _ => "https://quick.aws.com/sn",
    }
}

fn social_data_plane(stage: &str) -> &'static str {
    match stage {
        "beta" | "integ" | "dev" => {
            "https://cell001.dp.appintegrations.us-west-2.beta.plato.ai.aws.dev"
        }
        _ => "https://cell001.dp.appintegrations.us-east-1.prod.plato.ai.aws.dev",
    }
}

fn social_scopes(stage: &str) -> &'static [&'static str] {
    match stage {
        "beta" | "integ" | "dev" => &["quicksight_test:read_write"],
        _ => &["quicksight:read_write"],
    }
}

fn data_plane(stage: &str, cfg: &Settings) -> String {
    if !cfg.data_plane_url.is_empty() {
        cfg.data_plane_url.clone()
    } else {
        social_data_plane(stage).to_string()
    }
}

fn oidc_endpoint(region: &str, cfg: &Settings) -> String {
    if !cfg.oidc_base.is_empty() {
        cfg.oidc_base.clone()
    } else {
        format!("https://oidc.{region}.amazonaws.com")
    }
}

pub fn discovery_url(
    stage: &str,
    nonce: &str,
    port: u16,
    app_version: &str,
    cfg: &Settings,
) -> String {
    let surface_raw = format!("desktop:{nonce}:{port}:{app_version}");
    let surface = urlencoding::encode(&surface_raw);
    if !cfg.discovery_url.is_empty() {
        return format!("{}?qwSurface={surface}", cfg.discovery_url);
    }
    format!("{}?qwSurface={surface}", social_discovery(stage))
}

pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn user_info_base(stage: &str, tenant_url: &str, cfg: &Settings) -> String {
    let tenant = tenant_url.trim().trim_end_matches('/');
    if tenant.contains("dp.appintegrations") {
        tenant.to_string()
    } else {
        data_plane(stage, cfg)
    }
}

fn one_line(v: Option<&Value>) -> String {
    let Some(v) = v else {
        return String::new();
    };
    let s = match v {
        Value::String(s) => s.as_str(),
        Value::Null => return String::new(),
        _ => return v.to_string(),
    };
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_value(raw: &Value, keys: &[&str]) -> String {
    for k in keys {
        let s = one_line(raw.get(*k));
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    pub email: String,
    pub user_name: String,
    pub display_name: String,
    pub user_arn: String,
    pub aws_account_id: String,
    pub account_name: String,
}

pub fn normalize_user_info(raw: &Value) -> UserInfo {
    let src = raw
        .get("userInfo")
        .filter(|v| v.is_object())
        .unwrap_or(raw);
    UserInfo {
        email: first_value(src, &["email", "emailAddress"]),
        user_name: first_value(src, &["userName", "user_name", "username"]),
        display_name: first_value(src, &["displayName", "display_name"]),
        user_arn: first_value(
            src,
            &[
                "userArn",
                "user_arn",
                "quicksightExternalUserArn",
                "quicksight_external_user_arn",
            ],
        ),
        aws_account_id: first_value(src, &["awsAccountId", "aws_account_id", "accountId"]),
        account_name: first_value(src, &["accountName", "account_name"]),
    }
}

pub fn decode_jwt(token: &str) -> Option<serde_json::Map<String, Value>> {
    let part = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(part).ok()?;
    let val: Value = serde_json::from_slice(&bytes).ok()?;
    val.as_object().cloned()
}

#[derive(Debug, Clone, Default)]
pub struct TokenBundle {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_in: i64,
}

fn tokens_from(raw: &Value) -> TokenBundle {
    let expires_in = raw
        .get("expires_in")
        .or_else(|| raw.get("expiresIn"))
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0);
    TokenBundle {
        access_token: first_value(raw, &["access_token", "accessToken"]),
        refresh_token: first_value(raw, &["refresh_token", "refreshToken"]),
        id_token: first_value(raw, &["id_token", "idToken"]),
        expires_in,
    }
}

pub fn build_http_client(cfg: &Settings) -> Client {
    let mut builder = Client::builder().timeout(Duration::from_secs(30));
    let proxy = cfg.proxy.trim();
    if matches!(proxy.to_ascii_lowercase().as_str(), "direct" | "none" | "off") {
        builder = builder.no_proxy();
    } else if !proxy.is_empty() {
        if let Ok(p) = reqwest::Proxy::all(proxy) {
            builder = builder.proxy(p);
        }
    }
    builder.build().unwrap_or_else(|_| Client::new())
}

async fn request_json(
    req: reqwest::RequestBuilder,
    endpoint: &str,
) -> std::result::Result<Value, OAuthError> {
    let host = Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| endpoint.to_string());
    let resp = req
        .send()
        .await
        .map_err(|e| OAuthError::Other(format!("request to {host} failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let trimmed = text.chars().take(1_048_576).collect::<String>();
    if !(200..300).contains(&status) {
        return Err(OAuthError::Http {
            status,
            host,
            body: trimmed.trim().chars().take(300).collect(),
        });
    }
    let val: Value = serde_json::from_str(&trimmed)
        .map_err(|_| OAuthError::Other(format!("invalid JSON from {host}")))?;
    if !val.is_object() {
        return Err(OAuthError::Other(format!("invalid JSON from {host}")));
    }
    Ok(val)
}

async fn post_json(
    client: &Client,
    endpoint: &str,
    payload: &Value,
    bearer: Option<&str>,
) -> std::result::Result<Value, OAuthError> {
    let mut req = client
        .post(endpoint)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(payload);
    if let Some(tok) = bearer {
        req = req.header("authorization", format!("Bearer {tok}"));
    }
    request_json(req, endpoint).await
}

async fn fetch_user_info(
    client: &Client,
    stage: &str,
    tenant_url: &str,
    tokens: &TokenBundle,
    cfg: &Settings,
) -> std::result::Result<UserInfo, OAuthError> {
    if tokens.access_token.is_empty() {
        return Err(OAuthError::Other("token response has no access_token".into()));
    }
    let base = user_info_base(stage, tenant_url, cfg);
    if base.is_empty() {
        return Err(OAuthError::Other("discovery response has no tenant_url".into()));
    }
    let url = format!("{base}/integration/quick-work/user-info");
    let raw = post_json(client, &url, &json!({}), Some(&tokens.access_token)).await?;
    Ok(normalize_user_info(&raw))
}

fn prefer(a: &str, b: &str) -> String {
    if !a.trim().is_empty() {
        a.to_string()
    } else {
        b.to_string()
    }
}

pub fn session_fields(
    base: &AccountRecord,
    tokens: &TokenBundle,
    info: &UserInfo,
    cfg: &Settings,
) -> Result<AccountRecord> {
    let access = tokens.access_token.trim().to_string();
    let id_token = tokens.id_token.trim().to_string();
    if id_token.is_empty() && access.is_empty() {
        return Err(anyhow!("token response has no id_token/access_token"));
    }

    let mut tenant = base.tenant_url.trim().to_string();
    if tenant.is_empty() {
        tenant = data_plane(&base.stage, cfg);
    }
    tenant = tenant.trim_end_matches('/').to_string();
    if !tenant.contains("://") {
        tenant = format!("https://{tenant}");
    }
    let parsed = Url::parse(&tenant)
        .map_err(|_| anyhow!("cannot derive ws_host from tenant_url {:?}", tenant))?;
    let host_port = match parsed.port() {
        Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or("")),
        None => parsed.host_str().unwrap_or("").to_string(),
    };
    if host_port.is_empty() {
        return Err(anyhow!("cannot derive ws_host from tenant_url {:?}", tenant));
    }

    let claims = decode_jwt(&id_token).or_else(|| decode_jwt(&access));
    let claim_str = |k: &str| -> String {
        claims
            .as_ref()
            .and_then(|m| m.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let sub = claim_str("sub");
    let actor_arn = if !info.user_arn.trim().is_empty() {
        info.user_arn.trim().to_string()
    } else if sub.starts_with("arn:") {
        sub
    } else {
        String::new()
    };
    let region = if !base.region.trim().is_empty() {
        base.region.trim().to_string()
    } else {
        let cr = claim_str("region");
        if !cr.is_empty() {
            cr
        } else {
            "us-east-1".to_string()
        }
    };

    let now_ms = Utc::now().timestamp_millis();
    let expires = if tokens.expires_in > 0 {
        now_ms + tokens.expires_in * 1000
    } else if let Some(exp) = claims
        .as_ref()
        .and_then(|m| m.get("exp"))
        .and_then(|v| v.as_i64())
    {
        if exp > 0 { exp * 1000 } else { 0 }
    } else {
        0
    };

    let email = prefer(&info.email, &base.email);
    let display_name = prefer(&info.display_name, &base.display_name);
    let account_name = prefer(&info.account_name, &base.account_name);
    let account_id = [
        email.as_str(),
        info.user_name.as_str(),
        display_name.as_str(),
        actor_arn.as_str(),
        base.account_id.as_str(),
        parsed.host_str().unwrap_or(""),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or("")
    .to_string();

    let claim_acc = claim_str("aws_account_id");
    let aws_acc = if !info.aws_account_id.is_empty() {
        info.aws_account_id.clone()
    } else if !claim_acc.is_empty() {
        claim_acc
    } else {
        base.aws_account_id.clone()
    };

    let ws_host = if parsed.scheme() == "http" {
        format!("ws://{host_port}")
    } else {
        host_port
    };
    let device_id = if is_device_id(&base.device_id) {
        base.device_id.clone()
    } else {
        Uuid::new_v4().to_string()
    };

    Ok(AccountRecord {
        auth_type: "oauth".to_string(),
        access,
        refresh: if !tokens.refresh_token.trim().is_empty() {
            tokens.refresh_token.trim().to_string()
        } else {
            base.refresh.clone()
        },
        expires,
        id_token,
        account_id,
        email,
        display_name,
        account_name,
        actor_arn: prefer(&actor_arn, &base.actor_arn),
        aws_account_id: aws_acc,
        stage: base.stage.clone(),
        region,
        tenant_url: tenant,
        ws_host,
        client_id: base.client_id.clone(),
        client_secret: base.client_secret.clone(),
        issuer_url: base.issuer_url.clone(),
        idc_region: base.idc_region.clone(),
        token_endpoint: base.token_endpoint.clone(),
        redirect_uri: base.redirect_uri.clone(),
        device_id,
        enabled: base.enabled,
    })
}

/// 使用 refresh_token 向 OIDC 端点刷新访问令牌与用户信息。
pub async fn refresh_account(
    auth: &AccountRecord,
    cfg: &Settings,
) -> std::result::Result<AccountRecord, OAuthError> {
    let token = auth.refresh.trim();
    if token.is_empty() || token == "none" {
        return Err(OAuthError::Expired(
            "Amazon Quick: no refresh token is stored; sign in again".into(),
        ));
    }
    let stage = normalize_stage(&auth.stage);
    let client = build_http_client(cfg);

    let ep = if !auth.token_endpoint.is_empty() {
        auth.token_endpoint.clone()
    } else {
        let r = if !auth.idc_region.is_empty() {
            auth.idc_region.as_str()
        } else if !auth.region.is_empty() {
            auth.region.as_str()
        } else {
            "us-east-1"
        };
        format!("{}/token", oidc_endpoint(r, cfg))
    };

    let res = post_json(
        &client,
        &ep,
        &json!({
            "clientId": auth.client_id,
            "clientSecret": auth.client_secret,
            "grantType": "refresh_token",
            "refreshToken": token,
        }),
        None,
    )
    .await;

    static REFUSED_RE: OnceLock<Regex> = OnceLock::new();
    let refused_re = REFUSED_RE.get_or_init(|| {
        Regex::new(r"(?i)invalid_(grant|client)|InvalidGrant|InvalidClient|UnauthorizedClient|unauthorized_client|ExpiredToken|expired").unwrap()
    });

    let raw = match res {
        Ok(v) => v,
        Err(ref e @ OAuthError::Http { status, ref body, .. })
            if status == 401 || (status == 400 && refused_re.is_match(body)) =>
        {
            return Err(OAuthError::Expired(format!(
                "Amazon Quick refused the sign-in; sign in again ({e})"
            )));
        }
        Err(e) => return Err(e),
    };

    let tokens = tokens_from(&raw);
    if tokens.access_token.is_empty() {
        return Err(OAuthError::Other("refresh returned no access_token".into()));
    }
    let info = fetch_user_info(&client, &stage, &auth.tenant_url, &tokens, cfg).await?;
    let mut base = auth.clone();
    base.stage = stage;
    session_fields(&base, &tokens, &info, cfg).map_err(|e| OAuthError::Other(e.to_string()))
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn same_secret(a: &str, b: &str) -> bool {
    !a.is_empty() && a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

fn append_query(endpoint: &str, params: &[(&str, &str)]) -> Result<String> {
    let mut u = Url::parse(endpoint)?;
    {
        let mut pairs = u.query_pairs_mut();
        for (k, v) in params {
            pairs.append_pair(k, v);
        }
    }
    Ok(u.to_string())
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryData {
    pub issuer_url: String,
    pub tenant_url: String,
    pub idc_region: String,
}

#[derive(Debug, Clone)]
pub struct LoginFlow {
    pub stage: String,
    pub port: u16,
    pub app_version: String,
    pub redirect_uri: String,
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub discovery: Option<DiscoveryData>,
    pub client_id: String,
    pub client_secret: String,
    pub authorize_url: String,
}

impl LoginFlow {
    pub fn new(stage: &str, port: u16, app_version: &str) -> Self {
        let stage = normalize_stage(stage);
        let redirect = redirect_uri(port);
        Self {
            stage,
            port,
            app_version: app_version.to_string(),
            redirect_uri: redirect,
            state: random_token(),
            nonce: random_token(),
            verifier: random_token(),
            discovery: None,
            client_id: String::new(),
            client_secret: String::new(),
            authorize_url: String::new(),
        }
    }

    pub fn challenge(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()))
    }

    pub fn login_url(&self, cfg: &Settings) -> String {
        discovery_url(&self.stage, &self.nonce, self.port, &self.app_version, cfg)
    }

    /// 处理发现回调：注册动态 OIDC 客户端并生成授权跳转地址。
    pub async fn process_discovery(
        &mut self,
        query: &HashMap<String, String>,
        cfg: &Settings,
    ) -> std::result::Result<String, OAuthError> {
        let q = |keys: &[&str]| -> String {
            for k in keys {
                if let Some(v) = query.get(*k) {
                    let t = v.trim();
                    if !t.is_empty() {
                        return t.to_string();
                    }
                }
            }
            String::new()
        };

        if !same_secret(query.get("nonce").map(|s| s.as_str()).unwrap_or(""), &self.nonce) {
            return Err(OAuthError::Step(400, "discovery nonce mismatch".into()));
        }
        if let Some(err) = query.get("error").filter(|s| !s.is_empty()) {
            let desc = query.get("error_description").map(|s| s.as_str()).unwrap_or("");
            return Err(OAuthError::Step(
                400,
                format!("login failed: {err} {desc}").trim().to_string(),
            ));
        }
        if !self.authorize_url.is_empty() {
            return Ok(self.authorize_url.clone());
        }

        let issuer_url = q(&["issuerUrl", "issuer_url", "issuer"]);
        if issuer_url.is_empty() {
            return Err(OAuthError::Step(
                400,
                "discovery callback did not provide issuerUrl".into(),
            ));
        }
        let idc_region = {
            let r = q(&["idcRegion", "idc_region"]);
            if !r.is_empty() {
                r
            } else {
                "us-east-1".to_string()
            }
        };
        self.discovery = Some(DiscoveryData {
            issuer_url: issuer_url.clone(),
            tenant_url: q(&["tenantUrl", "tenant_url"]),
            idc_region: idc_region.clone(),
        });

        let challenge = self.challenge();
        let client = build_http_client(cfg);
        let reg_url = format!("{}/client/register", oidc_endpoint(&idc_region, cfg));
        let scopes = social_scopes(&self.stage);
        let registered = post_json(
            &client,
            &reg_url,
            &json!({
                "clientName": "AmazonQuick-Desktop",
                "clientType": "public",
                "scopes": scopes,
                "grantTypes": ["authorization_code", "refresh_token"],
                "redirectUris": [&self.redirect_uri],
                "issuerUrl": issuer_url,
            }),
            None,
        )
        .await
        .map_err(|e| OAuthError::Step(502, format!("client registration failed: {e}")))?;

        self.client_id = first_value(&registered, &["clientId", "client_id"]);
        self.client_secret = first_value(&registered, &["clientSecret", "client_secret"]);
        if self.client_id.is_empty() {
            return Err(OAuthError::Step(
                502,
                "client registration returned no clientId".into(),
            ));
        }
        let scope_str = scopes.join(" ");
        let auth_base = format!("{}/authorize", oidc_endpoint(&idc_region, cfg));
        self.authorize_url = append_query(
            &auth_base,
            &[
                ("response_type", "code"),
                ("client_id", &self.client_id),
                ("redirect_uri", &self.redirect_uri),
                ("scope", &scope_str),
                ("state", &self.state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
        )
        .map_err(|e| OAuthError::Step(502, format!("invalid authorize URL: {e}")))?;
        Ok(self.authorize_url.clone())
    }

    async fn exchange_code(
        &self,
        client: &Client,
        code: &str,
        cfg: &Settings,
    ) -> std::result::Result<TokenBundle, OAuthError> {
        let d = self
            .discovery
            .as_ref()
            .ok_or_else(|| OAuthError::Step(400, "missing discovery".into()))?;
        let ep = format!("{}/token", oidc_endpoint(&d.idc_region, cfg));
        let raw = post_json(
            client,
            &ep,
            &json!({
                "clientId": self.client_id,
                "clientSecret": self.client_secret,
                "grantType": "authorization_code",
                "code": code,
                "redirectUri": self.redirect_uri,
                "codeVerifier": self.verifier,
            }),
            None,
        )
        .await?;
        Ok(tokens_from(&raw))
    }

    /// 校验 OAuth 回调参数、换取令牌并拉取用户资料，返回完整的 `AccountRecord`。
    pub async fn complete(
        &mut self,
        query: &HashMap<String, String>,
        cfg: &Settings,
    ) -> std::result::Result<AccountRecord, OAuthError> {
        if let Some(err) = query.get("error").filter(|s| !s.is_empty()) {
            let desc = query.get("error_description").map(|s| s.as_str()).unwrap_or("");
            return Err(OAuthError::Step(
                400,
                format!("login failed: {err} {desc}").trim().to_string(),
            ));
        }
        if self.authorize_url.is_empty() || self.discovery.is_none() {
            return Err(OAuthError::Step(
                400,
                "the discovery step has not been completed yet".into(),
            ));
        }
        if !same_secret(query.get("state").map(|s| s.as_str()).unwrap_or(""), &self.state) {
            return Err(OAuthError::Step(400, "OAuth state mismatch".into()));
        }
        let code = query
            .get("code")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OAuthError::Step(400, "OAuth callback has no code".into()))?;

        let client = build_http_client(cfg);
        let tokens = self
            .exchange_code(&client, code, cfg)
            .await
            .map_err(|e| OAuthError::Step(502, format!("token exchange failed: {e}")))?;

        let d = self.discovery.clone().unwrap();
        let info = fetch_user_info(&client, &self.stage, &d.tenant_url, &tokens, cfg)
            .await
            .map_err(|e| OAuthError::Step(502, format!("user info failed: {e}")))?;

        let token_endpoint = format!("{}/token", oidc_endpoint(&d.idc_region, cfg));
        let base = AccountRecord {
            auth_type: "oauth".to_string(),
            access: String::new(),
            refresh: String::new(),
            expires: 0,
            id_token: String::new(),
            account_id: String::new(),
            email: String::new(),
            display_name: String::new(),
            account_name: String::new(),
            actor_arn: String::new(),
            aws_account_id: String::new(),
            stage: self.stage.clone(),
            region: d.idc_region.clone(),
            tenant_url: d.tenant_url,
            ws_host: String::new(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            issuer_url: d.issuer_url,
            idc_region: d.idc_region,
            token_endpoint,
            redirect_uri: self.redirect_uri.clone(),
            device_id: String::new(),
            enabled: true,
        };
        session_fields(&base, &tokens, &info, cfg)
            .map_err(|e| OAuthError::Step(502, e.to_string()))
    }
}
