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

// PORTS. Every test that starts an hbbs uses a fixed port base in 20116..20266.
// That range is chosen deliberately: it is below the OS's ephemeral port range on
// both Linux (32768-60999) and macOS/Windows (49152-65535). The ephemeral range is
// where outgoing connections and `bind("127.0.0.1:0")` get their ports from, so a
// fixed port inside it can be taken by one of them first. This bit on Linux CI, as
// "Address already in use", once tests started opening many sockets in one process;
// it never showed on macOS, where the fixed ports happened to be outside the range.
// Keep new ports below 32768, and away from RustDesk's own 21115-21119.

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

/// A tiny HTTP server for tests of things that download: serves fixed bodies by path
/// on 127.0.0.1, answers 404 for anything else, and records every path requested.
pub struct FileServer {
    pub base: String,
    files: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (u16, Vec<u8>)>>>,
    pub requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl FileServer {
    pub fn start() -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files: std::sync::Arc<
            std::sync::Mutex<std::collections::HashMap<String, (u16, Vec<u8>)>>,
        > = Default::default();
        let requests: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let (f, r) = (files.clone(), requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (f, r) = (f.clone(), r.clone());
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_owned();
                    r.lock().unwrap().push(path.clone());
                    let (status, body) = f
                        .lock()
                        .unwrap()
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, b"not found".to_vec()));
                    let _ = write!(
                        s,
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(&body);
                });
            }
        });
        Self {
            base,
            files,
            requests,
        }
    }

    pub fn serve(&self, path: &str, status: u16, body: Vec<u8>) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, body));
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
