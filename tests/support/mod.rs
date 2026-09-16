//! Shared helpers for the websocket integration tests.
//!
//! Each tests/*.rs is its own binary, so each one starts its own hbbs on its own
//! port range. This file is included with `mod`, not compiled on its own.

#![allow(dead_code)]

use hbb_common::{protobuf::Message as _, rendezvous_proto::*, tokio};
use std::time::Duration;
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message as WsMessage},
    MaybeTlsStream, WebSocketStream,
};

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Start hbbs on its own thread. start() is #[tokio::main], so it builds and
/// blocks on its own runtime and cannot be spawned onto the caller's.
pub fn start_server(port: i32, db_name: &str) {
    let db = std::env::temp_dir().join(format!("{db_name}-{}.sqlite3", std::process::id()));
    std::env::set_var("DB-URL", db.to_str().unwrap());
    // Otherwise RendezvousServer::new runs a self-test that calls
    // std::process::exit(1) on failure, taking the test runner with it.
    std::env::set_var("TEST-HBBS", "no");
    std::thread::spawn(move || {
        if let Err(e) = hbbs::RendezvousServer::start(port, 0, "", 0) {
            eprintln!("hbbs on port {port} exited: {e}");
        }
    });
}

pub async fn wait_for_port(ws_port: i32) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", ws_port as u16))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("hbbs did not start listening on {ws_port}");
}

pub async fn connect(ws_port: i32, real_ip: Option<&str>) -> Ws {
    let mut req = format!("ws://127.0.0.1:{ws_port}")
        .into_client_request()
        .unwrap();
    if let Some(ip) = real_ip {
        req.headers_mut()
            .insert("X-Real-IP", HeaderValue::from_str(ip).unwrap());
    }
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("websocket handshake");
    ws
}

pub async fn send(ws: &mut Ws, msg: RendezvousMessage) {
    use hbb_common::futures_util::SinkExt;
    ws.send(WsMessage::Binary(msg.write_to_bytes().unwrap()))
        .await
        .expect("send");
}

/// Next protocol message, skipping transport frames (Ping/Pong).
pub async fn recv(ws: &mut Ws, what: &str) -> RendezvousMessage {
    use hbb_common::futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame = tokio::time::timeout_at(deadline, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("connection closed while waiting for {what}"))
            .unwrap_or_else(|e| panic!("stream error while waiting for {what}: {e}"));
        match frame {
            WsMessage::Binary(bytes) => {
                return RendezvousMessage::parse_from_bytes(&bytes)
                    .unwrap_or_else(|e| panic!("undecodable {what}: {e}"))
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("expected a binary frame for {what}, got {other:?}"),
        }
    }
}

/// Register a peer id over an open websocket and assert the server accepted it.
pub async fn register_pk(ws: &mut Ws, id: &str) {
    use hbb_common::bytes::Bytes;
    let mut msg = RendezvousMessage::new();
    msg.set_register_pk(RegisterPk {
        id: id.to_owned(),
        uuid: Bytes::from(format!("uuid-{id}")),
        pk: Bytes::from(format!("pk-{id}")),
        ..Default::default()
    });
    send(ws, msg).await;
    match recv(ws, "RegisterPkResponse").await.union {
        Some(rendezvous_message::Union::RegisterPkResponse(r)) => assert_eq!(
            r.result.enum_value(),
            Ok(register_pk_response::Result::OK),
            "registration of {id} refused"
        ),
        other => panic!("expected RegisterPkResponse, got {other:?}"),
    }
}
