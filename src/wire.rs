//! Amazon Quick 双向 WebSocket（AIDP）帧编解码。
//!
//! AWS event-stream 二进制帧格式：
//! `[total_len:4][headers_len:4][prelude_crc:4][headers][payload][message_crc:4]`

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use std::collections::HashMap;

const HV_BOOL_TRUE: u8 = 0;
const HV_BOOL_FALSE: u8 = 1;
const HV_BYTE: u8 = 2;
const HV_SHORT: u8 = 3;
const HV_INT: u8 = 4;
const HV_LONG: u8 = 5;
const HV_BYTES: u8 = 6;
const HV_STRING: u8 = 7;
const HV_TIMESTAMP: u8 = 8;
const HV_UUID: u8 = 9;

#[derive(Debug, Clone)]
pub enum HeaderValue {
    Bool(bool),
    Byte(u8),
    Short(u16),
    Int(u32),
    Long(u64),
    Bytes(Vec<u8>),
    String(String),
    Uuid([u8; 16]),
}

impl HeaderValue {
    pub fn as_str(&self) -> Option<String> {
        match self {
            HeaderValue::String(s) => Some(s.clone()),
            HeaderValue::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventStreamMessage {
    pub headers: HashMap<String, HeaderValue>,
    pub payload: Vec<u8>,
}

pub fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

pub fn event_stream_encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        let nb = name.as_bytes();
        let vb = value.as_bytes();
        header_bytes.push(nb.len() as u8);
        header_bytes.extend_from_slice(nb);
        header_bytes.push(HV_STRING);
        header_bytes.extend_from_slice(&(vb.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(vb);
    }

    let headers_len = header_bytes.len() as u32;
    let total_len = (12 + header_bytes.len() + payload.len() + 4) as u32;
    let mut msg = Vec::with_capacity(total_len as usize);
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&headers_len.to_be_bytes());
    let prelude_crc = crc32(&msg[0..8]);
    msg.extend_from_slice(&prelude_crc.to_be_bytes());
    msg.extend_from_slice(&header_bytes);
    msg.extend_from_slice(payload);
    let msg_crc = crc32(&msg);
    msg.extend_from_slice(&msg_crc.to_be_bytes());
    msg
}

fn decode_event_headers(b: &[u8]) -> Result<HashMap<String, HeaderValue>> {
    let mut out = HashMap::new();
    let mut i = 0usize;
    let need = |pos: usize, n: usize, what: &str| -> Result<()> {
        if pos + n > b.len() {
            bail!("truncated {what}");
        }
        Ok(())
    };

    while i < b.len() {
        let name_len = b[i] as usize;
        i += 1;
        need(i, name_len, "event header name")?;
        let name = String::from_utf8_lossy(&b[i..i + name_len]).into_owned();
        i += name_len;
        need(i, 1, "event header type")?;
        let typ = b[i];
        i += 1;
        let val = match typ {
            HV_BOOL_TRUE => HeaderValue::Bool(true),
            HV_BOOL_FALSE => HeaderValue::Bool(false),
            HV_BYTE => {
                need(i, 1, "byte header")?;
                let v = b[i];
                i += 1;
                HeaderValue::Byte(v)
            }
            HV_SHORT => {
                need(i, 2, "short header")?;
                let v = u16::from_be_bytes([b[i], b[i + 1]]);
                i += 2;
                HeaderValue::Short(v)
            }
            HV_INT => {
                need(i, 4, "int header")?;
                let v = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
                i += 4;
                HeaderValue::Int(v)
            }
            HV_LONG | HV_TIMESTAMP => {
                need(i, 8, "long header")?;
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&b[i..i + 8]);
                i += 8;
                HeaderValue::Long(u64::from_be_bytes(arr))
            }
            HV_BYTES | HV_STRING => {
                need(i, 2, "header length")?;
                let n = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
                i += 2;
                need(i, n, "header value")?;
                let slice = &b[i..i + n];
                i += n;
                if typ == HV_STRING {
                    HeaderValue::String(String::from_utf8_lossy(slice).into_owned())
                } else {
                    HeaderValue::Bytes(slice.to_vec())
                }
            }
            HV_UUID => {
                need(i, 16, "uuid header")?;
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&b[i..i + 16]);
                i += 16;
                HeaderValue::Uuid(arr)
            }
            _ => bail!("unsupported event header type {typ}"),
        };
        out.insert(name, val);
    }
    Ok(out)
}

pub fn event_stream_decode_all(buf: &[u8]) -> Result<(Vec<EventStreamMessage>, usize)> {
    let mut messages = Vec::new();
    let mut pos = 0usize;
    while pos + 16 <= buf.len() {
        let total = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        if total < 16 || total > 16 * 1024 * 1024 {
            bail!("invalid event-stream frame length {total}");
        }
        if pos + total > buf.len() {
            break;
        }
        let headers_len =
            u32::from_be_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        let hstart = pos + 12;
        let hend = hstart + headers_len;
        if hend > pos + total - 4 {
            bail!("invalid event-stream header length {headers_len}");
        }
        let headers = decode_event_headers(&buf[hstart..hend])?;
        let payload = buf[hend..pos + total - 4].to_vec();
        messages.push(EventStreamMessage { headers, payload });
        pos += total;
    }
    Ok((messages, pos))
}

fn header_string(headers: &HashMap<String, HeaderValue>, key: &str) -> String {
    headers.get(key).and_then(|v| v.as_str()).unwrap_or_default()
}

fn frame_error(headers: &HashMap<String, HeaderValue>) -> Option<anyhow::Error> {
    let msg_type = header_string(headers, ":message-type");
    if msg_type != "error" && msg_type != "exception" {
        return None;
    }
    let code = {
        let c = header_string(headers, ":error-code");
        if c.is_empty() {
            header_string(headers, ":exception-type")
        } else {
            c
        }
    };
    let msg = header_string(headers, ":error-message");
    Some(anyhow!("upstream error [{code}] {msg}"))
}

/// AIDP 出站帧：UTF-8 JSON `{"eventType":"rawInputEvent","payload":{"payload":b64}}`。
pub fn aidp_encode(json_str: &str) -> Vec<u8> {
    let b64 = B64.encode(json_str.as_bytes());
    let envelope = json!({
        "eventType": "rawInputEvent",
        "payload": { "payload": b64 }
    });
    serde_json::to_vec(&envelope).expect("json serialize")
}

/// AIDP 入站帧：AWS event-stream，载荷为 JSON `{"payload": b64(json)}`；
/// 若未封装在 event-stream 中则回退为纯 JSON 对象解析。
pub fn aidp_decode(bytes: &[u8]) -> Result<Vec<String>> {
    let mut failure: Option<anyhow::Error> = None;
    let messages = match event_stream_decode_all(bytes) {
        Ok((msgs, _)) => msgs,
        Err(e) => {
            failure = Some(e);
            Vec::new()
        }
    };

    if messages.is_empty() {
        let text = String::from_utf8_lossy(bytes);
        if let Ok(Value::Object(_)) = serde_json::from_str::<Value>(&text) {
            return Ok(vec![text.into_owned()]);
        }
        let reason = failure
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no complete frame".to_string());
        bail!("decode AIDP event-stream: {reason}");
    }

    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        if let Some(err) = frame_error(&msg.headers) {
            return Err(err);
        }
        let outer: Value = serde_json::from_slice(&msg.payload)
            .map_err(|e| anyhow!("decode AIDP body: {e}"))?;
        let b64 = outer
            .get("payload")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("AIDP body has no payload"))?;
        let decoded = B64
            .decode(b64)
            .map_err(|e| anyhow!("decode AIDP payload base64: {e}"))?;
        out.push(String::from_utf8_lossy(&decoded).into_owned());
    }
    Ok(out)
}

pub fn encode_frame(value: &Value) -> Vec<u8> {
    let json_str = serde_json::to_string(value).expect("json serialize");
    aidp_encode(&json_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_stream_roundtrip() {
        let encoded = event_stream_encode(
            &[(":message-type", "event"), (":event-type", "test")],
            b"hello world",
        );
        let (msgs, consumed) = event_stream_decode_all(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload, b"hello world");
        assert_eq!(header_string(&msgs[0].headers, ":event-type"), "test");
    }

    #[test]
    fn test_aidp_roundtrip() {
        let sample = r#"{"type":"user_message","payload":{"text":"hello"}}"#;
        let out = aidp_encode(sample);
        let outer: Value = serde_json::from_slice(&out).unwrap();
        let b64 = outer["payload"]["payload"].as_str().unwrap();
        let server_payload = serde_json::to_vec(&json!({ "payload": b64 })).unwrap();
        let wrapped = event_stream_encode(&[(":message-type", "event")], &server_payload);
        let decoded = aidp_decode(&wrapped).unwrap();
        assert_eq!(decoded, vec![sample.to_string()]);
    }

    #[test]
    fn test_aidp_plain_json_fallback() {
        let sample = r#"{"type":"pong"}"#;
        let decoded = aidp_decode(sample.as_bytes()).unwrap();
        assert_eq!(decoded, vec![sample.to_string()]);
    }

    #[test]
    fn test_error_frame() {
        let err_frame = event_stream_encode(
            &[
                (":message-type", "exception"),
                (":exception-type", "ValidationException"),
                (":error-message", "bad frame"),
            ],
            b"{}",
        );
        let err = aidp_decode(&err_frame).unwrap_err().to_string();
        assert!(err.contains("upstream error [ValidationException] bad frame"));
    }
}
