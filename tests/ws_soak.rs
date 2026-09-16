//! Soak test: run hbbs as a real child process under sustained websocket churn
//! and assert that its resident memory and file descriptor count stay flat.
//!
//! Ignored by default because it is slow. Run it with:
//!
//!     cargo test --release --test ws_soak -- --ignored --nocapture
//!
//! Tunables (environment): SOAK_MINUTES (default 60), SOAK_CONCURRENCY
//! (default 10), SOAK_RECONNECT_MS (default 7000), SOAK_SAMPLE_SEC (default 30).
//!
//! Every connection registers its public key, because that is what a real
//! client does on a fresh websocket and it is the path the leak was on - the
//! fork stashed the write half on a successful RegisterPk and nowhere else.
//! A soak that only heartbeats never touches it and would pass against the
//! leaking build, which is worth nothing.
//!
//! Hence the 7 second reconnect interval: hbbs refuses more than three
//! RegisterPk from one peer in six seconds, so anything faster is answered with
//! TOO_FREQUENT and registers nothing. Ten clients at 7 s is about 5000
//! connections an hour, against ~2300 sockets/day in production, so a leak of
//! the same shape shows up in minutes rather than days.

mod support;

use hbb_common::{rendezvous_proto::*, tokio};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use support::*;

const PORT: i32 = 37116;
const WS_PORT: i32 = PORT + 2;

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn rss_kb(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn fd_count(pid: u32) -> usize {
    let proc_fd = format!("/proc/{pid}/fd");
    if let Ok(entries) = std::fs::read_dir(&proc_fd) {
        return entries.count();
    }
    // macOS
    let out = Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .expect("lsof");
    String::from_utf8_lossy(&out.stdout).lines().skip(1).count()
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_hbbs(dir: &std::path::Path) -> Server {
    let child = Command::new(env!("CARGO_BIN_EXE_hbbs"))
        .current_dir(dir)
        .args(["-p", &PORT.to_string()])
        .env("TEST-HBBS", "no")
        .env("DB-URL", dir.join("soak.sqlite3"))
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("failed to start hbbs");
    Server(child)
}

/// One client's worth of churn: connect, register, heartbeat, close, repeat.
///
/// Registering on every connection is the point - see the note at the top of
/// the file. The id pool is fixed, so the peer table does not grow and any
/// memory growth is the leak rather than legitimate bookkeeping.
async fn churn(client: usize, reconnect: Duration, until: Instant) -> u64 {
    use hbb_common::futures_util::SinkExt;
    let mut connections = 0u64;
    let id = format!("soak-{client:03}");

    while Instant::now() < until {
        // A distinct source address per connection, which is what hbbs sees in
        // production: every reconnect arrives from a fresh ephemeral port, so
        // every connection is a distinct key in the peer registry. Reusing one
        // address per client would make each registration *replace* the
        // previous entry and free the socket it held, hiding the leak. It also
        // keeps clear of the limiter that blocks an IP after 30 registrations
        // in 60 seconds.
        //
        // The peer id pool stays fixed, so the peer table does not grow.
        let n = connections;
        let ip = format!("10.{}.{}.{}", client % 256, (n / 256) % 256, n % 256);
        let mut ws = connect(WS_PORT, Some(&ip)).await;
        register_pk(&mut ws, &id).await;

        // Fire and forget: the reply is not waited for, both because a real
        // client does not block on its heartbeat and because the leaking build
        // cannot answer at all - having handed its write half to the registry,
        // it has nothing left to reply with. Waiting here would make this test
        // die against the very build it is supposed to catch.
        let mut msg = RendezvousMessage::new();
        msg.set_register_peer(RegisterPeer {
            id: id.clone(),
            serial: 0,
            ..Default::default()
        });
        send(&mut ws, msg).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        ws.close(None).await.ok();
        drop(ws);
        connections += 1;
        tokio::time::sleep(reconnect).await;
    }
    connections
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow: run explicitly with --ignored"]
async fn resident_memory_and_descriptors_stay_flat() {
    let minutes: u64 = env_or("SOAK_MINUTES", 60);
    let clients: usize = env_or("SOAK_CONCURRENCY", 10);
    let reconnect = Duration::from_millis(env_or("SOAK_RECONNECT_MS", 7000));
    let sample_every = Duration::from_secs(env_or("SOAK_SAMPLE_SEC", 30));

    let dir = std::env::temp_dir().join(format!("hbbs-soak-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server = spawn_hbbs(&dir);
    let pid = server.0.id();
    wait_for_port(WS_PORT).await;
    // Let startup settle before the first sample.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let until = Instant::now() + Duration::from_secs(minutes * 60);
    let mut drivers = Vec::new();
    for c in 0..clients {
        drivers.push(tokio::spawn(churn(c, reconnect, until)));
    }

    println!("elapsed_s,rss_kb,fds");
    let mut samples: Vec<(u64, u64, usize)> = Vec::new();
    let started = Instant::now();
    while Instant::now() < until {
        tokio::time::sleep(sample_every).await;
        let elapsed = started.elapsed().as_secs();
        let (rss, fds) = (rss_kb(pid), fd_count(pid));
        println!("{elapsed},{rss},{fds}");
        samples.push((elapsed, rss, fds));
    }

    let mut connections = 0u64;
    for d in drivers {
        connections += d.await.unwrap_or(0);
    }
    println!("total connections: {connections}");
    assert!(
        connections >= 10,
        "the churn drivers barely ran ({connections} connections); \
         nothing was measured"
    );
    assert!(samples.len() >= 4, "not enough samples to judge a trend");

    // Compare the first quarter of the run with the last quarter. A leak of the
    // shape we are guarding against grows without bound, so any real leak shows
    // up as a large difference here however the run is sliced.
    let q = samples.len() / 4;
    let mean = |s: &[(u64, u64, usize)], f: fn(&(u64, u64, usize)) -> f64| {
        s.iter().map(f).sum::<f64>() / s.len() as f64
    };
    let first_rss = mean(&samples[..q], |s| s.1 as f64);
    let last_rss = mean(&samples[samples.len() - q..], |s| s.1 as f64);
    let first_fds = mean(&samples[..q], |s| s.2 as f64);
    let last_fds = mean(&samples[samples.len() - q..], |s| s.2 as f64);

    println!("rss  first quarter {first_rss:.0} kB -> last quarter {last_rss:.0} kB");
    println!("fds  first quarter {first_fds:.1}    -> last quarter {last_fds:.1}");

    assert!(
        last_fds <= first_fds + 10.0,
        "descriptor count grew from {first_fds:.1} to {last_fds:.1} over {minutes} min \
         and {connections} connections"
    );
    assert!(
        last_rss <= first_rss * 1.5 + 4096.0,
        "resident memory grew from {first_rss:.0} kB to {last_rss:.0} kB over {minutes} min \
         and {connections} connections"
    );
}
