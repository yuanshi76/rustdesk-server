//! The optional WebRTC signalling (src/webrtc_signal.rs), driven against a real hbbs
//! the way RustDesk 1.5.x clients drive it: the controller's offer and the peer's
//! answer ride on the punch messages, and ICE candidates (RendezvousMessage field 29)
//! travel both ways, the controller's on its punch connection and the peer's on a
//! connection of its own.
//!
//! The fields are newer than the protocol this server is built against, so they are
//! written and read here as raw protobuf fields, exactly as they appear on the wire.

mod support;

use hbb_common::{
    bytes::{Bytes, BytesMut},
    bytes_codec::BytesCodec,
    futures_util::{SinkExt, StreamExt},
    protobuf::{well_known_types::empty::Empty, Message as _, UnknownValueRef},
    rendezvous_proto::*,
    sodiumoxide::crypto::{box_, secretbox, sign},
    tcp::Encrypt,
    tokio,
    tokio_util::codec::Framed,
    AddrMangle,
};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use support::*;

const OFFER: &[u8] = b"webrtc://offer-envelope";
const ANSWER: &[u8] = b"webrtc://answer-envelope";
const SESSION_KEY: &str = "session-key-1";
const B_IP: &str = "10.50.0.2";

struct Hbbs {
    child: Child,
    dir: std::path::PathBuf,
    pk_b64: String,
    server_pk: sign::PublicKey,
}

impl Drop for Hbbs {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A real hbbs with a key pair, so TCP connections are encrypted as a real
/// client's are.
fn start_hbbs(port: i32, name: &str, webrtc: bool) -> Hbbs {
    let out = Command::new(env!("CARGO_BIN_EXE_rustdesk-utils"))
        .arg("genkeypair")
        .output()
        .expect("genkeypair");
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |label: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(label))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let (pk_b64, sk_b64) = (field("Public Key:"), field("Secret Key:"));
    let server_pk = sign::PublicKey::from_slice(&base64::decode(&pk_b64).unwrap()).unwrap();
    let dir = std::env::temp_dir().join(format!("hbbs-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hbbs"));
    cmd.current_dir(&dir)
        .args(["-p", &port.to_string(), "-k", &sk_b64])
        .env("TEST-HBBS", "no")
        .env("DB-URL", dir.join("hbbs.sqlite3"))
        .stdout(Stdio::null());
    if webrtc {
        cmd.env("WEBRTC", "Y");
    }
    Hbbs {
        child: cmd.spawn().expect("hbbs"),
        dir,
        pk_b64,
        server_pk,
    }
}

type Conn = Framed<tokio::net::TcpStream, BytesCodec>;

/// An encrypted TCP connection, as a desktop client makes it (see key_exchange.rs).
struct Tcp {
    conn: Conn,
    crypt: Encrypt,
}

impl Tcp {
    async fn connect(port: i32, server_pk: &sign::PublicKey) -> Self {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port as u16))
            .await
            .expect("connect");
        let mut conn = Framed::new(stream, BytesCodec::new());
        let frame = tokio::time::timeout(Duration::from_secs(10), conn.next())
            .await
            .expect("phase 1 timeout")
            .expect("closed before phase 1")
            .expect("read");
        let msg = RendezvousMessage::parse_from_bytes(&frame).unwrap();
        let Some(rendezvous_message::Union::KeyExchange(ex)) = msg.union else {
            panic!("expected KeyExchange, got {msg:?}");
        };
        let signed = sign::verify(&ex.keys[0], server_pk).expect("unsigned phase 1");
        let server_pk_b = box_::PublicKey::from_slice(&signed).unwrap();
        let (my_pk, my_sk) = box_::gen_keypair();
        let symmetric = secretbox::gen_key();
        let sealed = box_::seal(
            &symmetric.0,
            &box_::Nonce([0u8; box_::NONCEBYTES]),
            &server_pk_b,
            &my_sk,
        );
        let mut msg = RendezvousMessage::new();
        msg.set_key_exchange(KeyExchange {
            keys: vec![Bytes::from(my_pk.0.to_vec()), Bytes::from(sealed)],
            ..Default::default()
        });
        conn.send(Bytes::from(msg.write_to_bytes().unwrap()))
            .await
            .unwrap();
        Self {
            conn,
            crypt: Encrypt::new(symmetric),
        }
    }

    async fn send(&mut self, msg: &RendezvousMessage) {
        let bytes = self.crypt.enc(&msg.write_to_bytes().unwrap());
        self.conn.send(Bytes::from(bytes)).await.expect("tcp send");
    }

    /// The next message, or None if nothing arrives within `wait`.
    async fn recv_within(&mut self, wait: Duration) -> Option<RendezvousMessage> {
        let frame = tokio::time::timeout(wait, self.conn.next())
            .await
            .ok()??
            .ok()?;
        let mut bytes = BytesMut::from(&frame[..]);
        self.crypt.dec(&mut bytes).expect("decrypt");
        Some(RendezvousMessage::parse_from_bytes(&bytes).expect("decode"))
    }
}

fn unknown(fields: &hbb_common::protobuf::UnknownFields, n: u32) -> Option<Vec<u8>> {
    match fields.get(n) {
        Some(UnknownValueRef::LengthDelimited(v)) => Some(v.to_vec()),
        _ => None,
    }
}

fn punch_request(peer: &str, licence_key: &str, offer: Option<&[u8]>) -> RendezvousMessage {
    let mut ph = PunchHoleRequest {
        id: peer.to_owned(),
        licence_key: licence_key.to_owned(),
        ..Default::default()
    };
    if let Some(o) = offer {
        ph.special_fields
            .mut_unknown_fields()
            .add_length_delimited(12, o.to_vec());
    }
    let mut msg = RendezvousMessage::new();
    msg.set_punch_hole_request(ph);
    msg
}

fn hole_sent(peer: &str, to_controller: &[u8], answer: Option<&[u8]>) -> RendezvousMessage {
    let mut phs = PunchHoleSent {
        socket_addr: to_controller.to_vec().into(),
        id: peer.to_owned(),
        version: "1.5.3".to_owned(),
        ..Default::default()
    };
    if let Some(a) = answer {
        phs.special_fields
            .mut_unknown_fields()
            .add_length_delimited(8, a.to_vec());
    }
    let mut msg = RendezvousMessage::new();
    msg.set_punch_hole_sent(phs);
    msg
}

/// IceCandidate { id = 1, socket_addr = 2, session_key = 3, candidate = 4 } as field 29.
fn candidate(id: &str, socket_addr: &[u8], body: &str) -> RendezvousMessage {
    let mut ice = Empty::new();
    let f = ice.special_fields.mut_unknown_fields();
    if !id.is_empty() {
        f.add_length_delimited(1, id.as_bytes().to_vec());
    }
    if !socket_addr.is_empty() {
        f.add_length_delimited(2, socket_addr.to_vec());
    }
    f.add_length_delimited(3, SESSION_KEY.as_bytes().to_vec());
    f.add_length_delimited(4, body.as_bytes().to_vec());
    let mut msg = RendezvousMessage::new();
    msg.special_fields
        .mut_unknown_fields()
        .add_length_delimited(29, ice.write_to_bytes().unwrap());
    msg
}

/// The candidate body in a received message, if it is a candidate.
fn candidate_body(msg: &RendezvousMessage) -> Option<String> {
    let raw = unknown(msg.special_fields.unknown_fields(), 29)?;
    let ice = Empty::parse_from_bytes(&raw).ok()?;
    let body = unknown(ice.special_fields.unknown_fields(), 4)?;
    Some(String::from_utf8(body).unwrap())
}

/// Wait for a candidate on a websocket, skipping nothing else: the next frame must be it.
async fn ws_next(ws: &mut Ws, wait: Duration) -> Option<RendezvousMessage> {
    use hbb_common::futures_util::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let frame = tokio::time::timeout_at(deadline, ws.next())
            .await
            .ok()??
            .ok()?;
        match frame {
            WsMessage::Binary(b) => return RendezvousMessage::parse_from_bytes(&b).ok(),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            _ => return None,
        }
    }
}

async fn ws_send(ws: &mut Ws, msg: &RendezvousMessage) {
    send(ws, msg.clone()).await;
}

/// A desktop controller over encrypted TCP, a 1.5.x peer reachable over websocket:
/// the whole exchange, including candidates in both directions after the response.
async fn full_exchange(port: i32, name: &str, webrtc: bool) -> Vec<String> {
    let hbbs = start_hbbs(port, name, webrtc);
    wait_for_port(port).await;
    wait_for_port(port + 2).await;
    let peer = format!("{name}-b");
    let mut b = connect(port + 2, Some(B_IP)).await;
    register_pk(&mut b, &peer).await;
    let mut a = Tcp::connect(port, &hbbs.server_pk).await;
    let mut seen = Vec::new();

    // 1. Offer.
    a.send(&punch_request(&peer, &hbbs.pk_b64, Some(OFFER)))
        .await;
    let ph = match recv(&mut b, "PunchHole at B").await.union {
        Some(rendezvous_message::Union::PunchHole(ph)) => ph,
        other => panic!("expected PunchHole at B, got {other:?}"),
    };
    if unknown(ph.special_fields.unknown_fields(), 10).as_deref() == Some(OFFER) {
        seen.push("offer".to_owned());
    }
    let to_a = ph.socket_addr.to_vec();
    assert!(!to_a.is_empty());

    // 2. Answer.
    send(&mut b, hole_sent(&peer, &to_a, Some(ANSWER))).await;
    let resp = a
        .recv_within(Duration::from_secs(5))
        .await
        .expect("no PunchHoleResponse at A");
    match resp.union {
        Some(rendezvous_message::Union::PunchHoleResponse(r)) => {
            if unknown(r.special_fields.unknown_fields(), 12).as_deref() == Some(ANSWER) {
                seen.push("answer".to_owned());
            }
        }
        other => panic!("expected PunchHoleResponse at A, got {other:?}"),
    }

    // 3. B's candidate, on a connection of its own from B's address, to A, which
    //    has already had its response.
    let mut b_ice = connect(port + 2, Some(B_IP)).await;
    ws_send(&mut b_ice, &candidate("", &to_a, "cand-from-b")).await;
    if let Some(m) = a.recv_within(Duration::from_secs(3)).await {
        if candidate_body(&m).as_deref() == Some("cand-from-b") {
            seen.push("candidate to controller".to_owned());
        }
    }

    // 4. A's candidate, on its punch connection, to B.
    a.send(&candidate(&peer, b"", "cand-from-a")).await;
    if let Some(m) = ws_next(&mut b, Duration::from_secs(3)).await {
        if candidate_body(&m).as_deref() == Some("cand-from-a") {
            seen.push("candidate to peer".to_owned());
        }
    }

    // 5. A candidate for A from somewhere that is not B never reaches it.
    let mut stranger = connect(port + 2, Some("10.99.0.9")).await;
    ws_send(&mut stranger, &candidate("", &to_a, "cand-from-stranger")).await;
    if let Some(m) = a.recv_within(Duration::from_secs(2)).await {
        if candidate_body(&m).is_some() {
            seen.push("stranger's candidate delivered".to_owned());
        }
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_webrtc_on_offer_answer_and_candidates_flow_over_encrypted_tcp() {
    let seen = full_exchange(20406, "webrtc-on", true).await;
    assert_eq!(
        seen,
        [
            "offer",
            "answer",
            "candidate to controller",
            "candidate to peer"
        ],
        "with WEBRTC=Y"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_webrtc_off_nothing_new_passes_and_the_punch_works_as_before() {
    // The default. The same client messages: the punch itself works (the answer
    // step asserts a PunchHoleResponse arrives), but no WebRTC field or candidate
    // gets through, so the clients fall back to their other ways of connecting.
    let seen = full_exchange(20416, "webrtc-off", false).await;
    assert!(
        seen.is_empty(),
        "with WEBRTC off, these got through: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_in_a_relay_response_reaches_the_controller_and_keeps_its_connection() {
    // A peer behind a symmetric NAT answers in a RelayResponse instead.
    const PORT: i32 = 20426;
    const UUID: &str = "e4c181d0-41c7-4ae6-8ef0-36581b6b2b12";
    let hbbs = start_hbbs(PORT, "webrtc-relay", true);
    wait_for_port(PORT).await;
    wait_for_port(PORT + 2).await;
    let mut b = connect(PORT + 2, Some(B_IP)).await;
    register_pk(&mut b, "webrtc-relay-b").await;
    let mut a = Tcp::connect(PORT, &hbbs.server_pk).await;

    a.send(&punch_request("webrtc-relay-b", &hbbs.pk_b64, Some(OFFER)))
        .await;
    let to_a = match recv(&mut b, "PunchHole at B").await.union {
        Some(rendezvous_message::Union::PunchHole(ph)) => ph.socket_addr.to_vec(),
        other => panic!("expected PunchHole at B, got {other:?}"),
    };
    let mut rr = RelayResponse {
        socket_addr: to_a.clone().into(),
        uuid: UUID.to_owned(),
        relay_server: "relay.example.com:21117".to_owned(),
        ..Default::default()
    };
    rr.set_id("webrtc-relay-b".to_owned());
    rr.special_fields
        .mut_unknown_fields()
        .add_length_delimited(12, ANSWER.to_vec());
    let mut msg = RendezvousMessage::new();
    msg.set_relay_response(rr);
    send(&mut b, msg).await;

    let got = a
        .recv_within(Duration::from_secs(5))
        .await
        .expect("no RelayResponse at A");
    match got.union {
        Some(rendezvous_message::Union::RelayResponse(r)) => assert_eq!(
            unknown(r.special_fields.unknown_fields(), 12).as_deref(),
            Some(ANSWER),
            "the answer was lost from the RelayResponse"
        ),
        other => panic!("expected RelayResponse at A, got {other:?}"),
    }
    let mut b_ice = connect(PORT + 2, Some(B_IP)).await;
    ws_send(&mut b_ice, &candidate("", &to_a, "after-relay-response")).await;
    let m = a
        .recv_within(Duration::from_secs(3))
        .await
        .expect("A's connection did not survive the RelayResponse");
    assert_eq!(candidate_body(&m).as_deref(), Some("after-relay-response"));
    let _ = AddrMangle::decode(&to_a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_without_an_offer_is_handled_exactly_as_before() {
    // WEBRTC=Y, but a client that does not offer (WebRTC off on that device, or an
    // older client): the controller's connection gets its response and nothing else
    // is kept for it, so a candidate for it goes nowhere.
    const PORT: i32 = 20436;
    let hbbs = start_hbbs(PORT, "webrtc-nooffer", true);
    wait_for_port(PORT).await;
    wait_for_port(PORT + 2).await;
    let mut b = connect(PORT + 2, Some(B_IP)).await;
    register_pk(&mut b, "webrtc-nooffer-b").await;
    let mut a = Tcp::connect(PORT, &hbbs.server_pk).await;
    a.send(&punch_request("webrtc-nooffer-b", &hbbs.pk_b64, None))
        .await;
    let ph = match recv(&mut b, "PunchHole at B").await.union {
        Some(rendezvous_message::Union::PunchHole(ph)) => ph,
        other => panic!("expected PunchHole at B, got {other:?}"),
    };
    assert!(unknown(ph.special_fields.unknown_fields(), 10).is_none());
    send(&mut b, hole_sent("webrtc-nooffer-b", &ph.socket_addr, None)).await;
    assert!(matches!(
        a.recv_within(Duration::from_secs(5))
            .await
            .and_then(|m| m.union),
        Some(rendezvous_message::Union::PunchHoleResponse(_))
    ));
    let mut b_ice = connect(PORT + 2, Some(B_IP)).await;
    ws_send(&mut b_ice, &candidate("", &ph.socket_addr, "unexpected")).await;
    assert!(
        a.recv_within(Duration::from_secs(2)).await.is_none(),
        "a candidate reached a controller that never offered"
    );
}
