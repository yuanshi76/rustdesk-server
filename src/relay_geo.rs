//! Where a client is, from its address, and so which relay is nearest.
//!
//! Two inputs, both files the operator provides: a GeoIP database (MMDB, such as
//! DB-IP's free "IP to City Lite") that maps an address to an approximate latitude
//! and longitude, and a short list saying where each relay is. The distance between
//! them, turned into a rough number of milliseconds, is used exactly like a routing
//! table entry (see `relay_routes`), for the addresses the table has no line for.

use crate::relay_routes::normalize;
use hbb_common::log;
use std::{collections::HashMap, net::IpAddr, path::Path};

/// Rough conversion from distance to round-trip time: light in fibre covers about
/// 100 km per millisecond of round trip, and real routes are about twice as long as
/// the straight line. It only has to rank relays and be commensurate with the
/// milliseconds in a routing table, not predict a ping.
pub const KM_PER_MS: f64 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub lat: f64,
    pub lon: f64,
}

/// Great-circle distance, kilometres.
pub fn distance_km(a: Point, b: Point) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6371.0;
    let (la, lb) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = lb - la;
    let dlon = (b.lon - a.lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + la.cos() * lb.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

/// Where each relay is. `host:port  latitude,longitude`, one per line.
#[derive(Debug, Clone, Default)]
pub struct Locations {
    /// Normalised relay -> position.
    relays: HashMap<String, Point>,
}

impl Locations {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut relays = HashMap::new();
        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (relay, rest) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("line {n}: expected `host:port  latitude,longitude`"))?;
            let nums: Vec<&str> = rest
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|t| !t.is_empty())
                .collect();
            let [lat, lon] = nums[..] else {
                return Err(format!(
                    "line {n}: {relay} needs exactly a latitude and a longitude, got {rest:?}"
                ));
            };
            let lat: f64 = lat
                .parse()
                .map_err(|_| format!("line {n}: {lat:?} is not a latitude"))?;
            let lon: f64 = lon
                .parse()
                .map_err(|_| format!("line {n}: {lon:?} is not a longitude"))?;
            if !(-90.0..=90.0).contains(&lat) {
                return Err(format!("line {n}: latitude {lat} is outside -90 to 90"));
            }
            if !(-180.0..=180.0).contains(&lon) {
                return Err(format!("line {n}: longitude {lon} is outside -180 to 180"));
            }
            if relays
                .insert(normalize(relay), Point { lat, lon })
                .is_some()
            {
                return Err(format!("line {n}: relay {relay} is listed twice"));
            }
        }
        if relays.is_empty() {
            return Err("no relay locations found".to_owned());
        }
        Ok(Self { relays })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn len(&self) -> usize {
        self.relays.len()
    }

    /// Does this list say where `relay` is?
    pub fn knows(&self, relay: &str) -> bool {
        self.relays.contains_key(&normalize(relay))
    }

    /// Relays in this list that are not among `relays`.
    pub fn not_in(&self, relays: &[String]) -> Vec<String> {
        let have: std::collections::HashSet<String> = relays.iter().map(|r| normalize(r)).collect();
        let mut extra: Vec<String> = self
            .relays
            .keys()
            .filter(|r| !have.contains(*r))
            .cloned()
            .collect();
        extra.sort();
        extra
    }

    /// Estimated milliseconds from `at` to every relay whose position is known. A
    /// relay with no position is left out, which makes it lose to any that has one.
    pub fn costs_from(&self, at: Point) -> HashMap<String, f64> {
        self.relays
            .iter()
            .map(|(relay, p)| (relay.clone(), distance_km(at, *p) / KM_PER_MS))
            .collect()
    }
}

/// An open GeoIP database.
pub struct GeoDb {
    locate: Box<dyn Fn(IpAddr) -> Option<Point> + Send + Sync>,
    kind: String,
}

/// Map a private copy of the file, never the file itself.
///
/// A mapped file that is truncated underneath the mapping kills the process with
/// SIGBUS on the next access to a page that is no longer there, and "the next access"
/// is a lookup for some device, in a server everyone depends on. Truncation is what
/// `curl -o geo.mmdb`, or unpacking straight onto the file, does. So the file is copied
/// first and the copy mapped; the copy is unlinked at once, and the kernel keeps its
/// data until the mapping is dropped. Whatever happens to the original afterwards
/// cannot reach the running server; a half-written original just fails to open and the
/// previous database stays in use.
#[cfg(unix)]
fn open_mapped(path: &Path) -> Result<maxminddb::Reader<maxminddb::Mmap>, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A copy left behind by a process that died between copying and unlinking.
    if let Ok(entries) = std::fs::read_dir(path.parent().unwrap_or_else(|| Path::new("."))) {
        for e in entries.flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if f.starts_with(&format!(".{name}.")) && f.ends_with(".loaded") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let copy_name = format!(
        ".{name}.{}.{}.loaded",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    // Beside the original if that folder can be written (it normally can: it is the
    // data folder), otherwise in the temporary folder, which a read-only single-file
    // mount or a minimal image may or may not have.
    let mut copy_error = String::new();
    for dir in [
        path.parent().map(Path::to_path_buf),
        Some(std::env::temp_dir()),
    ]
    .into_iter()
    .flatten()
    {
        let copy = dir.join(&copy_name);
        match std::fs::copy(path, &copy) {
            Ok(_) => {
                // SAFETY: the copy is private to this process and is unlinked below,
                // so nothing else can modify it while it is mapped.
                let reader = unsafe { maxminddb::Reader::open_mmap(&copy) };
                let _ = std::fs::remove_file(&copy);
                return reader.map_err(|e| format!("cannot open {}: {e}", path.display()));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&copy);
                copy_error = format!("{}: {e}", dir.display());
            }
        }
    }
    // Nowhere to put a copy. Map the original, which is safe while nothing writes to
    // it; a read-only mount cannot be written from inside the container.
    log::warn!(
        "geo database: no folder to copy {} into ({copy_error}); reading it in place, \
         so replace it only by renaming a new file over it",
        path.display()
    );
    // SAFETY: as in the non-unix version below.
    unsafe { maxminddb::Reader::open_mmap(path) }
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

/// Windows cannot unlink a mapped file, so there the file itself is mapped.
#[cfg(not(unix))]
fn open_mapped(path: &Path) -> Result<maxminddb::Reader<maxminddb::Mmap>, String> {
    // SAFETY: the file is only read; update it by renaming a new one over it.
    unsafe { maxminddb::Reader::open_mmap(path) }
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

impl GeoDb {
    /// The copy is mapped, not read into memory: the City database is over 100 MB and
    /// only the parts a lookup touches are paged in. See `open_mapped` for why it is
    /// a copy.
    pub fn open(path: &Path) -> Result<Self, String> {
        let reader = open_mapped(path)?;
        let kind = reader.metadata().database_type.clone();
        Ok(Self {
            kind,
            locate: Box::new(move |ip| {
                use maxminddb::PathElement::Key;
                let found = reader.lookup(ip).ok()?;
                let lat: f64 = found
                    .decode_path(&[Key("location"), Key("latitude")])
                    .ok()??;
                let lon: f64 = found
                    .decode_path(&[Key("location"), Key("longitude")])
                    .ok()??;
                // A damaged file must not feed nonsense into the distance sums.
                if !(lat.is_finite() && lon.is_finite())
                    || !(-90.0..=90.0).contains(&lat)
                    || !(-180.0..=180.0).contains(&lon)
                {
                    return None;
                }
                Some(Point { lat, lon })
            }),
        })
    }

    /// What the database says it is, for example "DBIP-City-Lite" or "GeoIP2-Country".
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Is this file a usable City database? Used on a download before it replaces the
    /// current one: it must open, say it is a City database, and have at least
    /// `min_nodes` nodes in its search tree (a handful would mean it is empty or a toy).
    pub fn validate_city(path: &Path, min_nodes: u32) -> Result<(), String> {
        // SAFETY: as in `open`; the file is not modified while it is mapped.
        let reader = unsafe { maxminddb::Reader::open_mmap(path) }
            .map_err(|e| format!("not a GeoIP database: {e}"))?;
        let m = reader.metadata();
        if !m.database_type.contains("City") {
            return Err(format!(
                "database type is {:?}, not a City database",
                m.database_type
            ));
        }
        if m.node_count < min_nodes {
            return Err(format!(
                "only {} nodes, fewer than the {min_nodes} a real City database has",
                m.node_count
            ));
        }
        Ok(())
    }

    /// An address the database has no position for, such as a private one, is `None`.
    pub fn locate(&self, ip: IpAddr) -> Option<Point> {
        (self.locate)(ip.to_canonical())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HK: Point = Point {
        lat: 22.32,
        lon: 114.17,
    };
    const SH: Point = Point {
        lat: 31.23,
        lon: 121.47,
    };
    const LON: Point = Point {
        lat: 51.51,
        lon: -0.13,
    };

    #[test]
    fn distances_are_about_right() {
        // Real figures: Hong Kong - Shanghai about 1,230 km, Hong Kong - London
        // about 9,640 km. Within 3%.
        let near = |got: f64, want: f64| (got - want).abs() / want < 0.03;
        assert!(near(distance_km(HK, SH), 1230.0), "{}", distance_km(HK, SH));
        assert!(
            near(distance_km(HK, LON), 9640.0),
            "{}",
            distance_km(HK, LON)
        );
        assert_eq!(distance_km(HK, HK), 0.0);
        // Across the date line the short way round.
        let a = Point {
            lat: 0.0,
            lon: 179.0,
        };
        let b = Point {
            lat: 0.0,
            lon: -179.0,
        };
        assert!(distance_km(a, b) < 300.0);
    }

    #[test]
    fn locations_parse_with_comments_and_default_port() {
        let l = Locations::parse(
            "# where the relays are\nhk.example.com  22.32, 114.17  # Hong Kong\nsh.example.com:21117 31.23 121.47\n",
        )
        .unwrap();
        assert_eq!(l.len(), 2);
        let costs = l.costs_from(HK);
        assert!(costs["hk.example.com:21117"] < 1.0);
        let sh = costs["sh.example.com:21117"];
        assert!((sh - 1230.0 / KM_PER_MS).abs() < 1.0, "{sh}");
    }

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/geo-test.mmdb");

    #[test]
    fn the_database_locates_known_ranges_and_only_those() {
        let db = GeoDb::open(Path::new(FIXTURE)).unwrap();
        let at = |s: &str| db.locate(s.parse().unwrap());
        let hk = at("10.40.7.7").expect("10.40/16 is in the fixture");
        assert!((hk.lat - 22.3).abs() < 1e-9 && (hk.lon - 114.2).abs() < 1e-9);
        let london = at("10.41.0.1").unwrap();
        assert!((london.lat - 51.5).abs() < 1e-9);
        // IPv6, and an IPv4 address wrapped in IPv6 as a dual-stack listener reports it.
        assert_eq!(at("2001:db8:41::9"), Some(london));
        assert_eq!(at("::ffff:10.40.7.7"), Some(hk));
        // An address the database does not know, and a private one outside it.
        assert_eq!(at("8.8.8.8"), None);
        assert_eq!(at("192.168.1.1"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_database_in_a_read_only_folder_still_opens() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("geo-readonly-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("geo.mmdb");
        std::fs::copy(FIXTURE, &db).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let opened = GeoDb::open(&db);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        let db = opened.expect("a database in a read-only folder did not open");
        assert!(db.locate("10.40.0.1".parse().unwrap()).is_some());
        assert_eq!(leftovers, ["geo.mmdb"], "left copies behind");
    }

    #[test]
    fn a_missing_or_wrong_file_is_an_error_not_a_crash() {
        assert!(GeoDb::open(Path::new("/nonexistent/geo.mmdb")).is_err());
        // Any file that is not a database, here this source file.
        let not_a_db = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        assert!(GeoDb::open(Path::new(not_a_db)).is_err());
    }

    #[test]
    fn the_nearest_relay_by_location_wins_for_an_address_the_table_never_saw() {
        use crate::relay_routes::choose_from;
        let db = GeoDb::open(Path::new(FIXTURE)).unwrap();
        let locations = Locations::parse(
            "hk.example.com 22.3,114.2\nlon.example.com 51.5,-0.1\nny.example.com 40.7,-74.0",
        )
        .unwrap();
        let healthy: Vec<String> = ["hk.example.com", "lon.example.com", "ny.example.com"]
            .iter()
            .map(|r| normalize(r))
            .collect();
        let side = |ip: &str| locations.costs_from(db.locate(ip.parse().unwrap()).unwrap());
        let pick = |a: &str, b: &str| choose_from(&healthy, &[side(a), side(b)]).unwrap().relay;
        assert_eq!(pick("10.40.0.1", "10.40.0.2"), "hk.example.com:21117");
        assert_eq!(pick("10.41.0.1", "10.41.0.2"), "lon.example.com:21117");
        assert_eq!(pick("10.42.0.1", "10.42.0.2"), "ny.example.com:21117");
        // London to New York: London is 5,570 km from New York, so either end's relay
        // is far from the other; the middle ground is whichever sum is smaller.
        let across = pick("10.41.0.1", "10.42.0.1");
        assert!(across == "lon.example.com:21117" || across == "ny.example.com:21117");
    }

    /// Run by hand against a real database:
    ///
    ///     GEO_REAL_DB=/path/to/dbip-city-lite.mmdb cargo test --lib real_database -- --ignored --nocapture
    ///
    /// Well-known public addresses with a known city, checked to within 600 km.
    #[test]
    #[ignore]
    fn real_database_places_well_known_addresses_sensibly() {
        let Ok(path) = std::env::var("GEO_REAL_DB") else {
            panic!("set GEO_REAL_DB to a City-layout MMDB file");
        };
        let db = GeoDb::open(Path::new(&path)).unwrap();
        // (address, what it is, a point in that city)
        let cases: [(&str, &str, Point); 4] = [
            (
                "8.8.8.8",
                "Google DNS, US west",
                Point {
                    lat: 37.4,
                    lon: -122.1,
                },
            ),
            (
                "9.9.9.9",
                "Quad9",
                Point {
                    lat: 37.8,
                    lon: -122.4,
                },
            ),
            (
                "180.76.76.76",
                "Baidu DNS, Beijing",
                Point {
                    lat: 39.9,
                    lon: 116.4,
                },
            ),
            (
                "223.5.5.5",
                "Alibaba DNS, Hangzhou",
                Point {
                    lat: 30.3,
                    lon: 120.2,
                },
            ),
        ];
        let mut bad = Vec::new();
        for (ip, what, want) in cases {
            let got = db.locate(ip.parse().unwrap());
            println!("{ip:24} {what:28} -> {got:?}");
            match got {
                Some(p) if distance_km(p, want) < 600.0 => {}
                other => bad.push(format!("{ip} ({what}): {other:?}")),
            }
        }
        // IPv6 is placed too. The city is not checked: these are anycast addresses,
        // served from many places, so a database can only guess.
        let v6 = db.locate("2001:4860:4860::8888".parse().unwrap());
        println!("{:24} IPv6 anycast -> {v6:?}", "2001:4860:4860::8888");
        if v6.is_none() {
            bad.push("an IPv6 address was not placed at all".to_owned());
        }
        // Private and reserved addresses must not be placed.
        for ip in ["10.0.0.1", "192.168.1.1", "127.0.0.1"] {
            let got = db.locate(ip.parse().unwrap());
            println!("{ip:24} private -> {got:?}");
            if got.is_some() {
                bad.push(format!("{ip} should not be placed, got {got:?}"));
            }
        }
        assert!(bad.is_empty(), "implausible: {bad:#?}");
    }

    #[test]
    fn bad_locations_are_refused_with_the_line_number() {
        for (text, needle) in [
            ("hk.example.com\n", "line 1"),
            ("a:1 1,2\nb:1 91,0\n", "line 2"),
            ("a:1 1,200\n", "line 1"),
            ("a:1 north,east\n", "not a latitude"),
            ("a:1 1,2,3\n", "exactly"),
            ("a:1 1,2\na:1 3,4\n", "twice"),
            ("# nothing\n", "no relay locations"),
        ] {
            let err = Locations::parse(text).unwrap_err();
            assert!(err.contains(needle), "{text:?} gave {err:?}");
        }
    }
}
