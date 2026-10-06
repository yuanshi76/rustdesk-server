//! Keeping the GeoIP database current.
//!
//! DB-IP publishes "IP to City Lite" once a month at
//! `<base>/dbip-city-lite-YYYY-MM.mmdb.gz`. `update` fetches the newest one this
//! machine does not have yet, checks that it really is a City database, and renames
//! it over the old file, so `hbbs` (which re-reads the file when it changes) never
//! sees a half-written one. Any failure leaves the old file in place.
//!
//! What was fetched is recorded next to the database, in `<db>.version`, as `YYYY-MM`,
//! so a day-to-day check costs nothing once the month's file is installed and
//! nothing is downloaded twice.

use crate::relay_geo::GeoDb;
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const DEFAULT_BASE_URL: &str = "https://download.db-ip.com/free";
/// The compressed City Lite file is about 60 MB and the unpacked one about 127 MB.
/// These caps are far above that and exist so that a broken or hostile server cannot
/// fill the disk.
pub const MAX_DOWNLOAD_BYTES: u64 = 300 << 20;
pub const MAX_UNPACKED_BYTES: u64 = 1 << 30;
/// The real file has millions of nodes; anything tiny is not a usable City database.
pub const MIN_NODES: u32 = 100_000;

pub struct Options {
    pub out: PathBuf,
    pub base_url: String,
    /// Today as (year, month), UTC. A parameter so that it can be tested.
    pub now: (i32, u32),
    /// Fetch even if this month's file is already installed.
    pub force: bool,
    pub max_download_bytes: u64,
    pub max_unpacked_bytes: u64,
    pub min_nodes: u32,
}

impl Options {
    pub fn new(out: PathBuf, base_url: String, now: (i32, u32)) -> Self {
        Self {
            out,
            base_url,
            now,
            force: false,
            max_download_bytes: MAX_DOWNLOAD_BYTES,
            max_unpacked_bytes: MAX_UNPACKED_BYTES,
            min_nodes: MIN_NODES,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A new file was installed; this is its month.
    Updated(String),
    /// Nothing newer is available than the file already installed (its month).
    UpToDate(String),
}

fn label((y, m): (i32, u32)) -> String {
    format!("{y:04}-{m:02}")
}

fn previous((y, m): (i32, u32)) -> (i32, u32) {
    if m <= 1 {
        (y - 1, 12)
    } else {
        (y, m - 1)
    }
}

fn version_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".version");
    PathBuf::from(s)
}

/// The month of the installed file, if there is a file and a record of it.
fn installed(out: &Path) -> Option<String> {
    if !out.is_file() {
        return None;
    }
    let text = fs::read_to_string(version_path(out)).ok()?;
    let l = text.trim();
    let ok = l.len() == 7
        && l.as_bytes()[4] == b'-'
        && l[..4].bytes().all(|b| b.is_ascii_digit())
        && l[5..].bytes().all(|b| b.is_ascii_digit());
    ok.then(|| l.to_owned())
}

enum Fetched {
    Installed,
    NotPublished,
}

/// Check once, and install a newer file if there is one. The current month's file is
/// tried first, then the previous month's, because early in a month the new one may
/// not be published yet.
pub fn update(o: &Options) -> Result<Outcome, String> {
    let have = installed(&o.out);
    for month in [o.now, previous(o.now)] {
        let cand = label(month);
        if !o.force {
            if let Some(h) = &have {
                if *h >= cand {
                    return Ok(Outcome::UpToDate(h.clone()));
                }
            }
        }
        match fetch_month(o, &cand)? {
            Fetched::Installed => {
                fs::write(version_path(&o.out), format!("{cand}\n"))
                    .map_err(|e| format!("installed {cand} but could not record it: {e}"))?;
                return Ok(Outcome::Updated(cand));
            }
            Fetched::NotPublished => continue,
        }
    }
    match have {
        Some(h) => Ok(Outcome::UpToDate(h)),
        None => Err(format!(
            "neither {} nor {} is published at {}",
            label(o.now),
            label(previous(o.now)),
            o.base_url
        )),
    }
}

/// Removes the half-written file unless it was renamed into place.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Stops with an error once more than `left` bytes have passed through.
struct Capped<R> {
    inner: R,
    left: u64,
    what: &'static str,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{} is larger than the limit", self.what),
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

fn fetch_month(o: &Options, month: &str) -> Result<Fetched, String> {
    let url = format!(
        "{}/dbip-city-lite-{month}.mmdb.gz",
        o.base_url.trim_end_matches('/')
    );
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(1800))
        .user_agent(concat!("rustdesk-utils/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("cannot start an HTTP client: {e}"))?;
    let resp = client.get(&url).send().map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(Fetched::NotPublished);
    }
    if !status.is_success() {
        return Err(format!("{url}: HTTP {status}"));
    }
    if let Some(len) = resp.content_length() {
        if len > o.max_download_bytes {
            return Err(format!(
                "{url}: {len} bytes is larger than the {} byte limit",
                o.max_download_bytes
            ));
        }
    }

    let tmp_path = {
        let mut s = o.out.as_os_str().to_owned();
        s.push(format!(".{}.tmp", std::process::id()));
        PathBuf::from(s)
    };
    let tmp = TempFile(tmp_path);
    {
        let compressed = Capped {
            inner: resp,
            left: o.max_download_bytes,
            what: "the download",
        };
        let mut unpacked = Capped {
            inner: flate2::read::GzDecoder::new(compressed),
            left: o.max_unpacked_bytes,
            what: "the unpacked file",
        };
        let mut file = fs::File::create(&tmp.0)
            .map_err(|e| format!("cannot write {}: {e}", tmp.0.display()))?;
        io::copy(&mut unpacked, &mut file).map_err(|e| format!("{url}: {e}"))?;
        file.flush().map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    GeoDb::validate_city(&tmp.0, o.min_nodes).map_err(|e| format!("{url}: {e}"))?;
    fs::rename(&tmp.0, &o.out).map_err(|e| format!("cannot replace {}: {e}", o.out.display()))?;
    // Renamed: nothing left to remove.
    std::mem::forget(tmp);
    Ok(Fetched::Installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_and_labels() {
        assert_eq!(label((2026, 3)), "2026-03");
        assert_eq!(previous((2026, 3)), (2026, 2));
        assert_eq!(previous((2026, 1)), (2025, 12));
        // Lexicographic order is chronological, which `update` relies on.
        assert!(label((2025, 12)) < label((2026, 1)));
        assert!(label((2026, 9)) < label((2026, 10)));
    }

    #[test]
    fn the_version_record_is_only_trusted_beside_a_file() {
        let dir = std::env::temp_dir().join(format!("geo-version-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("geo.mmdb");
        fs::write(version_path(&db), "2026-10\n").unwrap();
        assert_eq!(
            installed(&db),
            None,
            "a record with no file means no database"
        );
        fs::write(&db, b"x").unwrap();
        assert_eq!(installed(&db), Some("2026-10".to_owned()));
        fs::write(version_path(&db), "garbage").unwrap();
        assert_eq!(installed(&db), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
