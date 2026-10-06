//! Where a client is, from its address, and so which relay is nearest.
//!
//! Two inputs, both files the operator provides: a GeoIP database (MMDB, such as
//! DB-IP's free "IP to City Lite") that maps an address to an approximate latitude
//! and longitude, and a short list saying where each relay is. The distance between
//! them, turned into a rough number of milliseconds, is used exactly like a routing
//! table entry (see `relay_routes`), for the addresses the table has no line for.

use crate::relay_routes::normalize;
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
}

impl GeoDb {
    /// The file is mapped, not read: the City database is over 100 MB and only the
    /// parts a lookup touches are paged in. Replace the file by renaming a new one
    /// over it, never by writing into it, which would pull the ground from under
    /// the mapping.
    pub fn open(path: &Path) -> Result<Self, String> {
        // SAFETY: the mapping is only read. The documented way to update the file is
        // a rename, which leaves the mapped inode intact.
        let reader = unsafe { maxminddb::Reader::open_mmap(path) }
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        Ok(Self {
            locate: Box::new(move |ip| {
                use maxminddb::PathElement::Key;
                let found = reader.lookup(ip).ok()?;
                let lat: f64 = found
                    .decode_path(&[Key("location"), Key("latitude")])
                    .ok()??;
                let lon: f64 = found
                    .decode_path(&[Key("location"), Key("longitude")])
                    .ok()??;
                Some(Point { lat, lon })
            }),
        })
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
