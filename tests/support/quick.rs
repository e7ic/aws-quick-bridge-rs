use aws_quick_bridge_rs::config::AccountRecord;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub async fn receive(ws: &mut WebSocketStream<TcpStream>) -> Option<Value> {
    while let Some(msg) = ws.next().await {
        if let Ok(Message::Binary(bytes)) = msg {
            let outer: Value = serde_json::from_slice(&bytes).unwrap();
            let bytes = STANDARD
                .decode(outer["payload"]["payload"].as_str().unwrap())
                .unwrap();
            return Some(serde_json::from_slice(&bytes).unwrap());
        }
    }
    None
}

pub async fn send(ws: &mut WebSocketStream<TcpStream>, event: &str, payload: Value) {
    ws.send(Message::text(
        json!({ "type": event, "payload": payload }).to_string(),
    ))
    .await
    .unwrap();
}

pub async fn complete(ws: &mut WebSocketStream<TcpStream>) {
    send(ws, "message_complete", json!({})).await;
    send(ws, "turn_completed", json!({})).await;
}

pub fn account(id: &str, quick_port: u16, usage_port: u16) -> AccountRecord {
    serde_json::from_value(json!({
        "access": format!("fixture-{id}"),
        "accountId": id,
        "email": id,
        "expires": chrono::Utc::now().timestamp_millis() + 3_600_000,
        "tenantUrl": format!("http://127.0.0.1:{usage_port}"),
        "wsHost": format!("ws://127.0.0.1:{quick_port}"),
    }))
    .unwrap()
}
