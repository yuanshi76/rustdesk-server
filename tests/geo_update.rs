//! The monthly GeoIP database update, against a local HTTP server standing in for
//! DB-IP. The rules under test: the newest published month is fetched, a month already
//! installed is not fetched again, and nothing that goes wrong ever damages the file
//! that is in use.

mod support;

use hbbs::geo_update::{update, Options, Outcome};
use std::path::{Path, PathBuf};
use support::FileServer;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/geo-test.mmdb");

fn gz(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

fn fixture_gz() -> Vec<u8> {
    gz(&std::fs::read(FIXTURE).unwrap())
}

fn path_for(month: &str) -> String {
    format!("/dbip-city-lite-{month}.mmdb.gz")
}

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("geo-update-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
    fn db(&self) -> PathBuf {
        self.0.join("geo.mmdb")
    }
    /// Everything in the directory, to see that nothing is left behind.
    fn files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The fixture is a toy, so the "real databases are big" check is relaxed for it.
fn options(out: &Path, server: &FileServer, now: (i32, u32)) -> Options {
    let mut o = Options::new(out.to_owned(), server.base.clone(), now);
    o.min_nodes = 1;
    o
}

#[test]
fn the_previous_month_is_used_when_the_new_one_is_not_published_yet() {
    let dir = Dir::new("fallback");
    let server = FileServer::start();
    server.serve(&path_for("2026-09"), 200, fixture_gz());
    // 2026-10 is not served: 404, as in the first days of a month.
    let got = update(&options(&dir.db(), &server, (2026, 10))).unwrap();
    assert_eq!(got, Outcome::Updated("2026-09".to_owned()));
    assert_eq!(
        std::fs::read(dir.db()).unwrap(),
        std::fs::read(FIXTURE).unwrap()
    );
    assert_eq!(dir.files(), ["geo.mmdb", "geo.mmdb.version"]);
    assert_eq!(
        *server.requests.lock().unwrap(),
        [path_for("2026-10"), path_for("2026-09")]
    );
}

#[test]
fn a_month_already_installed_costs_no_request_and_a_new_month_is_fetched_once() {
    let dir = Dir::new("monthly");
    let server = FileServer::start();
    server.serve(&path_for("2026-10"), 200, fixture_gz());
    assert_eq!(
        update(&options(&dir.db(), &server, (2026, 10))).unwrap(),
        Outcome::Updated("2026-10".to_owned())
    );
    let after_first = server.request_count();

    // Every later day of the month: nothing to do, and no traffic.
    for _ in 0..3 {
        assert_eq!(
            update(&options(&dir.db(), &server, (2026, 10))).unwrap(),
            Outcome::UpToDate("2026-10".to_owned())
        );
    }
    assert_eq!(
        server.request_count(),
        after_first,
        "an up-to-date check went to the network"
    );

    // The first day of November, before DB-IP has published it: one cheap request
    // for November, which is not there, and nothing else.
    assert_eq!(
        update(&options(&dir.db(), &server, (2026, 11))).unwrap(),
        Outcome::UpToDate("2026-10".to_owned())
    );
    assert_eq!(server.request_count(), after_first + 1);

    // Once November is published, it is fetched.
    server.serve(&path_for("2026-11"), 200, fixture_gz());
    assert_eq!(
        update(&options(&dir.db(), &server, (2026, 11))).unwrap(),
        Outcome::Updated("2026-11".to_owned())
    );
    // And the year turns over correctly.
    assert_eq!(
        update(&options(&dir.db(), &server, (2027, 1))).unwrap(),
        Outcome::UpToDate("2026-11".to_owned())
    );
    assert!(server
        .requests
        .lock()
        .unwrap()
        .contains(&path_for("2027-01")));
}

#[test]
fn nothing_published_and_nothing_installed_is_an_error_not_a_silent_success() {
    let dir = Dir::new("nothing");
    let server = FileServer::start();
    let err = update(&options(&dir.db(), &server, (2026, 10))).unwrap_err();
    assert!(err.contains("2026-10") && err.contains("2026-09"), "{err}");
    assert_eq!(dir.files(), Vec::<String>::new());
}

#[test]
fn nothing_that_goes_wrong_damages_the_database_in_use() {
    let dir = Dir::new("damage");
    let server = FileServer::start();
    // A good database is installed.
    server.serve(&path_for("2026-09"), 200, fixture_gz());
    update(&options(&dir.db(), &server, (2026, 9))).unwrap();
    let good = std::fs::read(dir.db()).unwrap();
    let files_before = dir.files();

    let fixture = std::fs::read(FIXTURE).unwrap();
    let mut truncated = fixture_gz();
    truncated.truncate(truncated.len() / 2);
    let cases: Vec<(&str, u16, Vec<u8>)> = vec![
        ("server error", 500, b"oops".to_vec()),
        (
            "not gzip at all",
            200,
            b"<html>captive portal</html>".to_vec(),
        ),
        (
            "gzip of something that is not a database",
            200,
            gz(b"hello, not a database"),
        ),
        ("gzip cut short", 200, truncated),
        (
            "a database of the wrong kind",
            200,
            gz(&fixture[..fixture.len() / 2]),
        ),
    ];
    for (name, status, body) in cases {
        server.serve(&path_for("2026-10"), status, body);
        let err = update(&options(&dir.db(), &server, (2026, 10)));
        assert!(err.is_err(), "{name}: expected an error, got {err:?}");
        assert_eq!(
            std::fs::read(dir.db()).unwrap(),
            good,
            "{name}: the database changed"
        );
        assert_eq!(dir.files(), files_before, "{name}: left files behind");
    }
    // The record still says 2026-09, so the next day tries again.
    assert_eq!(
        std::fs::read_to_string(dir.0.join("geo.mmdb.version"))
            .unwrap()
            .trim(),
        "2026-09"
    );
}

#[test]
fn a_toy_database_is_refused_by_default() {
    // The default demands a database as big as a real one; the 1 KB fixture is not.
    let dir = Dir::new("toy");
    let server = FileServer::start();
    server.serve(&path_for("2026-10"), 200, fixture_gz());
    let o = Options::new(dir.db(), server.base.clone(), (2026, 10));
    let err = update(&o).unwrap_err();
    assert!(err.contains("nodes"), "{err}");
    assert_eq!(dir.files(), Vec::<String>::new());
}

#[test]
fn a_download_larger_than_the_limit_is_stopped_and_cleaned_up() {
    let dir = Dir::new("big");
    let server = FileServer::start();
    server.serve(&path_for("2026-10"), 200, fixture_gz());
    let mut o = options(&dir.db(), &server, (2026, 10));
    o.max_unpacked_bytes = 100; // the fixture unpacks to about 1.3 KB
    let err = update(&o).unwrap_err();
    assert!(err.contains("limit"), "{err}");
    assert_eq!(dir.files(), Vec::<String>::new());
    let mut o = options(&dir.db(), &server, (2026, 10));
    o.max_download_bytes = 10;
    assert!(update(&o).is_err());
    assert_eq!(dir.files(), Vec::<String>::new());
}

#[test]
fn force_fetches_again_even_when_the_month_is_installed() {
    let dir = Dir::new("force");
    let server = FileServer::start();
    server.serve(&path_for("2026-10"), 200, fixture_gz());
    update(&options(&dir.db(), &server, (2026, 10))).unwrap();
    let mut o = options(&dir.db(), &server, (2026, 10));
    o.force = true;
    assert_eq!(update(&o).unwrap(), Outcome::Updated("2026-10".to_owned()));
    assert_eq!(server.request_count(), 2);
}

#[test]
fn a_database_put_there_by_hand_is_replaced_by_the_first_update() {
    // No version record means we do not know what it is; auto-update was asked for, so
    // the current month's file takes its place.
    let dir = Dir::new("manual");
    std::fs::write(dir.db(), b"whatever was copied here").unwrap();
    let server = FileServer::start();
    server.serve(&path_for("2026-10"), 200, fixture_gz());
    assert_eq!(
        update(&options(&dir.db(), &server, (2026, 10))).unwrap(),
        Outcome::Updated("2026-10".to_owned())
    );
}
