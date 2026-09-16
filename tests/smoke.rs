//! Smoke test: a real hbbs and hbbr, started the way the Docker image starts
//! them, driven through the rendezvous handshake end to end.
//!
//! What this does cover: key generation and validation, the expected listeners,
//! the direct punch-hole exchange, and the relay exchange - all over the
//! websocket transport, which is the path the leak fix rewrote.
//!
//! What it does not cover: an actual RustDesk client establishing a session.
//! That needs the client, which is not available here.

mod support;

use hbb_common::{rendezvous_proto::*, tokio, AddrMangle};
use std::net::SocketAddr;
use std::process::{Child, Command};
use std::time::Duration;
use support::*;

const PORT: i32 = 38116;
const WS_PORT: i32 = PORT + 2;
const NAT_PORT: i32 = PORT - 1;
const RELAY_PORT: i32 = PORT + 1;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn keypair() -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rustdesk-utils"))
        .arg("genkeypair")
        .output()
        .expect("genkeypair");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pk = String::new();
    let mut sk = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Public Key:") {
            pk = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("Secret Key:") {
            sk = v.trim().to_string();
        }
    }
    assert!(!pk.is_empty() && !sk.is_empty(), "genkeypair said: {text}");
    (pk, sk)
}

async fn listens(port: i32) -> bool {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port as u16))
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rendezvous_handshake_end_to_end() {
    let (pk, sk) = keypair();
    let out = Command::new(env!("CARGO_BIN_EXE_rustdesk-utils"))
        .args(["validatekeypair", &pk, &sk])
        .output()
        .expect("validatekeypair");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("VALID"),
        "generated key pair did not validate"
    );

    let dir = std::env::temp_dir().join(format!("hbbs-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let _hbbr = Proc(
        Command::new(env!("CARGO_BIN_EXE_hbbr"))
            .current_dir(&dir)
            .args(["-p", &RELAY_PORT.to_string(), "-k", &sk])
            .spawn()
            .expect("hbbr"),
    );
    let _hbbs = Proc(
        Command::new(env!("CARGO_BIN_EXE_hbbs"))
            .current_dir(&dir)
            .args([
                "-p",
                &PORT.to_string(),
                "-k",
                &sk,
                "-r",
                &format!("127.0.0.1:{RELAY_PORT}"),
            ])
            .env("TEST-HBBS", "no")
            .env("DB-URL", dir.join("smoke.sqlite3"))
            .spawn()
            .expect("hbbs"),
    );

    assert!(listens(PORT).await, "hbbs is not listening on {PORT}");
    assert!(
        listens(NAT_PORT).await,
        "no NAT-test listener on {NAT_PORT}"
    );
    assert!(listens(WS_PORT).await, "no websocket listener on {WS_PORT}");
    assert!(listens(RELAY_PORT).await, "hbbr is not listening");

    // B comes online.
    let mut b = connect(WS_PORT, Some("10.20.0.2")).await;
    register_pk(&mut b, "smoke-peer-b").await;

    // --- direct: A asks to punch a hole to B ---
    let mut a = connect(WS_PORT, Some("10.20.0.1")).await;
    register_pk(&mut a, "smoke-peer-a").await;

    let mut msg = RendezvousMessage::new();
    msg.set_punch_hole_request(PunchHoleRequest {
        id: "smoke-peer-b".to_owned(),
        // hbbs was started with -k <secret key>, so it only serves clients that
        // present the matching public key.
        licence_key: pk.clone(),
        ..Default::default()
    });
    send(&mut a, msg).await;

    let addr_a = match recv(&mut b, "PunchHole at B").await.union {
        Some(rendezvous_message::Union::PunchHole(ph)) => {
            assert!(!ph.socket_addr.is_empty(), "B was not told where A is");
            let decoded: SocketAddr = AddrMangle::decode(&ph.socket_addr);
            assert_eq!(
                decoded.ip().to_string(),
                "10.20.0.1",
                "B should be told A's real address"
            );
            ph.socket_addr
        }
        other => panic!("expected PunchHole at B, got {other:?}"),
    };

    // B says it is ready; the server must carry that back to A.
    let mut msg = RendezvousMessage::new();
    msg.set_punch_hole_sent(PunchHoleSent {
        socket_addr: addr_a.clone(),
        id: "smoke-peer-b".to_owned(),
        version: "1.4.1".to_owned(),
        ..Default::default()
    });
    send(&mut b, msg).await;

    match recv(&mut a, "PunchHoleResponse at A").await.union {
        Some(rendezvous_message::Union::PunchHoleResponse(r)) => {
            assert!(
                r.other_failure.is_empty(),
                "punch hole refused: {}",
                r.other_failure
            );
            assert!(!r.socket_addr.is_empty(), "A was not told where B is");
        }
        other => panic!("expected PunchHoleResponse at A, got {other:?}"),
    }

    // --- relayed: A asks for a relay to B ---
    let mut msg = RendezvousMessage::new();
    msg.set_request_relay(RequestRelay {
        id: "smoke-peer-b".to_owned(),
        ..Default::default()
    });
    send(&mut a, msg).await;

    let a_mangled = match recv(&mut b, "RequestRelay at B").await.union {
        Some(rendezvous_message::Union::RequestRelay(rf)) => rf.socket_addr,
        other => panic!("expected RequestRelay at B, got {other:?}"),
    };

    let mut msg = RendezvousMessage::new();
    let mut rr = RelayResponse {
        socket_addr: a_mangled,
        version: "1.4.1".to_owned(),
        relay_server: format!("127.0.0.1:{RELAY_PORT}"),
        ..Default::default()
    };
    rr.set_id("smoke-peer-b".to_owned());
    msg.set_relay_response(rr);
    send(&mut b, msg).await;

    match recv(&mut a, "RelayResponse at A").await.union {
        Some(rendezvous_message::Union::RelayResponse(rr)) => {
            assert!(
                !rr.relay_server.is_empty(),
                "A was not given a relay server"
            );
        }
        other => panic!("expected RelayResponse at A, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}
