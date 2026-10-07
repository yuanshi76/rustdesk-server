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
    /// Is the process still running?
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn log(&self) -> String {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

/// The two peers of a session, both reachable over websocket as a 1.4.1+ client is.
struct Pair {
    a: Ws,
    b: Ws,
    pk: String,
    /// The controlled peer's ID, which differs per pair so that pairs on one hbbs
    /// do not collide.
    id: String,
}

impl Pair {
    async fn connect(ws_port: i32, pk: &str) -> Self {
        Self::connect_from(ws_port, pk, A_IP, B_IP).await
    }

    /// A pair whose two ends appear to hbbs to come from these addresses.
    async fn connect_from(ws_port: i32, pk: &str, a_ip: &str, b_ip: &str) -> Self {
        let id = format!("route-{b_ip}");
        let mut b = connect(ws_port, Some(b_ip)).await;
        register_pk(&mut b, &id).await;
        let a = connect(ws_port, Some(a_ip)).await;
        Self {
            a,
            b,
            pk: pk.to_owned(),
            id,
        }
    }

    /// One connection attempt: A asks for B, and the relay hbbs put in the
    /// PunchHole it pushed to B is the one it chose.
    async fn attempt(&mut self) -> String {
        let mut msg = RendezvousMessage::new();
        msg.set_punch_hole_request(PunchHoleRequest {
            id: self.id.clone(),
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

/// One console command to hbbs, which answers loopback only, on ID port - 1.
async fn console(port: i32, cmd: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", (port - 1) as u16))
        .await
        .expect("console");
    s.write_all(cmd.as_bytes()).await.unwrap();
    let mut out = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_string(&mut out)).await;
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn use_relay_sends_every_session_to_one_relay_and_survives_a_restart() {
    const PORT: i32 = 20276;
    const PORT2: i32 = 20286;
    let (r1, r2, r3) = (
        FakeRelay::start().await,
        FakeRelay::start().await,
        FakeRelay::start().await,
    );
    let dir = std::env::temp_dir().join(format!("prefer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("relay_preferred");
    let env = [
        ("RELAY_PIN_TTL", "0"), // every attempt is a fresh choice
        ("RELAY_PREFERRED_FILE", file.to_str().unwrap()),
    ];
    let addrs = [r1.addr.as_str(), r2.addr.as_str(), r3.addr.as_str()];

    let hbbs = start_hbbs(PORT, "prefer-1", &addrs, &env);
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect(PORT + 2, &hbbs.pk).await;

    // Control: with nothing chosen, round-robin spreads attempts over the relays.
    let mut seen = std::collections::HashSet::new();
    for _ in 0..6 {
        seen.insert(pair.attempt().await);
    }
    assert!(seen.len() > 1, "round-robin never changed relay: {seen:?}");
    assert!(console(PORT, "use-relay").await.contains("none"));

    // Choose r3: every attempt from now on goes there, however many.
    let said = console(PORT, &format!("use-relay {}", r3.addr)).await;
    assert!(said.contains(&r3.addr), "{said}");
    for n in 1..=6 {
        assert_eq!(pair.attempt().await, r3.addr, "attempt {n} after use-relay");
    }
    let dry = console(PORT, "test-relay 10.40.0.1 10.40.0.2").await;
    assert!(
        dry.contains(&r3.addr) && dry.contains("preferred"),
        "dry run did not report the choice: {dry}"
    );

    // A relay that is not in the list is refused, and nothing changes.
    let said = console(PORT, "use-relay 127.0.0.1:1").await;
    assert!(said.contains("nothing changed"), "{said}");
    assert_eq!(pair.attempt().await, r3.addr);
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), r3.addr);
    drop(pair);
    drop(hbbs);

    // A new hbbs process, same file: still r3, without being told again.
    let hbbs = start_hbbs(PORT2, "prefer-2", &addrs, &env);
    wait_for_port(PORT2 + 2).await;
    let mut pair = Pair::connect(PORT2 + 2, &hbbs.pk).await;
    for n in 1..=4 {
        assert_eq!(pair.attempt().await, r3.addr, "attempt {n} after a restart");
    }

    // `auto` goes back to spreading, and the file is gone so a restart stays automatic.
    let said = console(PORT2, "use-relay auto").await;
    assert!(said.contains("automatically"), "{said}");
    assert!(!file.exists(), "auto left the file behind");
    let mut seen = std::collections::HashSet::new();
    for _ in 0..6 {
        seen.insert(pair.attempt().await);
    }
    assert!(seen.len() > 1, "auto did not resume round-robin: {seen:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chosen_relay_that_stops_answering_does_not_take_connections_down() {
    const PORT: i32 = 20296;
    let (mut r1, r2, r3) = (
        FakeRelay::start().await,
        FakeRelay::start().await,
        FakeRelay::start().await,
    );
    let hbbs = start_hbbs(
        PORT,
        "prefer-down",
        &[&r1.addr, &r2.addr, &r3.addr],
        &[("RELAY_PIN_TTL", "0")],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect(PORT + 2, &hbbs.pk).await;

    console(PORT, &format!("use-relay {}", r1.addr)).await;
    assert_eq!(pair.attempt().await, r1.addr);

    // r1 dies. After the next health check (every 3 s) sessions must still be
    // placed, on a relay that answers, and the console must say what is happening.
    r1.stop();
    let deadline = Instant::now() + Duration::from_secs(15);
    let got = loop {
        let got = pair.attempt().await;
        if got != r1.addr || Instant::now() > deadline {
            break got;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    };
    assert!(
        got == r2.addr || got == r3.addr,
        "still handed {got} after the chosen relay stopped answering"
    );
    assert!(console(PORT, "use-relay").await.contains("NOT answering"));
}

const GEO_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/geo-test.mmdb");

/// Relay locations for the fixture database: 10.40/16 is Hong Kong, 10.41/16 London,
/// 10.42/16 New York. `near` names the relay at each place.
fn write_locations(path: &std::path::Path, hk: &str, london: &str, ny: &str) {
    write_routes(
        path,
        &format!("{hk} 22.3,114.2\n{london} 51.5,-0.1\n{ny} 40.7,-74.0\n"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_address_in_no_table_is_sent_to_the_relay_nearest_where_it_is_located() {
    const PORT: i32 = 20306;
    let (r1, r2, r3) = (
        FakeRelay::start().await,
        FakeRelay::start().await,
        FakeRelay::start().await,
    );
    let dir = std::env::temp_dir().join(format!("geo-basic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locations = dir.join("locations.txt");
    write_locations(&locations, &r1.addr, &r2.addr, &r3.addr);
    let hbbs = start_hbbs(
        PORT,
        "geo-basic",
        // Listed in an order that is not the geographic one, so that "first in the
        // list" and "round-robin" both give a different answer from the right one.
        &[&r3.addr, &r2.addr, &r1.addr],
        &[
            ("RELAY_PIN_TTL", "0"),
            ("GEO_DB", GEO_DB),
            ("RELAY_LOCATIONS", locations.to_str().unwrap()),
        ],
    );
    wait_for_port(PORT + 2).await;

    for (net, want, place) in [
        ("10.40.0", &r1.addr, "Hong Kong"),
        ("10.41.0", &r2.addr, "London"),
        ("10.42.0", &r3.addr, "New York"),
    ] {
        let mut pair =
            Pair::connect_from(PORT + 2, &hbbs.pk, &format!("{net}.1"), &format!("{net}.2")).await;
        // Several attempts, so that round-robin could not match by luck.
        for n in 1..=4 {
            assert_eq!(
                pair.attempt().await,
                *want,
                "attempt {n} for two devices in {place}"
            );
        }
    }

    let said = console(PORT, "test-relay 10.41.0.1 10.41.0.2").await;
    assert!(
        said.contains("(geo)") && said.contains("located 10.41.0.1: 51.50,-0.10"),
        "the dry run did not explain the location: {said}"
    );
    // An address the database does not know, with no table: the ordinary policy.
    let said = console(PORT, "test-relay 8.8.8.8 1.1.1.1").await;
    assert!(said.contains("round-robin"), "{said}");
    let said = console(PORT, "relay-routes").await;
    assert!(
        said.contains("geo database") && said.contains("(loaded)") && said.contains("3 relays"),
        "{said}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_routing_line_beats_the_location_for_its_own_end_and_the_two_ends_combine() {
    const PORT: i32 = 20316;
    let (r1, r2, r3) = (
        FakeRelay::start().await,
        FakeRelay::start().await,
        FakeRelay::start().await,
    );
    let dir = std::env::temp_dir().join(format!("geo-mixed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locations = dir.join("locations.txt");
    write_locations(&locations, &r1.addr, &r2.addr, &r3.addr);
    // The A end, which the database places in Hong Kong, has a line of its own that
    // says London is its best relay.
    let routes = dir.join("routes.txt");
    write_routes(
        &routes,
        &format!(
            "10.40.0.0/24 {}=300,{}=10,{}=300\n",
            r1.addr, r2.addr, r3.addr
        ),
    );
    let env = [
        ("RELAY_PIN_TTL", "0"),
        ("GEO_DB", GEO_DB),
        ("RELAY_LOCATIONS", locations.to_str().unwrap()),
        ("RELAY_ROUTES", routes.to_str().unwrap()),
    ];
    let hbbs = start_hbbs(PORT, "geo-mixed", &[&r1.addr, &r2.addr, &r3.addr], &env);
    wait_for_port(PORT + 2).await;

    // A: 10.40.0.1 (table), B: 10.42.0.1 (New York by location).
    // Table + location: r1 = 300+259, r2 = 10+111, r3 = 300+0, so London.
    // Location alone would say Hong Kong (r1 = 0+259) tied with New York (259) and
    // take the first listed, which is r1; so London shows the line was used.
    let said = console(PORT, "test-relay 10.40.0.1 10.42.0.1").await;
    assert!(
        said.contains(&format!("relay: {} (routes+geo)", r2.addr)),
        "{said}"
    );
    let mut pair = Pair::connect_from(PORT + 2, &hbbs.pk, "10.40.0.1", "10.42.0.1").await;
    for n in 1..=3 {
        assert_eq!(pair.attempt().await, r2.addr, "attempt {n}");
    }
    // The line covers only 10.40.0.0/24: another Hong Kong address has no line, so
    // it is placed by location and goes to the Hong Kong relay.
    let mut other = Pair::connect_from(PORT + 2, &hbbs.pk, "10.40.9.1", "10.40.9.2").await;
    assert_eq!(other.attempt().await, r1.addr);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edited_locations_apply_without_a_restart_and_a_broken_file_changes_nothing() {
    const PORT: i32 = 20326;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("geo-edit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locations = dir.join("locations.txt");
    // r1 is in Hong Kong and r2 in London; only these two relays are in use.
    write_routes(
        &locations,
        &format!("{} 22.3,114.2\n{} 51.5,-0.1\n", r1.addr, r2.addr),
    );
    let hbbs = start_hbbs(
        PORT,
        "geo-edit",
        &[&r1.addr, &r2.addr],
        &[
            ("RELAY_PIN_TTL", "0"),
            ("GEO_DB", GEO_DB),
            ("RELAY_LOCATIONS", locations.to_str().unwrap()),
        ],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect_from(PORT + 2, &hbbs.pk, "10.40.0.1", "10.40.0.2").await;
    assert_eq!(
        pair.attempt().await,
        r1.addr,
        "Hong Kong devices, Hong Kong relay"
    );

    // The relays move: now r2 is the Hong Kong one.
    write_routes(
        &locations,
        &format!("{} 51.5,-0.1\n{} 22.3,114.2\n", r1.addr, r2.addr),
    );
    settle_on(
        &mut pair,
        &r2.addr,
        Duration::from_secs(15),
        "the edited locations",
    )
    .await;

    // A file that is not a list of locations is reported and ignored.
    write_routes(&locations, "this is not a list of locations\n");
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert_eq!(
        pair.attempt().await,
        r2.addr,
        "a broken file changed the answer"
    );
    let log = hbbs.log();
    assert!(
        log.contains("relay locations") && log.contains("line 1"),
        "the broken file was not reported with its line number:\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_monthly_database_update_changes_where_devices_are_sent_without_a_restart() {
    const PORT: i32 = 20336;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("geo-monthly-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locations = dir.join("locations.txt");
    // r1 is in Hong Kong, r2 in London.
    write_routes(
        &locations,
        &format!("{} 22.3,114.2\n{} 51.5,-0.1\n", r1.addr, r2.addr),
    );
    // The installed database says 10.40/16 is Hong Kong.
    let db = dir.join("geo.mmdb");
    std::fs::copy(GEO_DB, &db).unwrap();
    let hbbs = start_hbbs(
        PORT,
        "geo-monthly",
        &[&r1.addr, &r2.addr],
        &[
            ("RELAY_PIN_TTL", "0"),
            ("GEO_DB", db.to_str().unwrap()),
            ("RELAY_LOCATIONS", locations.to_str().unwrap()),
        ],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect_from(PORT + 2, &hbbs.pk, "10.40.0.1", "10.40.0.2").await;
    assert_eq!(pair.attempt().await, r1.addr, "before the update");

    // The next month's file, served as DB-IP serves it, says the range is in London.
    let server = support::FileServer::start();
    let swapped = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/geo-test-swapped.mmdb"
    ))
    .unwrap();
    let gz = {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&swapped).unwrap();
        e.finish().unwrap()
    };
    server.serve("/dbip-city-lite-2026-10.mmdb.gz", 200, gz);
    let (out, base) = (db.clone(), server.base.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        let mut o = hbbs::geo_update::Options::new(out, base, (2026, 10));
        o.min_nodes = 1;
        hbbs::geo_update::update(&o)
    })
    .await
    .unwrap();
    assert!(outcome.is_ok(), "{outcome:?}");

    // hbbs picks the new file up on its own and the same devices now go to London.
    settle_on(
        &mut pair,
        &r2.addr,
        Duration::from_secs(15),
        "the updated database",
    )
    .await;
    assert!(
        hbbs.log().contains("geo database: loaded"),
        "the reload was not logged:\n{}",
        hbbs.log()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_download_is_picked_up_by_an_hbbs_that_started_without_a_database() {
    const PORT: i32 = 20346;
    // A fresh container with automatic updates: hbbs starts first and the file does
    // not exist yet. It must say so, keep working, and notice the file when it appears.
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("geo-first-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locations = dir.join("locations.txt");
    write_routes(
        &locations,
        &format!("{} 22.3,114.2\n{} 51.5,-0.1\n", r1.addr, r2.addr),
    );
    let db = dir.join("geo.mmdb");
    let hbbs = start_hbbs(
        PORT,
        "geo-first",
        &[&r1.addr, &r2.addr],
        &[
            ("RELAY_PIN_TTL", "0"),
            ("GEO_DB", db.to_str().unwrap()),
            ("RELAY_LOCATIONS", locations.to_str().unwrap()),
        ],
    );
    wait_for_port(PORT + 2).await;
    let said = console(PORT, "relay-routes").await;
    assert!(said.contains("NOT loaded"), "{said}");
    // Sessions still work, by taking relays in turn.
    let mut pair = Pair::connect_from(PORT + 2, &hbbs.pk, "10.40.0.1", "10.40.0.2").await;
    let got = pair.attempt().await;
    assert!(got == r1.addr || got == r2.addr);

    let server = support::FileServer::start();
    let gz = {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&std::fs::read(GEO_DB).unwrap()).unwrap();
        e.finish().unwrap()
    };
    server.serve("/dbip-city-lite-2026-10.mmdb.gz", 200, gz);
    let (out, base) = (db.clone(), server.base.clone());
    tokio::task::spawn_blocking(move || {
        let mut o = hbbs::geo_update::Options::new(out, base, (2026, 10));
        o.min_nodes = 1;
        hbbs::geo_update::update(&o)
    })
    .await
    .unwrap()
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let said = console(PORT, "relay-routes").await;
        if said.contains("(loaded)") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never loaded the new file: {said}"
        );
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    // Hong Kong devices, from the freshly downloaded database, go to the Hong Kong relay.
    for n in 1..=3 {
        assert_eq!(pair.attempt().await, r1.addr, "attempt {n}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overwriting_the_database_in_place_cannot_crash_hbbs() {
    const PORT: i32 = 20356;
    // The mistake: `curl -o geo.mmdb`, or unpacking straight onto the file, instead
    // of renaming a new file over it. The file is emptied and refilled while hbbs is
    // running. If hbbs read it by mapping the file itself, the next lookup after the
    // truncation would kill the whole server.
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("geo-inplace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locations = dir.join("locations.txt");
    write_routes(
        &locations,
        &format!("{} 22.3,114.2\n{} 51.5,-0.1\n", r1.addr, r2.addr),
    );
    let db = dir.join("geo.mmdb");
    std::fs::copy(GEO_DB, &db).unwrap();
    let mut hbbs = start_hbbs(
        PORT,
        "geo-inplace",
        &[&r1.addr, &r2.addr],
        &[
            ("RELAY_PIN_TTL", "0"),
            ("GEO_DB", db.to_str().unwrap()),
            ("RELAY_LOCATIONS", locations.to_str().unwrap()),
        ],
    );
    wait_for_port(PORT + 2).await;
    let mut pair = Pair::connect_from(PORT + 2, &hbbs.pk, "10.40.0.1", "10.40.0.2").await;
    assert_eq!(pair.attempt().await, r1.addr);

    // Empty the file in place, then keep using hbbs through the moment it is half
    // written and the moment it is whole again.
    std::fs::write(&db, b"").unwrap();
    for n in 1..=5 {
        let got = pair.attempt().await;
        assert!(got == r1.addr || got == r2.addr, "attempt {n}: {got}");
        assert!(
            hbbs.alive(),
            "hbbs died after the database was emptied in place"
        );
    }
    // Refill it in place with a database that says the range is in London.
    let swapped = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/geo-test-swapped.mmdb"
    ))
    .unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        f.write_all(&swapped).unwrap();
    }
    settle_on(
        &mut pair,
        &r2.addr,
        Duration::from_secs(15),
        "the refilled database",
    )
    .await;
    assert!(
        hbbs.alive(),
        "hbbs died after the database was refilled in place"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_with_no_location_is_reported_instead_of_silently_ignored() {
    const PORT: i32 = 20366;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    let dir = std::env::temp_dir().join(format!("geo-missing-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locations = dir.join("locations.txt");
    // r2 is missing, and a relay hbbs has never been told about is listed.
    write_routes(
        &locations,
        &format!("{} 22.3,114.2\n203.0.113.9:21117 51.5,-0.1\n", r1.addr),
    );
    let hbbs = start_hbbs(
        PORT,
        "geo-missing",
        &[&r1.addr, &r2.addr],
        &[
            ("GEO_DB", GEO_DB),
            ("RELAY_LOCATIONS", locations.to_str().unwrap()),
        ],
    );
    wait_for_port(PORT + 2).await;
    let log = hbbs.log();
    assert!(
        log.contains(&format!("relay {} has no entry", r2.addr)),
        "the relay with no location was not reported:\n{log}"
    );
    assert!(
        log.contains("203.0.113.9:21117, which is not in the relay list"),
        "the unused location was not reported:\n{log}"
    );
    let said = console(PORT, "relay-routes").await;
    assert!(
        said.contains("WARNING: no location for") && said.contains(&r2.addr),
        "{said}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_written_with_a_space_or_not_resolving_yet_is_not_lost_and_a_bad_one_is_reported() {
    const PORT: i32 = 20376;
    let (r1, r2) = (FakeRelay::start().await, FakeRelay::start().await);
    // `-r "R1, R2,not-yet.invalid:21117,http://bad"`: a space after a comma, a name that
    // does not resolve (as when DNS fails at the moment hbbs starts), and a URL.
    let second = format!(" {}", r2.addr);
    let hbbs = start_hbbs(
        PORT,
        "relay-list",
        &[
            &r1.addr,
            &second,
            "not-yet.invalid:21117",
            "http://bad.example",
        ],
        &[],
    );
    wait_for_port(PORT + 2).await;
    let log = hbbs.log();
    assert!(
        log.contains(&format!(
            "relay-servers=[\"{}\", \"{}\", \"not-yet.invalid:21117\"]",
            r1.addr, r2.addr
        )),
        "the list hbbs started with is not what was written:\n{log}"
    );
    assert!(
        log.contains("relay-servers: ignoring \"http://bad.example\"") && log.contains("URL"),
        "the bad entry was dropped silently:\n{log}"
    );
    // The health check (every 3 s) leaves the two that answer; the one that does not
    // resolve is not used, and would be as soon as it did.
    tokio::time::sleep(Duration::from_secs(7)).await;
    let live = console(PORT, "relay-servers").await;
    assert_eq!(
        live.lines().collect::<Vec<_>>(),
        [r1.addr.as_str(), r2.addr.as_str()],
        "{live}"
    );
}
