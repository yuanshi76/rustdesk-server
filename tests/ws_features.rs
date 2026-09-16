//! Feature assertions for the fork's websocket support (KEEP patch 0025).
//!
//! Phase 1 is also the other half of the leak regression: before the fix, the
//! server stopped reading a websocket the first time the client sent a
//! heartbeat, which is what made clients reconnect every few minutes.

mod support;

use hbb_common::{rendezvous_proto::*, tokio};
use std::time::Duration;
use support::*;

const PORT: i32 = 34116;
const WS_PORT: i32 = PORT + 2;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_peers_are_served_over_one_persistent_connection() {
    start_server(PORT, "hbbs-ws-features");
    wait_for_port(WS_PORT).await;

    // --- phase 1: one connection serves registration, heartbeat and query ---
    let mut b = connect(WS_PORT, None).await;
    register_pk(&mut b, "wsfeat-b").await;

    // A heartbeat must not end the connection.
    for round in 0..3 {
        let mut msg = RendezvousMessage::new();
        msg.set_register_peer(RegisterPeer {
            id: "wsfeat-b".to_owned(),
            serial: 0,
            ..Default::default()
        });
        send(&mut b, msg).await;
        match recv(&mut b, "RegisterPeerResponse").await.union {
            Some(rendezvous_message::Union::RegisterPeerResponse(_)) => {}
            other => panic!("round {round}: expected RegisterPeerResponse, got {other:?}"),
        }
    }

    // ... and the same connection still answers an online query.
    let mut msg = RendezvousMessage::new();
    msg.set_online_request(OnlineRequest {
        peers: vec!["wsfeat-b".to_owned(), "nobody-here".to_owned()],
        ..Default::default()
    });
    send(&mut b, msg).await;
    match recv(&mut b, "OnlineResponse").await.union {
        Some(rendezvous_message::Union::OnlineResponse(r)) => {
            assert_eq!(r.states.len(), 1, "two peers pack into one byte");
            // Most significant bit first: peer 0 online, peer 1 not.
            assert_eq!(
                r.states[0], 0b1000_0000,
                "expected only the registered peer to be online"
            );
        }
        other => panic!("expected OnlineResponse, got {other:?}"),
    }

    // --- phase 2: a punch-hole request reaches B on that same connection ---
    let mut a = connect(WS_PORT, None).await;
    register_pk(&mut a, "wsfeat-a").await;

    let mut msg = RendezvousMessage::new();
    msg.set_punch_hole_request(PunchHoleRequest {
        id: "wsfeat-b".to_owned(),
        ..Default::default()
    });
    send(&mut a, msg).await;

    match recv(&mut b, "PunchHole pushed to B").await.union {
        Some(rendezvous_message::Union::PunchHole(_)) => {}
        other => panic!("expected B to be woken with PunchHole, got {other:?}"),
    }

    // B is still usable afterwards: the push must not consume its connection.
    let mut msg = RendezvousMessage::new();
    msg.set_online_request(OnlineRequest {
        peers: vec!["wsfeat-b".to_owned()],
        ..Default::default()
    });
    send(&mut b, msg).await;
    match recv(&mut b, "OnlineResponse after push").await.union {
        Some(rendezvous_message::Union::OnlineResponse(_)) => {}
        other => panic!("B unusable after a push: {other:?}"),
    }
}
