//! Choosing a relay for a session: everything this fork adds to it, in one place.
//!
//! Upstream's `rendezvous_server.rs` keeps a list of relays and hands them out in
//! turn. This fork adds a held choice per connection attempt, the routing table
//! (`relay_routes`), geo placement (`relay_geo`), `use-relay`, and the console
//! commands that explain them. They live here so that upstream's file holds only a
//! few calls into this module (`configure`, `reload`, `set_live`, `pick`, `console`),
//! and an upstream change to that file rarely meets this code.

use crate::common::{get_arg, get_arg_or};
use crate::relay_geo::{GeoDb, Locations, Point};
use crate::relay_routes::{choose_from, normalize, RouteTable};
use hbb_common::{log, tokio};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    sync::Arc,
    sync::Mutex as StdMutex,
    time::{Duration, Instant, SystemTime},
};

static ROTATION_RELAY_SERVER: AtomicUsize = AtomicUsize::new(0);

/// How long one connection attempt's choice of relay is held. A choice is made
/// per attempt, not per call: the same attempt can ask for a relay more than once
/// (a retry, or the LAN branch that only exists with `--mask`), and two peers that
/// are handed different relays never meet. Override with RELAY_PIN_TTL (seconds,
/// 0 turns pinning off).
const RELAY_PIN_TTL_DEFAULT_SEC: u64 = 30;
static RELAY_PIN_TTL_MS: AtomicU64 = AtomicU64::new(RELAY_PIN_TTL_DEFAULT_SEC * 1000);
/// Bound on held choices; expired ones are dropped first.
const MAX_RELAY_PINS: usize = 4096;

/// The two ends of a session, in a fixed order, so either can be the controller.
type PinKey = (IpAddr, IpAddr);

struct RelayPin {
    relay: String,
    until: Instant,
}

/// The routing table and what is known about the file it came from.
#[derive(Default)]
struct RouteState {
    path: Option<PathBuf>,
    table: Option<RouteTable>,
    /// What the file looked like when last read. `None` outside: never looked.
    seen: Option<Option<(SystemTime, u64)>>,
    error: Option<String>,
}

/// The GeoIP database and the relay locations, and what is known about the files
/// they came from. Both are needed before an address can be turned into a distance.
#[derive(Default)]
struct GeoState {
    db_path: Option<PathBuf>,
    db: Option<GeoDb>,
    db_seen: Option<Option<(SystemTime, u64)>>,
    db_error: Option<String>,
    loc_path: Option<PathBuf>,
    locations: Option<Locations>,
    loc_seen: Option<Option<(SystemTime, u64)>>,
    loc_error: Option<String>,
}

/// The relay the operator has chosen for every new session, if any, and the file
/// that remembers it across a restart.
#[derive(Default)]
struct PreferredRelay {
    relay: Option<String>,
    path: PathBuf,
}

/// A decision, with enough to explain it.
pub(crate) struct RelayPick {
    pub(crate) relay: String,
    pub(crate) reason: &'static str,
    /// Each healthy relay's total cost, when the routing table decided.
    pub(crate) costs: Vec<(String, f64)>,
}

/// The healthy relays and how to choose among them. Cheap to clone: every field is
/// shared, because a connection handler runs on a clone of the server taken when
/// the connection opened, and a long-lived websocket would otherwise keep the list
/// it was born with and never learn that a relay stopped answering.
#[derive(Clone, Default)]
pub(crate) struct RelaySelector {
    live: Arc<StdMutex<Arc<Vec<String>>>>,
    routes: Arc<StdMutex<RouteState>>,
    pins: Arc<StdMutex<HashMap<PinKey, RelayPin>>>,
    preferred: Arc<StdMutex<PreferredRelay>>,
    geo: Arc<StdMutex<GeoState>>,
}

impl RelaySelector {
    /// Console help lines for the commands `console` answers.
    pub(crate) const HELP: &'static [&'static str] = &[
        "test-relay(tr) <ip1> [<ip2>]",
        "relay-routes(rr)",
        "use-relay(ur) [<host:port>|auto]",
    ];

    /// Read the settings (RELAY_PIN_TTL, RELAY_PREFERRED_FILE, GEO_DB,
    /// RELAY_LOCATIONS, RELAY_ROUTES) and load the files they name. Called once,
    /// after the relay list is set.
    pub(crate) fn configure(&self) {
        let pin_ttl = get_arg("relay-pin-ttl")
            .parse::<u64>()
            .unwrap_or(RELAY_PIN_TTL_DEFAULT_SEC);
        RELAY_PIN_TTL_MS.store(pin_ttl * 1000, Ordering::SeqCst);
        log::info!("RELAY_PIN_TTL={}s", pin_ttl);
        self.load_preferred_relay();
        {
            let (db, loc) = (get_arg("geo-db"), get_arg("relay-locations"));
            if db.is_empty() != loc.is_empty() {
                log::warn!(
                    "geo routing needs both GEO_DB and RELAY_LOCATIONS; with only one it does nothing"
                );
            }
            if let Ok(mut g) = self.geo.lock() {
                if !db.is_empty() {
                    g.db_path = Some(PathBuf::from(&db));
                }
                if !loc.is_empty() {
                    g.loc_path = Some(PathBuf::from(&loc));
                }
            }
            self.reload_relay_geo();
        }
        let routes_path = get_arg("relay-routes");
        if !routes_path.is_empty() {
            log::info!("RELAY_ROUTES={}", routes_path);
            if let Ok(mut st) = self.routes.lock() {
                st.path = Some(PathBuf::from(&routes_path));
            }
            self.reload_relay_routes();
        }
    }

    /// Re-read whichever files changed. Called every few seconds.
    pub(crate) fn reload(&self) {
        self.reload_relay_routes();
        self.reload_relay_geo();
    }

    /// The console commands this module owns, or `None` for any other command.
    pub(crate) fn console(&self, cmd: &str) -> Option<String> {
        use std::fmt::Write as _;
        let mut res = String::new();
        let mut fds = cmd.trim().split(' ');
        match fds.next() {
            Some("test-relay" | "tr") => {
                // The dry run: what would be chosen for a session between these
                // two addresses, and why. Holds, rotates and logs nothing.
                if let Some(first) = fds.next() {
                    let second = fds.next().unwrap_or(first);
                    if let (Ok(a), Ok(b)) = (first.parse::<IpAddr>(), second.parse::<IpAddr>()) {
                        let pick = self.pick(a, b, false);
                        let _ = writeln!(res, "relay: {} ({})", pick.relay, pick.reason);
                        for ip in [a, b] {
                            if let Some(p) = self.locate(ip) {
                                let _ = writeln!(res, "located {ip}: {:.2},{:.2}", p.lat, p.lon);
                            }
                        }
                        for (relay, cost) in &pick.costs {
                            let _ = writeln!(res, "{relay}={cost}");
                        }
                    } else {
                        let _ = writeln!(res, "usage: test-relay <ip1> [<ip2>]");
                    }
                }
            }
            Some("use-relay" | "ur") => {
                res = self.use_relay_command(fds.next());
            }
            Some("relay-routes" | "rr") => {
                if let Ok(st) = self.routes.lock() {
                    match &st.path {
                        None => {
                            let _ = writeln!(res, "no routing table configured (RELAY_ROUTES)");
                        }
                        Some(path) => {
                            let _ = writeln!(res, "file: {}", path.display());
                            let _ = writeln!(
                                res,
                                "routes: {}",
                                st.table.as_ref().map_or(0, |t| t.rule_count())
                            );
                            if let Some(err) = &st.error {
                                let _ = writeln!(res, "last error: {err}");
                            }
                        }
                    }
                }
                if let Ok(g) = self.geo.lock() {
                    match (&g.db_path, &g.loc_path) {
                        (None, None) => {
                            let _ = writeln!(
                                res,
                                "no geo routing configured (GEO_DB, RELAY_LOCATIONS)"
                            );
                        }
                        (db, loc) => {
                            let _ = writeln!(
                                res,
                                "geo database: {}",
                                match (db, &g.db, &g.db_error) {
                                    (None, _, _) => "not configured".to_owned(),
                                    (Some(p), Some(_), None) => format!("{} (loaded)", p.display()),
                                    (Some(p), Some(_), Some(e)) => {
                                        format!("{} (previous kept: {e})", p.display())
                                    }
                                    (Some(p), None, e) => format!(
                                        "{} (NOT loaded: {})",
                                        p.display(),
                                        e.clone().unwrap_or_default()
                                    ),
                                }
                            );
                            let _ = writeln!(
                                res,
                                "relay locations: {}",
                                match (loc, &g.locations, &g.loc_error) {
                                    (None, _, _) => "not configured".to_owned(),
                                    (Some(p), Some(l), None) => {
                                        format!("{} ({} relays)", p.display(), l.len())
                                    }
                                    (Some(p), Some(l), Some(e)) => format!(
                                        "{} ({} relays; previous kept: {e})",
                                        p.display(),
                                        l.len()
                                    ),
                                    (Some(p), None, e) => format!(
                                        "{} (NOT loaded: {})",
                                        p.display(),
                                        e.clone().unwrap_or_default()
                                    ),
                                }
                            );
                        }
                    }
                }
                if let Ok(g) = self.geo.lock() {
                    if let Some(l) = &g.locations {
                        let live = self.live();
                        let missing: Vec<&str> = live
                            .iter()
                            .filter(|r| !l.knows(r))
                            .map(|r| r.as_str())
                            .collect();
                        if !missing.is_empty() {
                            let _ = writeln!(
                                res,
                                "WARNING: no location for {}: treated as very far away",
                                missing.join(", ")
                            );
                        }
                    }
                }
                let _ = writeln!(
                    res,
                    "pin ttl: {}s, held: {}",
                    RELAY_PIN_TTL_MS.load(Ordering::SeqCst) / 1000,
                    self.pins.lock().map_or(0, |p| p.len())
                );
            }
            _ => return None,
        }
        Some(res)
    }

    pub(crate) fn live(&self) -> Arc<Vec<String>> {
        self.live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default()
    }

    pub(crate) fn set_live(&self, relays: Arc<Vec<String>>) {
        if let Ok(mut live) = self.live.lock() {
            *live = relays;
        }
    }

    /// Choose a relay for a session between `pa` and `pb`.
    ///
    /// With `commit`, the choice is held for RELAY_PIN_TTL so that every question
    /// about the same attempt gets the same answer, and is logged. Without it,
    /// nothing is held, rotated or logged: that is the dry run.
    ///
    /// Policy: the routing table if one is loaded and knows either end, otherwise
    /// round-robin over the healthy relays. A broken or missing table therefore
    /// degrades to what a server without one does, and never blocks a connection.
    pub(crate) fn pick(&self, pa: IpAddr, pb: IpAddr, commit: bool) -> RelayPick {
        let live = self.live();
        match live.len() {
            0 => {
                return RelayPick {
                    relay: "".to_owned(),
                    reason: "no relay configured",
                    costs: Vec::new(),
                }
            }
            1 => {
                return RelayPick {
                    relay: live[0].clone(),
                    reason: "only relay",
                    costs: Vec::new(),
                }
            }
            _ => {}
        }
        let preferred = self.preferred.lock().ok().and_then(|p| p.relay.clone());
        if let Some(want) = preferred {
            let want_n = normalize(&want);
            if let Some(relay) = live.iter().find(|r| normalize(r) == want_n) {
                if commit {
                    log::info!("relay for {} <-> {}: {} (preferred)", pa, pb, relay);
                }
                return RelayPick {
                    relay: relay.clone(),
                    reason: "preferred",
                    costs: Vec::new(),
                };
            }
            // A chosen relay that has stopped answering must not take every
            // connection down with it: choose among the others, as if none were chosen.
            if commit {
                log::warn!("preferred relay {want} is not answering; choosing among the others");
            }
        }

        let (a, b) = (pa.to_canonical(), pb.to_canonical());
        let key = if a <= b { (a, b) } else { (b, a) };

        if commit {
            if let Some(relay) = self.pinned_relay(&key, &live) {
                return RelayPick {
                    relay,
                    reason: "pinned",
                    costs: Vec::new(),
                };
            }
        }

        // What is known about each end: the routing table's line for it, or failing
        // that the relays' distances from where the address is located.
        let (sides, sources): (Vec<_>, Vec<_>) = [pa, pb]
            .into_iter()
            .filter_map(|ip| self.side_costs(ip))
            .unzip();
        let routed = choose_from(live.as_slice(), &sides);
        let pick = match routed {
            Some(c) => RelayPick {
                relay: c.relay,
                reason: if sources.iter().all(|s| *s == "routes") {
                    "routes"
                } else if sources.iter().all(|s| *s == "geo") {
                    "geo"
                } else {
                    "routes+geo"
                },
                costs: c.costs,
            },
            None => {
                let n = if commit {
                    ROTATION_RELAY_SERVER.fetch_add(1, Ordering::SeqCst)
                } else {
                    ROTATION_RELAY_SERVER.load(Ordering::SeqCst)
                };
                RelayPick {
                    relay: live[n % live.len()].clone(),
                    reason: "round-robin",
                    costs: Vec::new(),
                }
            }
        };
        if commit {
            self.pin_relay(key, &pick.relay);
            log::info!(
                "relay for {} <-> {}: {} ({}{})",
                pa,
                pb,
                pick.relay,
                pick.reason,
                pick.costs
                    .iter()
                    .map(|(r, c)| format!(" {r}={c}"))
                    .collect::<String>()
            );
        }
        pick
    }

    /// What is known about one address: the routing table's line for it, else its
    /// distance from each relay by location, else nothing.
    fn side_costs(&self, ip: IpAddr) -> Option<(HashMap<String, f64>, &'static str)> {
        let from_table = self
            .routes
            .lock()
            .ok()
            .and_then(|st| st.table.as_ref().and_then(|t| t.costs_for(ip)));
        if let Some(costs) = from_table {
            return Some((costs, "routes"));
        }
        let at = self.locate(ip)?;
        let g = self.geo.lock().ok()?;
        Some((g.locations.as_ref()?.costs_from(at), "geo"))
    }

    /// Where the GeoIP database puts an address, if geo routing is set up.
    fn locate(&self, ip: IpAddr) -> Option<Point> {
        let g = self.geo.lock().ok()?;
        g.locations.as_ref()?;
        g.db.as_ref()?.locate(ip)
    }

    /// A held choice that has not expired and names a relay that is still healthy.
    /// A pin must never keep handing out a relay that has stopped answering.
    fn pinned_relay(&self, key: &PinKey, live: &[String]) -> Option<String> {
        let mut pins = self.pins.lock().ok()?;
        match pins.get(key) {
            Some(p) if p.until > Instant::now() && live.contains(&p.relay) => Some(p.relay.clone()),
            Some(_) => {
                pins.remove(key);
                None
            }
            None => None,
        }
    }

    fn pin_relay(&self, key: PinKey, relay: &str) {
        let ttl = RELAY_PIN_TTL_MS.load(Ordering::SeqCst);
        if ttl == 0 {
            return;
        }
        let Ok(mut pins) = self.pins.lock() else {
            return;
        };
        let now = Instant::now();
        if pins.len() >= MAX_RELAY_PINS {
            pins.retain(|_, p| p.until > now);
            if pins.len() >= MAX_RELAY_PINS {
                pins.clear();
            }
        }
        pins.insert(
            key,
            RelayPin {
                relay: relay.to_owned(),
                until: now + Duration::from_millis(ttl),
            },
        );
    }

    /// Read the remembered choice, if there is one. Not validated against the relay
    /// list: a relay that is not answering yet is handled when a session is placed.
    fn load_preferred_relay(&self) {
        let path = PathBuf::from(get_arg_or(
            "relay-preferred-file",
            "relay_preferred".to_owned(),
        ));
        let relay = std::fs::read_to_string(&path).ok().and_then(|t| {
            t.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned)
        });
        if let Some(r) = &relay {
            log::info!("preferred relay: {} (from {})", r, path.display());
        }
        if let Ok(mut p) = self.preferred.lock() {
            p.relay = relay;
            p.path = path;
        }
    }

    /// `use-relay`: show, set or clear the preferred relay.
    fn use_relay_command(&self, arg: Option<&str>) -> String {
        let mut res = String::new();
        let live = self.live();
        let Ok(mut pref) = self.preferred.lock() else {
            return "internal error\n".to_owned();
        };
        match arg {
            None => match &pref.relay {
                None => res.push_str("none: relays are chosen automatically\n"),
                Some(r) => {
                    let up = live.iter().any(|l| normalize(l) == normalize(r));
                    res.push_str(&format!(
                        "{r} ({})\n",
                        if up {
                            "answering, in use for every new session"
                        } else {
                            "NOT answering, the other relays are being used"
                        }
                    ));
                }
            },
            Some("auto") => {
                pref.relay = None;
                let removed = match std::fs::remove_file(&pref.path) {
                    Ok(()) => true,
                    Err(e) => e.kind() == std::io::ErrorKind::NotFound,
                };
                res.push_str("relays are chosen automatically again\n");
                if !removed {
                    res.push_str(&format!(
                        "warning: could not delete {}, the choice will come back after a restart\n",
                        pref.path.display()
                    ));
                }
                log::info!("preferred relay cleared");
            }
            Some(want) => {
                let want_n = normalize(want);
                match live.iter().find(|r| normalize(r) == want_n) {
                    None => {
                        res.push_str(&format!("{want} is not one of the answering relays:\n"));
                        for r in live.iter() {
                            res.push_str(&format!("  {r}\n"));
                        }
                        res.push_str("nothing changed\n");
                    }
                    Some(relay) => {
                        pref.relay = Some(relay.clone());
                        res.push_str(&format!("every new session now uses {relay}\n"));
                        if let Err(e) = std::fs::write(&pref.path, format!("{relay}\n")) {
                            res.push_str(&format!(
                                "warning: could not save to {}: {e}; the choice is lost on restart\n",
                                pref.path.display()
                            ));
                        }
                        log::info!("preferred relay set to {relay}");
                    }
                }
            }
        }
        drop(pref);
        // Held choices were made before this one.
        if let Ok(mut pins) = self.pins.lock() {
            pins.clear();
        }
        res
    }

    /// Re-read the routing file if it changed. Called every few seconds. A file
    /// that cannot be read or parsed is reported once per change and the previous
    /// table, or round-robin if there was none, stays in force.
    fn reload_relay_routes(&self) {
        let Ok(mut st) = self.routes.lock() else {
            return;
        };
        let Some(path) = st.path.clone() else {
            return;
        };
        let stamp = std::fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if st.seen == Some(stamp) {
            return;
        }
        st.seen = Some(stamp);
        let loaded = match stamp {
            Some(_) => RouteTable::load(&path),
            None => Err(format!("cannot read {}", path.display())),
        };
        match loaded {
            Ok(table) => {
                log::info!(
                    "relay routes: loaded {} route(s) from {}",
                    table.rule_count(),
                    path.display()
                );
                st.table = Some(table);
                st.error = None;
                // Decisions made under the old table are not worth keeping.
                if let Ok(mut pins) = self.pins.lock() {
                    pins.clear();
                }
            }
            Err(err) => {
                log::error!(
                    "relay routes: {}: {}; keeping {}",
                    path.display(),
                    err,
                    if st.table.is_some() {
                        "the previous table"
                    } else {
                        "round-robin"
                    }
                );
                st.error = Some(err);
            }
        }
    }

    /// Re-read the GeoIP database and the relay locations when either file changed.
    /// Called every few seconds, next to `reload_relay_routes`, with the same rule: a
    /// file that cannot be read is reported once and the previous one stays in force.
    fn reload_relay_geo(&self) {
        type Stamp = Option<(SystemTime, u64)>;
        fn stamp(path: &std::path::Path) -> Stamp {
            let m = std::fs::metadata(path).ok()?;
            Some((m.modified().ok()?, m.len()))
        }
        // Look under the lock, work outside it. Opening the database copies it, and
        // session placement waits on this lock, so nothing slow may happen inside.
        let (db_path, db_seen, loc_path, loc_seen) = match self.geo.lock() {
            Ok(g) => (g.db_path.clone(), g.db_seen, g.loc_path.clone(), g.loc_seen),
            Err(_) => return,
        };
        let db_now = db_path.as_deref().map(stamp);
        let loc_now = loc_path.as_deref().map(stamp);
        let db_changed = db_now.is_some() && db_seen != db_now;
        let loc_changed = loc_now.is_some() && loc_seen != loc_now;
        if !db_changed && !loc_changed {
            return;
        }

        let db_loaded = match (&db_path, db_changed, db_now.flatten()) {
            (Some(path), true, Some(_)) => {
                // A copy of a file this size takes a moment; other tasks move to
                // other threads meanwhile.
                Some(tokio::task::block_in_place(|| GeoDb::open(path)))
            }
            (Some(path), true, None) => Some(Err(format!("cannot read {}", path.display()))),
            _ => None,
        };
        let loc_loaded = match (&loc_path, loc_changed, loc_now.flatten()) {
            (Some(path), true, Some(_)) => Some(Locations::load(path)),
            (Some(path), true, None) => Some(Err(format!("cannot read {}", path.display()))),
            _ => None,
        };

        let mut changed = false;
        let Ok(mut g) = self.geo.lock() else {
            return;
        };
        if let (Some(path), Some(loaded)) = (&db_path, db_loaded) {
            g.db_seen = db_now;
            match loaded {
                Ok(db) => {
                    log::info!("geo database: loaded {} ({})", path.display(), db.kind());
                    if !db.kind().contains("City") {
                        log::warn!(
                            "geo database: this is a {:?} database, which has no coordinates, \
                             so no address will be placed; use a City database",
                            db.kind()
                        );
                    }
                    g.db = Some(db);
                    g.db_error = None;
                    changed = true;
                }
                Err(err) => {
                    log::error!(
                        "geo database: {err}; keeping {}",
                        if g.db.is_some() {
                            "the previous one"
                        } else {
                            "none"
                        }
                    );
                    g.db_error = Some(err);
                }
            }
        }
        if let (Some(path), Some(loaded)) = (&loc_path, loc_loaded) {
            g.loc_seen = loc_now;
            match loaded {
                Ok(l) => {
                    log::info!(
                        "relay locations: loaded {} relay(s) from {}",
                        l.len(),
                        path.display()
                    );
                    g.locations = Some(l);
                    g.loc_error = None;
                    changed = true;
                }
                Err(err) => {
                    log::error!(
                        "relay locations: {}: {err}; keeping {}",
                        path.display(),
                        if g.locations.is_some() {
                            "the previous list"
                        } else {
                            "none"
                        }
                    );
                    g.loc_error = Some(err);
                }
            }
        }
        // A relay with no position is treated as very far away and is chosen only if
        // nothing else can be. That is almost always a typo, so say so.
        if loc_changed {
            if let Some(l) = &g.locations {
                let configured = self.live();
                for relay in configured.iter().filter(|r| !l.knows(r)) {
                    log::warn!(
                        "relay {relay} has no entry in the relay locations; it will be treated \
                         as very far away. Write it there exactly as in the relay list (host:port)"
                    );
                }
                for extra in l.not_in(&configured) {
                    log::warn!(
                        "relay locations has {extra}, which is not in the relay list, so it is ignored"
                    );
                }
            }
        }
        drop(g);
        if changed {
            // Decisions made under the old data are not worth keeping.
            if let Ok(mut pins) = self.pins.lock() {
                pins.clear();
            }
        }
    }
}
