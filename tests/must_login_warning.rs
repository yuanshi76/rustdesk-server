//! MUST_LOGIN without RUSTDESK_API_JWT_KEY accepts any non-empty token. That is
//! not changed - refusing to start would break setups that run this way - but it
//! must not be silent. Starts the real binary and reads its log.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Everything hbbs logs up to and including its "Start" line, which comes after
/// the MUST_LOGIN handling.
fn startup_log(port: i32, name: &str, jwt_key: Option<&str>) -> String {
    let dir = std::env::temp_dir().join(format!("hbbs-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hbbs"));
    cmd.current_dir(&dir)
        .args(["-p", &port.to_string()])
        .env("TEST-HBBS", "no")
        .env("DB-URL", dir.join("hbbs.sqlite3"))
        .env("MUST_LOGIN", "Y")
        .env_remove("RUSTDESK_API_JWT_KEY")
        .stdout(Stdio::piped());
    if let Some(k) = jwt_key {
        cmd.env("RUSTDESK_API_JWT_KEY", k);
    }
    let mut child = cmd.spawn().expect("hbbs");
    let stdout = child.stdout.take().expect("piped stdout");
    let _guard = Proc(child);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let done = line.contains("] INFO ") && line.ends_with("Start");
            if tx.send(line).is_err() || done {
                return;
            }
        }
    });

    let mut log = String::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(20)) {
        log.push_str(&line);
        log.push('\n');
        if line.ends_with("Start") {
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        log.contains("MUST_LOGIN=Y"),
        "hbbs did not report MUST_LOGIN:\n{log}"
    );
    log
}

#[test]
fn warns_when_tokens_cannot_be_verified() {
    let log = startup_log(20196, "login-warn", None);
    assert!(
        log.contains("any non-empty token is accepted"),
        "no warning that MUST_LOGIN without a key accepts any token:\n{log}"
    );
}

#[test]
fn says_nothing_when_a_key_is_set() {
    let log = startup_log(20206, "login-key", Some("a-shared-secret"));
    assert!(
        !log.contains("any non-empty token is accepted"),
        "warned although RUSTDESK_API_JWT_KEY is set:\n{log}"
    );
}
