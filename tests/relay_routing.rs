//! Relay selection, observed from outside: real hbbs processes, two stand-in
//! relays, and the relay each connection attempt is actually handed.
//!
//! Written before the feature. Without it, the pin test fails (round-robin hands
//! consecutive attempts different relays) and the routing tests fail (the table is
//! ignored).

mod support;

use hbb_common::{rendezvous_proto::*, tokio};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::*;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const A_IP: &str = "10.40.0.1";
const B_IP: &str = "10.40.0.2";

/// A stand-in relay: accepts TCP connections, which is all hbbs's health check does.
struct FakeRelay {
    addr: String,
    task: Option<JoinHandle<()>>,
}

impl FakeRelay {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                drop(s);
            }
        });
        Self {
            addr,
            task: Some(task),
        }
    }

    /// Stop listening, so hbbs's next health check fails.
    fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

struct Hbbs {
    child: Child,
    dir: std::path::PathBuf,
    pk: String,
    log: Arc<Mutex<String>>,
}

impl Drop for Hbbs {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_hbbs(port: i32, name: &str, relays: &[&str], env: &[(&str, &str)]) -> Hbbs {
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
    let (pk, sk) = (field("Public Key:"), field("Secret Key:"));

    let dir = std::env::temp_dir().join(format!("hbbs-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hbbs"));
    cmd.current_dir(&dir)
        .args(["-p", &port.to_string(), "-k", &sk, "-r", &relays.join(",")])
        .env("TEST-HBBS", "no")
        .env("DB-URL", dir.join("hbbs.sqlite3"))
        .stdout(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("hbbs");
    let stdout = child.stdout.take().unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    let sink = log.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(mut l) = sink.lock() {
                l.push_str(&line);
                l.push('\n');
            }
        }
    });
    Hbbs {
        child,
        dir,
        pk,
        log,
    }
}

impl Hbbs {
    fn log(&self) -> String {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

/// The two peers of a session, both reachable over websocket as a 1.4.1+ client is.
struct Pair {
    a: Ws,
    b: Ws,
    pk: String,
}

impl Pair {
    async fn connect(ws_port: i32, pk: &str) -> Self {
        let mut b = connect(ws_port, Some(B_IP)).await;
        register_pk(&mut b, "route-b").await;
        let a = connect(ws_port, Some(A_IP)).await;
        Self {
            a,
            b,
            pk: pk.to_owned(),
        }
    }

    /// One connection attempt: A asks for B, and the relay hbbs put in the
    /// PunchHole it pushed to B is the one it chose.
    async fn attempt(&mut self) -> String {
        let mut msg = RendezvousMessage::new();
        msg.set_punch_hole_request(PunchHoleRequest {
            id: "route-b".to_owned(),
            licence_key: self.pk.clone(),
            ..Default::default()
        });
        send(&mut self.a, msg).await;
        match recv(&mut self.b, "PunchHole at B").await.union {
            Some(rendezvous_message::Union::PunchHole(ph)) => ph.relay_server,
            other => panic!("expected PunchHole at B, got {other:?}"),
        }
    }
}

/// Keep attempting until `want` comes back, or panic after `within`.
async fn settle_on(pair: &mut Pair, want: &str, within: Duration, why: &str) {
    let deadline = Instant::now() + within;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = pair.attempt().await;
        if last == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    panic!("{why}: wanted {want}, still getting {last}");
}

fn write_routes(path: &std::path::Path, text: &str) {
    // A temp file and a rename, so hbbs never reads a half-written table.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_attempt_gets_one_relay_and_the_pin_expires() {
    const PORT: i32 = 20216;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    // No routing table: plain round-robin, which on its own hands consecutive
    // attempts different relays.
    let hbbs = start_hbbs(
        PORT,
        "route-pin",
        &[&r1.addr, &r2.addr],
        &[("RELAY_PIN_TTL", "3")],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect(PORT + 2, &hbbs.pk).await;

    let first = pair.attempt().await;
    let second = pair.attempt().await;
    assert!(
        first == r1.addr || first == r2.addr,
        "handed {first}, which is neither relay"
    );
    assert_eq!(
        first, second,
        "two attempts by the same pair, one moment apart, were handed different relays"
    );

    // The pin is a window around one attempt, not a permanent assignment: after
    // it lapses, round-robin resumes and the other relay comes up.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let later = pair.attempt().await;
    assert_ne!(
        later, first,
        "the pin outlived its TTL: round-robin never resumed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_routing_table_picks_the_nearest_relay_and_edits_take_effect() {
    const PORT: i32 = 20226;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("routes-edit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let routes = dir.join("routes.txt");
    // Both ends are near r2.
    write_routes(
        &routes,
        &format!("10.40.0.0/24  {}=60,{}=10\n", r1.addr, r2.addr),
    );
    let hbbs = start_hbbs(
        PORT,
        "route-table",
        &[&r1.addr, &r2.addr],
        &[("RELAY_ROUTES", routes.to_str().unwrap())],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect(PORT + 2, &hbbs.pk).await;

    // Round-robin would start with r1 and alternate; the table says r2, always.
    for n in 1..=3 {
        assert_eq!(
            pair.attempt().await,
            r2.addr,
            "attempt {n} ignored the table"
        );
    }
    assert!(
        hbbs.log().contains("relay routes"),
        "the table's loading was not logged:\n{}",
        hbbs.log()
    );

    // Edit the file: now r1 is nearer. A change applies to new attempts, within a
    // few seconds, with no restart.
    write_routes(
        &routes,
        &format!("10.40.0.0/24  {}=10,{}=60\n", r1.addr, r2.addr),
    );
    settle_on(
        &mut pair,
        &r1.addr,
        Duration::from_secs(15),
        "the edited table",
    )
    .await;
    for n in 1..=3 {
        assert_eq!(pair.attempt().await, r1.addr, "attempt {n} after the edit");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_stops_answering_is_not_chosen_whatever_the_table_says() {
    const PORT: i32 = 20236;
    // Three relays, not two: with a single relay left the picker returns it
    // without looking at any pin, so a test with two cannot tell whether a pin
    // checks health. With r1 gone, r2 and r3 remain and the pin must give way.
    let (mut r1, r2, r3) = (
        FakeRelay::start().await,
        FakeRelay::start().await,
        FakeRelay::start().await,
    );
    let dir = std::env::temp_dir().join(format!("routes-health-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let routes = dir.join("routes.txt");
    write_routes(
        &routes,
        &format!(
            "10.40.0.0/24  {}=5,{}=80,{}=200\n",
            r1.addr, r2.addr, r3.addr
        ),
    );
    let hbbs = start_hbbs(
        PORT,
        "route-health",
        &[&r1.addr, &r2.addr, &r3.addr],
        &[("RELAY_ROUTES", routes.to_str().unwrap())],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect(PORT + 2, &hbbs.pk).await;

    assert_eq!(pair.attempt().await, r1.addr, "the table prefers r1");
    assert_eq!(pair.attempt().await, r1.addr, "and the pin holds it");

    // r1 goes down. hbbs checks relay health every 3 s; a held choice must not
    // keep handing out a relay that has stopped answering, and the next best,
    // r2 and not r3, must take over.
    r1.stop();
    settle_on(&mut pair, &r2.addr, Duration::from_secs(15), "r1 is down").await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_table_never_stops_connections_and_a_fixed_one_is_picked_up() {
    const PORT: i32 = 20246;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("routes-broken-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let routes = dir.join("routes.txt");
    write_routes(&routes, "this is not a routing table\n");
    let hbbs = start_hbbs(
        PORT,
        "route-broken",
        &[&r1.addr, &r2.addr],
        &[("RELAY_ROUTES", routes.to_str().unwrap())],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect(PORT + 2, &hbbs.pk).await;

    // Falls back to round-robin: some healthy relay, and the server is up.
    let got = pair.attempt().await;
    assert!(got == r1.addr || got == r2.addr, "handed {got}");
    let log = hbbs.log();
    assert!(
        log.contains("relay routes") && log.contains("line 1"),
        "the broken table was not reported with its line number:\n{log}"
    );

    write_routes(
        &routes,
        &format!("10.40.0.0/24  {}=90,{}=10\n", r1.addr, r2.addr),
    );
    settle_on(
        &mut pair,
        &r2.addr,
        Duration::from_secs(15),
        "the repaired table",
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_relay_explains_a_decision_without_making_one() {
    const PORT: i32 = 20256;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("routes-dry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let routes = dir.join("routes.txt");
    write_routes(
        &routes,
        &format!("10.40.0.0/24  {}=70,{}=20\n", r1.addr, r2.addr),
    );
    let hbbs = start_hbbs(
        PORT,
        "route-dry",
        &[&r1.addr, &r2.addr],
        &[("RELAY_ROUTES", routes.to_str().unwrap())],
    );
    wait_for_port(PORT + 2).await;

    // The console answers loopback only, on the NAT-test port (ID port - 1).
    let ask = |cmd: &'static str| async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", (PORT - 1) as u16))
            .await
            .expect("console");
        s.write_all(cmd.as_bytes()).await.unwrap();
        let mut out = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_string(&mut out)).await;
        out
    };
    let said = ask("test-relay 10.40.0.1 10.40.0.2").await;
    assert!(said.contains(&r2.addr), "named the wrong relay: {said}");
    assert!(
        said.contains(&format!("{}=40", r2.addr)) && said.contains(&format!("{}=140", r1.addr)),
        "did not show each relay's total cost: {said}"
    );

    // And it pinned nothing: a real attempt now still gets the table's answer.
    let mut pair = Pair::connect(PORT + 2, &hbbs.pk).await;
    assert_eq!(pair.attempt().await, r2.addr);
    let _ = std::fs::remove_dir_all(&dir);
    drop(hbbs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn healthy_relays_stay_in_the_order_they_were_configured() {
    const PORT: i32 = 20266;
    // Order is the tie-break, and the only way to say "prefer this one", so it
    // must survive the health check, which probes every relay concurrently and
    // would otherwise return them in whatever order the network finishes.
    let relays = vec![
        FakeRelay::start().await,
        FakeRelay::start().await,
        FakeRelay::start().await,
        FakeRelay::start().await,
    ];
    let addrs: Vec<&str> = relays.iter().map(|r| r.addr.as_str()).collect();
    let _hbbs = start_hbbs(PORT, "route-order", &addrs, &[]);
    wait_for_port(PORT + 2).await;

    let live = || async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", (PORT - 1) as u16))
            .await
            .expect("console");
        s.write_all(b"relay-servers").await.unwrap();
        let mut out = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_string(&mut out)).await;
        out.lines().map(str::to_owned).collect::<Vec<_>>()
    };
    // Several health checks apart: one lucky ordering must not pass this.
    for round in 1..=3 {
        tokio::time::sleep(Duration::from_millis(3300)).await;
        assert_eq!(
            live().await,
            addrs,
            "after health check {round} the relays were reordered"
        );
    }
}
