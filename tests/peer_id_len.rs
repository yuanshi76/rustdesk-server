//! Upstream #713 caps a peer id at 253 bytes, on the UDP path. This fork also
//! registers peers over TCP and websocket, through a shared handler, so the same
//! cap must hold there: an over-long id is refused at registration and ignored in
//! a heartbeat.

mod support;

use hbb_common::{bytes::Bytes, rendezvous_proto::*, tokio};
use support::*;

const PORT: i32 = 20386;
const WS_PORT: i32 = PORT + 2;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_long_peer_id_is_refused_over_websocket() {
    start_server(PORT, "hbbs-id-len");
    wait_for_port(WS_PORT).await;
    let mut ws = connect(WS_PORT, None).await;

    // 253 bytes is still a valid id.
    register_pk(&mut ws, &"a".repeat(253)).await;

    // 254 is not.
    let long = "b".repeat(254);
    let mut msg = RendezvousMessage::new();
    msg.set_register_pk(RegisterPk {
        id: long.clone(),
        uuid: Bytes::from("uuid-long"),
        pk: Bytes::from("pk-long"),
        ..Default::default()
    });
    send(&mut ws, msg).await;
    match recv(&mut ws, "RegisterPkResponse").await.union {
        Some(rendezvous_message::Union::RegisterPkResponse(r)) => assert_eq!(
            r.result.enum_value(),
            Ok(register_pk_response::Result::UUID_MISMATCH),
            "a 254-byte id was accepted"
        ),
        other => panic!("expected RegisterPkResponse, got {other:?}"),
    }

    // A refused registration closes that connection (the fork's behaviour), so the
    // heartbeat goes on a new one. It must get no answer: the next message on the
    // connection is the answer to the query sent after it.
    let mut ws = connect(WS_PORT, None).await;
    let mut msg = RendezvousMessage::new();
    msg.set_register_peer(RegisterPeer {
        id: long,
        ..Default::default()
    });
    send(&mut ws, msg).await;
    let mut msg = RendezvousMessage::new();
    msg.set_online_request(OnlineRequest {
        peers: vec!["nobody".to_owned()],
        ..Default::default()
    });
    send(&mut ws, msg).await;
    match recv(&mut ws, "OnlineResponse").await.union {
        Some(rendezvous_message::Union::OnlineResponse(_)) => {}
        other => panic!("the over-long heartbeat was answered: {other:?}"),
    }
}
