//! Feature assertion for the fork's encrypted TCP rendezvous (KEEP patch 0001).
//!
//! Drives the two-phase handshake against a real hbbs: the server signs an
//! ephemeral public key with its long-term key, this test seals a symmetric key
//! to it, and everything after that is secretbox-encrypted in both directions.

mod support;

use hbb_common::{
    bytes::Bytes,
    bytes_codec::BytesCodec,
    futures_util::{SinkExt, StreamExt},
    protobuf::Message as _,
    rendezvous_proto::*,
    sodiumoxide::crypto::{box_, secretbox, sign},
    tcp::Encrypt,
    tokio,
    tokio_util::codec::Framed,
};
use std::process::{Child, Command};
use std::time::Duration;
use support::wait_for_port;

const PORT: i32 = 40116;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_tcp_rendezvous() {
    // A key pair, the way the Docker image's key-secret service makes one.
    let out = Command::new(env!("CARGO_BIN_EXE_rustdesk-utils"))
        .arg("genkeypair")
        .output()
        .expect("genkeypair");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pk_b64 = String::new();
    let mut sk_b64 = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Public Key:") {
            pk_b64 = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("Secret Key:") {
            sk_b64 = v.trim().to_string();
        }
    }
    let server_pk = sign::PublicKey::from_slice(&base64::decode(&pk_b64).unwrap()).unwrap();

    let dir = std::env::temp_dir().join(format!("hbbs-kex-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _hbbs = Proc(
        Command::new(env!("CARGO_BIN_EXE_hbbs"))
            .current_dir(&dir)
            .args(["-p", &PORT.to_string(), "-k", &sk_b64])
            .env("TEST-HBBS", "no")
            .env("DB-URL", dir.join("kex.sqlite3"))
            .spawn()
            .expect("hbbs"),
    );
    wait_for_port(PORT).await;

    let stream = tokio::net::TcpStream::connect(("127.0.0.1", PORT as u16))
        .await
        .expect("connect");
    let mut framed = Framed::new(stream, BytesCodec::new());

    // Phase 1: the server offers an ephemeral public key, signed with the key
    // pair above, so we can tell it really came from this server.
    let frame = tokio::time::timeout(Duration::from_secs(10), framed.next())
        .await
        .expect("timed out waiting for KeyExchange phase 1")
        .expect("connection closed before KeyExchange")
        .expect("read error");
    let msg = RendezvousMessage::parse_from_bytes(&frame).expect("undecodable phase 1");
    let signed = match msg.union {
        Some(rendezvous_message::Union::KeyExchange(ex)) => {
            assert_eq!(ex.keys.len(), 1, "phase 1 carries one signed key");
            ex.keys[0].clone()
        }
        other => panic!("expected KeyExchange, got {other:?}"),
    };
    let server_pk_b =
        sign::verify(&signed, &server_pk).expect("phase 1 was not signed by the server's key");
    let server_pk_b =
        box_::PublicKey::from_slice(&server_pk_b).expect("phase 1 key is not a public key");

    // Tampering with the signature must not verify: this is what stops a
    // man in the middle substituting its own ephemeral key.
    let mut tampered = signed.to_vec();
    tampered[0] ^= 0xff;
    assert!(
        sign::verify(&tampered, &server_pk).is_err(),
        "a tampered phase 1 message must not verify"
    );

    // Phase 2: seal a symmetric key to that public key.
    let (my_pk, my_sk) = box_::gen_keypair();
    let symmetric = secretbox::gen_key();
    let nonce = box_::Nonce([0u8; box_::NONCEBYTES]);
    let sealed = box_::seal(&symmetric.0, &nonce, &server_pk_b, &my_sk);

    let mut msg = RendezvousMessage::new();
    msg.set_key_exchange(KeyExchange {
        keys: vec![Bytes::from(my_pk.0.to_vec()), Bytes::from(sealed)],
        ..Default::default()
    });
    framed
        .send(Bytes::from(msg.write_to_bytes().unwrap()))
        .await
        .expect("send phase 2");

    // Everything from here is encrypted, in both directions.
    let mut crypt = Encrypt::new(symmetric);

    let mut msg = RendezvousMessage::new();
    msg.set_register_pk(RegisterPk {
        id: "kex-peer".to_owned(),
        uuid: Bytes::from_static(b"uuid-kex"),
        pk: Bytes::from_static(b"pk-kex"),
        ..Default::default()
    });
    let payload = crypt.enc(&msg.write_to_bytes().unwrap());
    framed
        .send(Bytes::from(payload))
        .await
        .expect("send encrypted RegisterPk");

    let mut frame = tokio::time::timeout(Duration::from_secs(10), framed.next())
        .await
        .expect("timed out waiting for the encrypted reply")
        .expect("connection closed before the encrypted reply")
        .expect("read error");
    crypt
        .dec(&mut frame)
        .expect("server's reply did not decrypt: the two sides disagree on the key");
    let msg = RendezvousMessage::parse_from_bytes(&frame).expect("undecodable reply");
    match msg.union {
        Some(rendezvous_message::Union::RegisterPkResponse(r)) => assert_eq!(
            r.result.enum_value(),
            Ok(register_pk_response::Result::OK),
            "registration over the encrypted connection was refused"
        ),
        other => panic!("expected RegisterPkResponse, got {other:?}"),
    }

    // A malformed phase 2 must not take the server down. The fork's version
    // did `ex.keys[0].to_vec().try_into().unwrap()` and `panic!` on a bad seal,
    // both reachable by any unauthenticated peer.
    for keys in [
        vec![],
        vec![Bytes::from_static(b"one key only")],
        vec![
            Bytes::from_static(b"short"),
            Bytes::from_static(b"also short"),
        ],
        vec![
            Bytes::from(my_pk.0.to_vec()),
            Bytes::from(vec![0u8; 48]), // right length, will not open
        ],
    ] {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", PORT as u16))
            .await
            .expect("connect");
        let mut framed = Framed::new(stream, BytesCodec::new());
        let _phase1 = tokio::time::timeout(Duration::from_secs(5), framed.next()).await;
        let mut msg = RendezvousMessage::new();
        msg.set_key_exchange(KeyExchange {
            keys,
            ..Default::default()
        });
        framed
            .send(Bytes::from(msg.write_to_bytes().unwrap()))
            .await
            .ok();
        // The server is entitled to close this connection; it must not die.
        let _ = tokio::time::timeout(Duration::from_secs(5), framed.next()).await;
    }

    // Still serving.
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", PORT as u16))
        .await
        .expect("hbbs stopped accepting connections after a malformed KeyExchange");
    let mut framed = Framed::new(stream, BytesCodec::new());
    let frame = tokio::time::timeout(Duration::from_secs(10), framed.next())
        .await
        .expect("hbbs stopped answering after a malformed KeyExchange")
        .expect("connection closed")
        .expect("read error");
    let msg = RendezvousMessage::parse_from_bytes(&frame).expect("undecodable phase 1");
    assert!(
        matches!(msg.union, Some(rendezvous_message::Union::KeyExchange(_))),
        "expected a fresh KeyExchange phase 1"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
