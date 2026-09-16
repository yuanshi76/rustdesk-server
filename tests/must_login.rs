//! End-to-end assertions for MUST_LOGIN (KEEP patch 0010) and the JWT check
//! that backs it (KEEP patch 0014), driven over a real websocket.

mod support;

use hbb_common::{rendezvous_proto::*, tokio};
use support::*;

const PORT: i32 = 36116;
const WS_PORT: i32 = PORT + 2;

async fn punch_hole_with(token: &str) -> PunchHoleResponse {
    let mut ws = connect(WS_PORT, None).await;
    let mut msg = RendezvousMessage::new();
    msg.set_punch_hole_request(PunchHoleRequest {
        id: "does-not-matter".to_owned(),
        token: token.to_owned(),
        ..Default::default()
    });
    send(&mut ws, msg).await;
    match recv(&mut ws, "PunchHoleResponse").await.union {
        Some(rendezvous_message::Union::PunchHoleResponse(r)) => r,
        other => panic!("expected PunchHoleResponse, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn must_login_gates_punch_hole_requests() {
    // Both are read once, at startup and on first jwt use respectively.
    std::env::set_var("MUST_LOGIN", "Y");
    std::env::set_var("RUSTDESK_API_JWT_KEY", "must-login-test-secret");
    start_server(PORT, "hbbs-must-login");
    wait_for_port(WS_PORT).await;

    let r = punch_hole_with("").await;
    assert_eq!(
        r.other_failure, "Connection failed, please login!",
        "an unauthenticated request must be refused"
    );

    let r = punch_hole_with("not.a.jwt").await;
    assert_eq!(
        r.other_failure, "Token error, please log out and log back in!",
        "a token that is not a valid JWT must be refused"
    );

    // A well-formed token signed with the wrong secret must not be accepted.
    // Minted here rather than via jwt::generate_token, which can only ever sign
    // with the one secret this process latched at startup.
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
        + 3600;
    let forged = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &hbbs::jwt::Claims { user_id: 1, exp },
        &jsonwebtoken::EncodingKey::from_secret(b"some-other-secret"),
    )
    .unwrap();
    let r = punch_hole_with(&forged).await;
    assert_eq!(
        r.other_failure, "Token error, please log out and log back in!",
        "a token signed with the wrong secret must be refused"
    );

    // A valid token gets past the gate; the request then fails on its merits,
    // because no such peer is registered.
    let token = hbbs::jwt::generate_token(1, 3600).unwrap();
    let r = punch_hole_with(&token).await;
    assert!(
        r.other_failure.is_empty(),
        "a valid token must not be refused, got {:?}",
        r.other_failure
    );
    assert_eq!(
        r.failure.enum_value(),
        Ok(punch_hole_response::Failure::ID_NOT_EXIST),
        "expected the request to proceed and fail on the unknown peer id"
    );
}
