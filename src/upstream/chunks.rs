use super::{UpstreamError, UpstreamErrorKind};
use serde_json::Value;
use std::collections::HashMap;

const MAX_GROUPS: usize = 32;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARTS: u64 = 4_096;

struct Group {
    parts: Vec<Option<String>>,
    received: usize,
    bytes: usize,
}

pub(super) struct ChunkBuffer {
    groups: HashMap<String, Group>,
    bytes: usize,
    max_groups: usize,
    max_bytes: usize,
}

impl Default for ChunkBuffer {
    fn default() -> Self {
        Self {
            groups: HashMap::new(),
            bytes: 0,
            max_groups: MAX_GROUPS,
            max_bytes: MAX_BYTES,
        }
    }
}

impl ChunkBuffer {
    pub(super) fn push(&mut self, frame: &Value) -> Result<Option<Value>, UpstreamError> {
        let (Some(id), Some(index), Some(total), Some(data)) = (
            frame["chunk_id"].as_str(),
            frame["chunk_index"].as_u64(),
            frame["total_chunks"].as_u64(),
            frame["data"].as_str(),
        ) else {
            return Ok(None);
        };
        if id.is_empty() || !(1..=MAX_PARTS).contains(&total) || index >= total {
            return Ok(None);
        }
        let (index, total) = (index as usize, total as usize);

        if self
            .groups
            .get(id)
            .is_some_and(|group| group.parts.len() != total)
        {
            self.remove(id);
        }
        let (metadata, replaced) = match self.groups.get(id) {
            Some(group) => (0, group.parts[index].as_ref().map_or(0, String::len)),
            None if self.groups.len() >= self.max_groups => {
                return Err(UpstreamError::new(
                    UpstreamErrorKind::ChunkLimit,
                    "Amazon Quick opened too many incomplete response groups",
                    502,
                ));
            }
            None => (id.len() + total * size_of::<Option<String>>(), 0),
        };

        let added = metadata.saturating_add(data.len());
        let bytes = (self.bytes - replaced).saturating_add(added);
        if bytes > self.max_bytes {
            return Err(UpstreamError::new(
                UpstreamErrorKind::ChunkLimit,
                "Amazon Quick's incomplete response frames exceed the memory budget",
                502,
            ));
        }
        let group = self.groups.entry(id.to_owned()).or_insert_with(|| Group {
            parts: vec![None; total],
            received: 0,
            bytes: 0,
        });
        if group.parts[index].replace(data.to_owned()).is_none() {
            group.received += 1;
        }
        group.bytes = group.bytes - replaced + added;
        self.bytes = bytes;

        if group.received < total {
            return Ok(None);
        }
        Ok(self.remove(id).and_then(|group| {
            let joined: String = group.parts.into_iter().flatten().collect();
            serde_json::from_str::<Value>(&joined)
                .ok()
                .filter(Value::is_object)
        }))
    }

    fn remove(&mut self, id: &str) -> Option<Group> {
        let group = self.groups.remove(id)?;
        self.bytes -= group.bytes;
        Some(group)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(id: &str, index: u64, total: u64, data: &str) -> Value {
        json!({"chunk_id": id, "chunk_index": index, "total_chunks": total, "data": data})
    }

    #[test]
    fn limits_incomplete_groups_and_reuses_capacity_when_one_completes() {
        let mut chunks = ChunkBuffer {
            max_groups: 1,
            ..ChunkBuffer::default()
        };
        assert!(
            chunks
                .push(&frame("a", 1, 2, r#""你好"}"#))
                .unwrap()
                .is_none()
        );
        let error = chunks.push(&frame("b", 0, 2, "{")).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::ChunkLimit);
        assert_eq!(
            chunks.push(&frame("a", 0, 2, r#"{"text":"#)).unwrap(),
            Some(json!({"text": "你好"}))
        );
        assert_eq!(chunks.bytes, 0);
        assert!(chunks.push(&frame("b", 0, 2, "{")).unwrap().is_none());
    }

    #[test]
    fn counts_utf8_metadata_and_replacements_before_reserving_memory() {
        let overhead = 1 + 2 * size_of::<Option<String>>();
        let mut chunks = ChunkBuffer {
            max_bytes: overhead + 6,
            ..ChunkBuffer::default()
        };
        assert!(chunks.push(&frame("a", 0, 2, "六字")).unwrap().is_none());
        assert_eq!(chunks.bytes, overhead + 6);
        let error = chunks.push(&frame("a", 0, 2, "三个字")).unwrap_err();
        assert_eq!(error.kind, UpstreamErrorKind::ChunkLimit);
        assert_eq!(chunks.bytes, overhead + 6);
        assert!(chunks.push(&frame("a", 0, 2, "{")).unwrap().is_none());
        assert_eq!(chunks.bytes, overhead + 1);
        assert_eq!(
            chunks.push(&frame("a", 1, 2, "}")).unwrap(),
            Some(json!({}))
        );
        assert_eq!(chunks.bytes, 0);
    }

    #[test]
    fn shares_the_byte_budget_across_groups() {
        let overhead = 1 + 2 * size_of::<Option<String>>();
        let mut chunks = ChunkBuffer {
            max_bytes: 2 * overhead + 3,
            ..ChunkBuffer::default()
        };
        assert!(chunks.push(&frame("a", 0, 2, "{ ")).unwrap().is_none());
        assert!(chunks.push(&frame("b", 0, 2, "{ ")).is_err());
        assert_eq!(chunks.groups.len(), 1);
        assert_eq!(chunks.bytes, overhead + 2);
        assert_eq!(
            chunks.push(&frame("a", 1, 2, "}")).unwrap(),
            Some(json!({}))
        );
        assert!(chunks.push(&frame("b", 0, 2, "{ ")).unwrap().is_none());
    }

    #[test]
    fn resetting_a_group_releases_previous_parts_and_ignores_invalid_indices() {
        let mut chunks = ChunkBuffer::default();
        chunks.push(&frame("a", 0, 2, "old part")).unwrap();
        chunks.push(&frame("a", 8, 3, "bad index")).unwrap();
        assert_eq!(chunks.bytes, 1 + 2 * size_of::<Option<String>>() + 8);
        assert_eq!(
            chunks.push(&frame("a", 0, 1, "{}")).unwrap(),
            Some(json!({}))
        );
        assert_eq!(chunks.bytes, 0);
    }
}
