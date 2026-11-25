//! 服务端多轮对话线程复用缓存（按消息前缀摘要匹配）。

use crate::messages::{content_text, role_of};
use http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_THREADS: usize = 300;
const TTL: Duration = Duration::from_secs(6 * 3600);

pub const CONVERSATION_HEADERS: &[&str] = &[
    "x-aws-quick-conversation",
    "x-magpie-session",
    "session_id",
    "session-id",
    "x-session-id",
    "x-session-affinity",
    "x-thread-id",
    "x-conversation-id",
    "conversation_id",
];

pub fn conversation_of(headers: &HeaderMap) -> String {
    for name in CONVERSATION_HEADERS {
        if let Some(val) = headers.get(*name).and_then(|v| v.to_str().ok()) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(256).collect();
            }
        }
    }
    String::new()
}

pub fn digest(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    hex::encode(hash)[..24].to_string()
}

pub fn message_key(m: &Value) -> String {
    let role = role_of(m);
    if role == "assistant" {
        return "assistant".to_string();
    }
    let tc_id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
    let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let content = content_text(m.get("content"), None);
    format!("{role}:{}", digest(&format!("{tc_id}\n{name}\n{content}")))
}

#[derive(Debug, Clone)]
pub struct ThreadEntry {
    pub id: String,
    pub account: String,
    pub session: String,
    pub custom: String,
    pub keys: Vec<String>,
    pub busy: bool,
    pub at: Instant,
}

#[derive(Debug, Default)]
pub struct ThreadStore {
    threads: VecDeque<ThreadEntry>,
}

impl ThreadStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn match_thread(
        &mut self,
        account: &str,
        session: &str,
        keys: &[String],
    ) -> Option<ThreadEntry> {
        let now = Instant::now();
        self.threads.retain(|t| now.duration_since(t.at) <= TTL);

        let mut best_idx: Option<usize> = None;
        for (idx, t) in self.threads.iter().enumerate() {
            if t.busy || t.account != account || t.session != session {
                continue;
            }
            if t.keys.len() >= keys.len() {
                continue;
            }
            if let Some(bi) = best_idx {
                if t.keys.len() <= self.threads[bi].keys.len() {
                    continue;
                }
            }
            if t.keys.iter().enumerate().all(|(i, k)| k == &keys[i]) {
                best_idx = Some(idx);
            }
        }

        let bi = best_idx?;
        if keys[self.threads[bi].keys.len()..]
            .iter()
            .any(|k| k == "assistant")
        {
            return None;
        }
        self.threads[bi].busy = true;
        Some(self.threads[bi].clone())
    }

    pub fn commit(&mut self, id: String, account: String, session: String, custom: String, keys: &[String]) {
        self.threads.retain(|t| t.id != id);
        while self.threads.len() >= MAX_THREADS {
            self.threads.pop_front();
        }
        let mut stored = keys.to_vec();
        stored.push("assistant".to_string());
        self.threads.push_back(ThreadEntry {
            id,
            account,
            session,
            custom,
            keys: stored,
            busy: false,
            at: Instant::now(),
        });
    }

    pub fn drop_thread(&mut self, id: &str) {
        self.threads.retain(|t| t.id != id);
    }
}
