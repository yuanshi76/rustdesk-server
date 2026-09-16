//! The websocket idle timeout: a connection that goes silent must be closed,
//! so that a peer which vanished without a FIN cannot hold a socket forever.

mod support;

use hbb_common::tokio;
use std::time::Duration;
use support::*;

const PORT: i32 = 35116;
const WS_PORT: i32 = PORT + 2;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_websockets_are_closed() {
    // A connection that says nothing at all must not be held open forever.
    // One second, so the test does not take ninety.
    std::env::set_var("WS_IDLE_TIMEOUT", "1");
    start_server(PORT, "hbbs-ws-idle");
    wait_for_port(WS_PORT).await;

    let mut ws = connect(WS_PORT, None).await;
    register_pk(&mut ws, "wsidle-b").await;

    use hbb_common::futures_util::StreamExt;
    let closed = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(Ok(_frame)) = ws.next().await {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "an idle websocket should have been closed by the server"
    );
}
