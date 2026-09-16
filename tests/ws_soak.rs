//! Soak test: run hbbs as a real child process under sustained websocket churn
//! and assert that its resident memory and file descriptor count stay flat.
//!
//! Ignored by default because it is slow. Run it with:
//!
//!     cargo test --release --test ws_soak -- --ignored --nocapture
//!
//! Tunables (environment): SOAK_MINUTES (default 60), SOAK_CONCURRENCY
//! (default 5, matching the five clients on the production host),
//! SOAK_RECONNECT_MS (default 1000), SOAK_SAMPLE_SEC (default 30).
//!
//! The production leak was ~2300 sockets/day. At the defaults this test opens
//! about 18000 connections an hour, so a leak of the same shape shows up within
//! minutes rather than days.

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

/// One client's worth of churn: connect, heartbeat, close, repeat.
///
/// The public key is registered once, on the first connection, and every
/// connection after that only heartbeats - which is what a real client does,
/// and which also stays clear of the anti-abuse limiter that refuses more than
/// three RegisterPk from one peer in six seconds. Keeping the id pool fixed
/// keeps the peer table from growing, so any memory growth is the leak and not
/// legitimate bookkeeping.
async fn churn(client: usize, reconnect: Duration, until: Instant) -> u64 {
    use hbb_common::futures_util::SinkExt;
    let mut connections = 0u64;
    let id = format!("soak-{client:03}");
    let ip = format!("10.9.{}.{}", client / 256 % 256, client % 256);

    let mut ws = connect(WS_PORT, Some(&ip)).await;
    register_pk(&mut ws, &id).await;
    ws.close(None).await.ok();
    drop(ws);
    connections += 1;

    while Instant::now() < until {
        let mut ws = connect(WS_PORT, Some(&ip)).await;
        let mut msg = RendezvousMessage::new();
        msg.set_register_peer(RegisterPeer {
            id: id.clone(),
            serial: 0,
            ..Default::default()
        });
        send(&mut ws, msg).await;
        let _ = recv(&mut ws, "RegisterPeerResponse").await;

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
    let clients: usize = env_or("SOAK_CONCURRENCY", 5);
    let reconnect = Duration::from_millis(env_or("SOAK_RECONNECT_MS", 1000));
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
