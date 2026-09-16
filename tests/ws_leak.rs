//! Regression test for the websocket socket leak.
//!
//! Opens N websocket connections to hbbs, registers on each one, closes them
//! client-side, and asserts that the server gave its file descriptors back.
//!
//! The server runs in this process, so "the server's file descriptors" are this
//! process's file descriptors. `/dev/fd` lists them on both Linux (where it is
//! `/proc/self/fd`) and macOS.
//!
//! This file deliberately duplicates the helpers in tests/support/ instead of
//! using them: it is meant to be dropped, unchanged, into another checkout -
//! upstream's, or the fork's - to reproduce the leak there. That is how the
//! three control measurements in NOTES.md were taken.
//!
//! Against the pre-fix build this test fails with a delta of roughly N: the
//! fork moved each websocket's write half into a global map and never removed
//! it, so the socket stayed open in CLOSE_WAIT for the lifetime of the process.
//! See NOTES.md.

use hbb_common::{
    bytes::Bytes,
    futures_util::{SinkExt, StreamExt},
    protobuf::Message as _,
    rendezvous_proto::*,
    tokio,
};
use hbbs::common::set_arg;
use hbbs::RendezvousServer;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, http::HeaderValue, Message as WsMessage,
};

/// hbbs derives nat_port = port - 1 and ws_port = port + 2.
const PORT: i32 = 31116;
const WS_PORT: i32 = PORT + 2;
const N: usize = 200;
/// Room for runtime noise (sqlite handles, epoll registrations, timers).
const TOLERANCE: usize = 25;

fn fd_count() -> usize {
    std::fs::read_dir("/dev/fd")
        .expect("/dev/fd should be readable")
        .count()
}

/// One websocket connection: connect, register, read the reply, close.
///
/// Each connection claims a distinct X-Real-IP and a distinct peer id. That is
/// the reverse-proxy path hbbs is normally deployed behind, and it is also what
/// keeps 200 registrations from tripping the per-IP rate limiter, which refuses
/// more than 30 registrations per IP per block interval.
async fn register_and_close(i: usize) -> Result<(), String> {
    let mut req = format!("ws://127.0.0.1:{WS_PORT}")
        .into_client_request()
        .map_err(|e| e.to_string())?;
    let ip = format!("10.{}.{}.{}", i / 65536 % 256, i / 256 % 256, i % 256);
    req.headers_mut().insert(
        "X-Real-IP",
        HeaderValue::from_str(&ip).map_err(|e| e.to_string())?,
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| format!("connect: {e}"))?;

    let mut msg = RendezvousMessage::new();
    msg.set_register_pk(RegisterPk {
        id: format!("leak{i:06}"),
        uuid: Bytes::from(format!("uuid-{i}")),
        pk: Bytes::from(format!("pk-{i}")),
        ..Default::default()
    });
    ws.send(WsMessage::Binary(msg.write_to_bytes().unwrap()))
        .await
        .map_err(|e| format!("send: {e}"))?;

    let reply = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .map_err(|_| "timed out waiting for RegisterPkResponse".to_string())?
        .ok_or_else(|| "connection closed before RegisterPkResponse".to_string())?
        .map_err(|e| format!("recv: {e}"))?;

    let WsMessage::Binary(bytes) = reply else {
        return Err(format!("expected a binary frame, got {reply:?}"));
    };
    let parsed = RendezvousMessage::parse_from_bytes(&bytes).map_err(|e| e.to_string())?;
    match parsed.union {
        Some(rendezvous_message::Union::RegisterPkResponse(r)) => {
            if r.result.enum_value() != Ok(register_pk_response::Result::OK) {
                return Err(format!("registration refused: {:?}", r.result));
            }
        }
        other => return Err(format!("unexpected reply: {other:?}")),
    }

    // Close cleanly, the way a client that is going away does.
    ws.close(None).await.ok();
    drop(ws);
    Ok(())
}

async fn wait_for_port() {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", WS_PORT as u16))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("hbbs did not start listening on {WS_PORT}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_connections_do_not_leak_sockets() {
    let db = std::env::temp_dir().join(format!("hbbs-ws-leak-{}.sqlite3", std::process::id()));
    set_arg("DB_URL", db.to_str().unwrap());
    // Without this, RendezvousServer::new runs a self-test that calls
    // std::process::exit(1) on failure, which would take the test runner with it.
    set_arg("TEST_HBBS", "no");

    // start() is #[tokio::main]: it builds and blocks on its own runtime, so it
    // gets its own OS thread. Same process, so its file descriptors are ours.
    std::thread::spawn(|| {
        let _ = RendezvousServer::start(PORT, 0, "", 0);
    });
    wait_for_port().await;

    // Warm up: the first connection pulls in lazily-created runtime resources
    // that must not be counted as a leak.
    register_and_close(0).await.expect("warm-up connection");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let baseline = fd_count();
    println!("baseline fds: {baseline}");

    for i in 1..=N {
        if let Err(e) = register_and_close(i).await {
            panic!("connection {i} failed: {e}");
        }
    }

    // Give the server a moment to notice the closes and drop its side.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let after = fd_count();
    let leaked = after.saturating_sub(baseline);
    println!("after {N} connections: {after} fds ({leaked} above baseline)");

    let _ = std::fs::remove_file(&db);

    assert!(
        leaked <= TOLERANCE,
        "leaked {leaked} file descriptors after {N} websocket connections \
         (baseline {baseline}, after {after}, tolerance {TOLERANCE}). \
         Every accepted websocket must be closed on every exit path."
    );
}
