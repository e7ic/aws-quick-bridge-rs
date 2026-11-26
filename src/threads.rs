//! Persisted Quick thread positions, matched against the client's conversation.

use crate::config::write_secure_file;
use crate::messages::{argument_string, content_text, role_of};
use crate::toolbridge::ToolCallReply;
use chrono::Utc;
use http::HeaderMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

const MAX_THREADS: usize = 300;
const THREAD_TTL_MS: i64 = 24 * 3600 * 1000;
const ANCHOR_MESSAGES: usize = 3;

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

fn combine(keys: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut hash = Sha256::new();
    for key in keys {
        let bytes = key.as_ref().as_bytes();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    hex::encode(hash.finalize())[..24].to_string()
}

fn tool_key(id: &str, name: &str, args: &str) -> String {
    let normalized = serde_json::from_str::<Value>(args).map(|v| v.to_string());
    combine([id, name, normalized.as_deref().unwrap_or(args)])
}

fn reply_key(text: &str, tools: impl Iterator<Item = String>) -> String {
    combine(std::iter::once(digest(text.trim())).chain(tools))
}

fn message_key(message: &Value, role: &str) -> String {
    let content = message.get("content");
    let text = content_text(content, None);
    if role == "assistant" {
        let tools = message["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tool| {
                tool_key(
                    tool["id"].as_str().unwrap_or(""),
                    tool["function"]["name"].as_str().unwrap_or(""),
                    &argument_string(tool["function"].get("arguments")),
                )
            });
        return reply_key(&text, tools);
    }
    // Include attachment identity in the fingerprint. A rewritten image often
    // has the same display text as the original one.
    let media: Vec<_> = content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| {
            !part.is_string()
                && !matches!(
                    part["type"].as_str(),
                    Some("text" | "input_text" | "output_text")
                )
        })
        .map(|part| digest(&part.to_string()))
        .collect();
    combine(
        [
            role,
            message["tool_call_id"].as_str().unwrap_or(""),
            message["name"].as_str().unwrap_or(""),
            text.trim(),
        ]
        .into_iter()
        .chain(media.iter().map(String::as_str)),
    )
}

#[derive(Debug)]
struct Continuation {
    from: usize,
    prefix: String,
    reply: String,
    tail: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ThreadHistory {
    scope: String,
    prefix: String,
    tail: Option<String>,
    message_count: usize,
    has_session: bool,
    continuation: Option<Continuation>,
}

impl ThreadHistory {
    pub(crate) fn new(session: &str, model: &str, system: &str, messages: &[Value]) -> Self {
        let scope = combine([session, model, system]);
        let mut prefix = String::new();
        let mut keys = VecDeque::with_capacity(ANCHOR_MESSAGES);
        let mut continuation = None;
        for (i, message) in messages.iter().enumerate() {
            let role = role_of(message);
            let key = message_key(message, &role);
            prefix = combine([&prefix, &key]);
            if role == "assistant" {
                continuation = Some(Continuation {
                    from: i + 1,
                    prefix: prefix.clone(),
                    reply: key.clone(),
                    tail: tail_key(&keys),
                });
            } else if !matches!(role.as_str(), "user" | "tool" | "function") {
                continuation = None;
            }
            if keys.len() == ANCHOR_MESSAGES {
                keys.pop_front();
            }
            keys.push_back(key);
        }
        Self {
            scope,
            prefix,
            tail: tail_key(&keys),
            message_count: messages.len(),
            has_session: !session.is_empty(),
            continuation: continuation.filter(|c| c.from < messages.len()),
        }
    }
}

fn tail_key(keys: &VecDeque<String>) -> Option<String> {
    (keys.len() == ANCHOR_MESSAGES).then(|| combine(keys))
}

#[derive(Debug, Serialize, Deserialize)]
struct ThreadEntry {
    id: String,
    account: String,
    scope: String,
    custom: String,
    prefix: String,
    reply: String,
    tail: Option<String>,
    message_count: usize,
    updated_at: i64,
    #[serde(skip)]
    busy: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ThreadMatch {
    pub(crate) id: String,
    pub(crate) custom: String,
    pub(crate) from: usize,
}

#[derive(Debug, Default)]
pub struct ThreadStore {
    threads: VecDeque<ThreadEntry>,
    path: Option<PathBuf>,
}

impl ThreadStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn load(path: PathBuf) -> Self {
        let mut threads: VecDeque<ThreadEntry> = std::fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        let now = Utc::now().timestamp_millis();
        threads.retain(|t| (0..THREAD_TTL_MS).contains(&(now.saturating_sub(t.updated_at))));
        while threads.len() > MAX_THREADS {
            threads.pop_front();
        }
        Self {
            threads,
            path: Some(path),
        }
    }

    fn best_match(&self, history: &ThreadHistory, account: Option<&str>) -> Option<usize> {
        let c = history.continuation.as_ref()?;
        let now = Utc::now().timestamp_millis();
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                !t.busy
                    && t.scope == history.scope
                    && account.is_none_or(|account| t.account == account)
                    && (0..THREAD_TTL_MS).contains(&now.saturating_sub(t.updated_at))
            })
            .filter_map(|(i, t)| {
                if t.prefix == c.prefix {
                    Some((2, i))
                } else if history.has_session
                    && c.from < t.message_count
                    && t.reply == c.reply
                    && t.tail.is_some()
                    && t.tail == c.tail
                {
                    Some((1, i))
                } else {
                    None
                }
            })
            .max()
            .map(|(_, i)| i)
    }

    pub(crate) fn preferred_account(&self, history: &ThreadHistory) -> Option<&str> {
        self.best_match(history, None)
            .map(|i| self.threads[i].account.as_str())
    }

    pub(crate) fn match_thread(
        &mut self,
        account: &str,
        history: &ThreadHistory,
    ) -> Option<ThreadMatch> {
        let index = self.best_match(history, Some(account))?;
        let entry = &mut self.threads[index];
        entry.busy = true;
        Some(ThreadMatch {
            id: entry.id.clone(),
            custom: entry.custom.clone(),
            from: history.continuation.as_ref()?.from,
        })
    }

    pub(crate) async fn remember(
        store: Arc<Mutex<Self>>,
        history: &ThreadHistory,
        id: String,
        account: String,
        custom: String,
        text: &str,
        tools: &[ToolCallReply],
    ) {
        let reply = reply_key(
            text,
            tools.iter().map(|c| tool_key(&c.id, &c.name, &c.arguments)),
        );
        let entry = ThreadEntry {
            id,
            account,
            scope: history.scope.clone(),
            custom,
            prefix: combine([&history.prefix, &reply]),
            reply,
            tail: history.tail.clone(),
            message_count: history.message_count + 1,
            updated_at: Utc::now().timestamp_millis(),
            busy: false,
        };
        let _ = tokio::task::spawn_blocking(move || {
            let mut store = store.lock();
            store.threads.retain(|t| {
                t.id != entry.id && entry.updated_at.saturating_sub(t.updated_at) < THREAD_TTL_MS
            });
            while store.threads.len() >= MAX_THREADS {
                store.threads.pop_front();
            }
            store.threads.push_back(entry);
            store.persist();
        })
        .await;
    }

    pub(crate) fn release(&mut self, id: &str) {
        if let Some(entry) = self.threads.iter_mut().find(|t| t.id == id) {
            entry.busy = false;
        }
    }

    pub(crate) async fn discard(store: Arc<Mutex<Self>>, id: String) {
        let _ = tokio::task::spawn_blocking(move || {
            let mut store = store.lock();
            store.threads.retain(|t| t.id != id);
            store.persist();
        })
        .await;
    }

    fn persist(&self) {
        if let Some(path) = &self.path {
            if let Ok(data) = serde_json::to_vec(&self.threads) {
                // A write failure only loses restart persistence. Live matching
                // continues from the entries held in memory.
                let _ = write_secure_file(path, &data);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn persisted_thread_matches_an_append_and_a_shortened_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads-v1.json");
        let store = Arc::new(Mutex::new(ThreadStore::load(path.clone())));
        let original = [
            json!({"role":"user","content":"original private question"}),
            json!({"role":"assistant","content":"old answer"}),
            json!({"role":"user","content":"recent question"}),
            json!({"role":"assistant","content":"recent answer"}),
            json!({"role":"user","content":"latest question"}),
        ];
        let history = ThreadHistory::new("private-session", "smart", "system", &original);
        ThreadStore::remember(
            store,
            &history,
            "quick-thread".into(),
            "account-digest".into(),
            "instruction-digest".into(),
            "latest answer",
            &[],
        )
        .await;

        let data = std::fs::read_to_string(&path).unwrap();
        assert!(!data.contains("private-session"));
        assert!(!data.contains("original private question"));
        assert!(!data.contains("latest answer"));
        assert!(data.len() < 600);

        let mut restored = ThreadStore::load(path);
        let mut follow_up = original.to_vec();
        follow_up.extend([
            json!({"role":"assistant","content":"latest answer"}),
            json!({"role":"user","content":"follow up"}),
        ]);
        let query = |mode: &str, system: &str, messages: &[Value]| {
            ThreadHistory::new("private-session", mode, system, messages)
        };
        let full = query("smart", "system", &follow_up);
        assert_eq!(restored.preferred_account(&full), Some("account-digest"));
        assert!(restored.match_thread("different-account", &full).is_none());
        let matched = restored.match_thread("account-digest", &full).unwrap();
        assert_eq!(matched.from, 6);
        assert!(restored.match_thread("account-digest", &full).is_none());
        restored.release(&matched.id);

        let compacted = query("smart", "system", &follow_up[2..]);
        let matched = restored.match_thread("account-digest", &compacted).unwrap();
        assert_eq!(matched.id, "quick-thread");
        assert_eq!(matched.from, 4);
        restored.release(&matched.id);

        for history in [
            query("fast", "system", &follow_up),
            query("smart", "edited system", &follow_up),
            ThreadHistory::new("other-session", "smart", "system", &follow_up),
        ] {
            assert!(restored.preferred_account(&history).is_none());
        }
        follow_up[0]["content"] = json!("edited earlier question");
        assert!(
            restored
                .preferred_account(&query("smart", "system", &follow_up))
                .is_none()
        );
        follow_up[5]["content"] = json!("rewritten reply");
        assert!(
            restored
                .preferred_account(&query("smart", "system", &follow_up[2..]))
                .is_none()
        );
    }
}
