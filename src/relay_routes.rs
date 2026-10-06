//! A hand-made relay routing table: which relay is closest to which clients.
//!
//! One line per client network, each listing what every relay costs from there,
//! in milliseconds:
//!
//! ```text
//! # CIDR              relay=ms[,relay=ms ...]
//! 203.0.113.0/24      hk.example.com:21117=38,sh.example.com:21117=95
//! 198.51.100.0/22     sh.example.com:21117=12,hk.example.com:21117=80
//! ```
//!
//! A relayed session crosses two legs, controller -> relay and target -> relay, so
//! a relay's cost for a session is the sum of what it costs from each side. The
//! cheapest healthy relay wins. This is a pure function of the two addresses, the
//! table and the set of healthy relays: no clock, no counter, no I/O. That is what
//! lets the caller promise one decision per connection attempt, and what makes
//! every rule below testable on its own.
//!
//! The numbers come from `tools/relay-rtt` (`relay_rtt.py routes`), or by hand.

use std::{collections::HashMap, net::IpAddr, path::Path};

use hbb_common::config::RELAY_PORT;
use ipnetwork::IpNetwork;

/// What a relay costs from a client network whose line does not list it. Far
/// above any real round trip, so a listed relay always beats an unlisted one, but
/// finite, so an all-unlisted table still yields an answer.
pub const UNLISTED_COST_MS: f64 = 1000.0;

/// Relays are compared as `host:port`, lower case, with hbbr's default port
/// filled in, because hbbs accepts `-r host` for `host:21117`.
pub fn normalize(relay: &str) -> String {
    let relay = relay.trim().to_lowercase();
    if relay.contains(':') {
        relay
    } else {
        format!("{relay}:{RELAY_PORT}")
    }
}

#[derive(Debug, Clone)]
struct Rule {
    net: IpNetwork,
    /// Normalised relay -> milliseconds.
    costs: HashMap<String, f64>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    /// Longest prefix first, so the first rule that contains an address is the
    /// most specific one.
    rules: Vec<Rule>,
}

/// The outcome of a decision, with enough to explain it.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// The relay exactly as it was configured, which is what clients are handed.
    pub relay: String,
    pub cost_ms: f64,
    /// Every healthy relay with its total, in the order they were given.
    pub costs: Vec<(String, f64)>,
}

impl RouteTable {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut rules: Vec<Rule> = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut tokens = line.split_whitespace();
            let net_text = tokens.next().unwrap_or("");
            let net: IpNetwork = net_text.parse().map_err(|_| {
                format!("line {n}: {net_text:?} is not a network such as 203.0.113.0/24")
            })?;
            if rules
                .iter()
                .any(|r| r.net.network() == net.network() && r.net.prefix() == net.prefix())
            {
                return Err(format!("line {n}: {net_text} appears twice"));
            }

            // Entries may be separated by commas, spaces, or both.
            let mut costs = HashMap::new();
            for entry in tokens.flat_map(|t| t.split(',')).filter(|e| !e.is_empty()) {
                let (relay, ms) = entry
                    .rsplit_once('=')
                    .ok_or_else(|| format!("line {n}: {entry:?} is not relay=milliseconds"))?;
                let ms: f64 = ms
                    .parse()
                    .map_err(|_| format!("line {n}: {ms:?} is not a number of milliseconds"))?;
                if !ms.is_finite() || ms < 0.0 {
                    return Err(format!(
                        "line {n}: {entry:?} must be a finite, non-negative time"
                    ));
                }
                if relay.trim().is_empty() {
                    return Err(format!("line {n}: {entry:?} has no relay name"));
                }
                if costs.insert(normalize(relay), ms).is_some() {
                    return Err(format!("line {n}: relay {relay} is listed twice"));
                }
            }
            if costs.is_empty() {
                return Err(format!("line {n}: {net_text} lists no relays"));
            }
            rules.push(Rule { net, costs });
        }
        if rules.is_empty() {
            return Err("no routes found".to_owned());
        }
        // Stable, so equal prefixes keep file order.
        rules.sort_by_key(|r| std::cmp::Reverse(r.net.prefix()));
        Ok(Self { rules })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// The most specific rule containing `ip`. An IPv4 address that arrives
    /// wrapped in IPv6, as it does on a dual-stack listener, is unwrapped first.
    fn lookup(&self, ip: IpAddr) -> Option<&Rule> {
        let ip = ip.to_canonical();
        self.rules.iter().find(|r| r.net.contains(ip))
    }

    /// The cheapest of `healthy` for a session between `a` and `b`, or `None` when
    /// the table knows neither address (or there is nothing healthy), in which case
    /// the caller falls back to its ordinary policy.
    ///
    /// A side the table does not know contributes nothing, so one known side is
    /// enough to decide. Ties go to the earliest in `healthy`. The answer does not
    /// depend on which of the two is the controller.
    pub fn choose(&self, healthy: &[String], a: IpAddr, b: IpAddr) -> Option<Choice> {
        let sides: Vec<HashMap<String, f64>> = [a, b]
            .into_iter()
            .filter_map(|ip| self.costs_for(ip))
            .collect();
        choose_from(healthy, &sides)
    }

    /// What the table says about one address: relay -> milliseconds, from the
    /// most specific line containing it, or `None` if no line does.
    pub fn costs_for(&self, ip: IpAddr) -> Option<HashMap<String, f64>> {
        self.lookup(ip).map(|r| r.costs.clone())
    }
}

/// The cheapest of `healthy` given what is known about each end of the session,
/// one map (relay -> milliseconds) per end that is known at all. `None` when
/// nothing is known about either end, or nothing is healthy. A relay an end's map
/// does not mention costs `UNLISTED_COST_MS` for that end. Ties go to the earliest
/// in `healthy`.
pub fn choose_from(healthy: &[String], sides: &[HashMap<String, f64>]) -> Option<Choice> {
    if healthy.is_empty() || sides.is_empty() {
        return None;
    }
    let costs: Vec<(String, f64)> = healthy
        .iter()
        .map(|relay| {
            let key = normalize(relay);
            let total = sides
                .iter()
                .map(|s| s.get(&key).copied().unwrap_or(UNLISTED_COST_MS))
                .sum();
            (relay.clone(), total)
        })
        .collect();
    let mut best = 0;
    for (i, (_, cost)) in costs.iter().enumerate() {
        if *cost < costs[best].1 {
            best = i;
        }
    }
    Some(Choice {
        relay: costs[best].0.clone(),
        cost_ms: costs[best].1,
        costs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn relays(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const TABLE: &str = "
        # home is near HK, school is near Shanghai
        203.0.113.0/24   hk.example.com:21117=38, sh.example.com:21117=95
        198.51.100.0/22  sh.example.com:21117=12  hk.example.com:21117=80
    ";

    #[test]
    fn a_session_costs_the_sum_of_both_legs() {
        let t = RouteTable::parse(TABLE).unwrap();
        let healthy = relays(&["hk.example.com:21117", "sh.example.com:21117"]);
        // home <-> school: hk = 38 + 80 = 118, sh = 95 + 12 = 107.
        let c = t
            .choose(&healthy, ip("203.0.113.7"), ip("198.51.100.9"))
            .unwrap();
        assert_eq!(c.relay, "sh.example.com:21117");
        assert_eq!(c.cost_ms, 107.0);
        assert_eq!(c.costs[0], ("hk.example.com:21117".to_owned(), 118.0));
        // home <-> home: hk = 76, sh = 190.
        let c = t
            .choose(&healthy, ip("203.0.113.7"), ip("203.0.113.200"))
            .unwrap();
        assert_eq!(c.relay, "hk.example.com:21117");
    }

    #[test]
    fn the_answer_does_not_depend_on_who_is_the_controller() {
        let t = RouteTable::parse(TABLE).unwrap();
        let healthy = relays(&["hk.example.com:21117", "sh.example.com:21117"]);
        let (a, b) = (ip("203.0.113.7"), ip("198.51.100.9"));
        assert_eq!(t.choose(&healthy, a, b), t.choose(&healthy, b, a));
    }

    #[test]
    fn the_most_specific_network_wins() {
        let t = RouteTable::parse(
            "10.0.0.0/8 a:1=50,b:1=10\n10.1.0.0/16 a:1=5,b:1=90\n10.1.2.0/24 a:1=70,b:1=7",
        )
        .unwrap();
        let healthy = relays(&["a:1", "b:1"]);
        let pick = |s: &str| t.choose(&healthy, ip(s), ip("192.0.2.1")).unwrap().relay;
        assert_eq!(pick("10.9.9.9"), "b:1"); // /8
        assert_eq!(pick("10.1.9.9"), "a:1"); // /16
        assert_eq!(pick("10.1.2.3"), "b:1"); // /24
    }

    #[test]
    fn a_catch_all_line_covers_roaming_addresses_and_loses_to_any_specific_line() {
        // The traveller's address is in no list; `0.0.0.0/0` and `::/0` say what to
        // do about any such address, and a specific line still wins where it applies.
        let t = RouteTable::parse(
            "203.0.113.0/24 hk:1=10,sh:1=50\n0.0.0.0/0 hk:1=0,sh:1=5\n::/0 hk:1=0,sh:1=5",
        )
        .unwrap();
        let healthy = relays(&["sh:1", "hk:1"]);
        let pick = |a: &str, b: &str| {
            t.choose(&healthy, ip(a), ip(b))
                .map(|c| (c.relay, c.cost_ms))
        };
        // Roamer to the office: the office's line and the catch-all add up. hk = 10, sh = 55.
        assert_eq!(
            pick("8.8.8.8", "203.0.113.5"),
            Some(("hk:1".to_owned(), 10.0))
        );
        // Office to office uses the office line twice, not the catch-all.
        assert_eq!(
            pick("203.0.113.5", "203.0.113.6"),
            Some(("hk:1".to_owned(), 20.0))
        );
        // Two roamers: hk = 0, sh = 10. A table without the catch-all gives no answer.
        assert_eq!(pick("8.8.8.8", "1.1.1.1"), Some(("hk:1".to_owned(), 0.0)));
        assert_eq!(
            pick("2001:db8::1", "2001:db8::2"),
            Some(("hk:1".to_owned(), 0.0))
        );
        let plain = RouteTable::parse("203.0.113.0/24 hk:1=10,sh:1=50").unwrap();
        assert_eq!(plain.choose(&healthy, ip("8.8.8.8"), ip("1.1.1.1")), None);
    }

    #[test]
    fn one_known_side_is_enough_and_two_unknown_sides_defer() {
        let t = RouteTable::parse(TABLE).unwrap();
        let healthy = relays(&["hk.example.com:21117", "sh.example.com:21117"]);
        let c = t
            .choose(&healthy, ip("203.0.113.7"), ip("192.0.2.1"))
            .unwrap();
        assert_eq!(c.relay, "hk.example.com:21117");
        assert_eq!(c.cost_ms, 38.0);
        assert!(t
            .choose(&healthy, ip("192.0.2.1"), ip("192.0.2.2"))
            .is_none());
    }

    #[test]
    fn only_healthy_relays_are_considered() {
        let t = RouteTable::parse(TABLE).unwrap();
        // The table prefers hk for home, but hk is down.
        let c = t
            .choose(
                &relays(&["sh.example.com:21117"]),
                ip("203.0.113.7"),
                ip("203.0.113.8"),
            )
            .unwrap();
        assert_eq!(c.relay, "sh.example.com:21117");
        assert!(t
            .choose(&[], ip("203.0.113.7"), ip("203.0.113.8"))
            .is_none());
    }

    #[test]
    fn a_relay_the_table_never_mentions_loses_to_one_it_does_but_can_still_win_alone() {
        let t = RouteTable::parse("10.0.0.0/8 a:1=900").unwrap();
        let c = t
            .choose(&relays(&["z:1", "a:1"]), ip("10.0.0.1"), ip("10.0.0.2"))
            .unwrap();
        assert_eq!(c.relay, "a:1"); // 1800 beats 2000
        let c = t
            .choose(&relays(&["z:1", "y:1"]), ip("10.0.0.1"), ip("10.0.0.2"))
            .unwrap();
        assert_eq!(c.relay, "z:1"); // both unlisted: the first, deterministically
    }

    #[test]
    fn a_relay_written_without_a_port_means_the_default_port() {
        let t = RouteTable::parse("10.0.0.0/8 HK.Example.com=5, sh.example.com:21117=50").unwrap();
        // hbbs may be started with `-r hk.example.com`.
        let c = t
            .choose(
                &relays(&["sh.example.com", "hk.example.com"]),
                ip("10.0.0.1"),
                ip("10.0.0.2"),
            )
            .unwrap();
        assert_eq!(c.relay, "hk.example.com");
    }

    #[test]
    fn ties_go_to_the_earliest() {
        let t = RouteTable::parse("10.0.0.0/8 a:1=10,b:1=10").unwrap();
        let c = t
            .choose(&relays(&["b:1", "a:1"]), ip("10.0.0.1"), ip("10.0.0.2"))
            .unwrap();
        assert_eq!(c.relay, "b:1");
    }

    #[test]
    fn ipv6_and_wrapped_ipv4_addresses_are_matched() {
        let t = RouteTable::parse("2001:db8::/32 a:1=5,b:1=50\n192.0.2.0/24 a:1=50,b:1=5").unwrap();
        let healthy = relays(&["a:1", "b:1"]);
        assert_eq!(
            t.choose(&healthy, ip("2001:db8::1"), ip("2001:db8::2"))
                .unwrap()
                .relay,
            "a:1"
        );
        // 192.0.2.9 as a dual-stack listener reports it.
        assert_eq!(
            t.choose(&healthy, ip("::ffff:192.0.2.9"), ip("192.0.2.10"))
                .unwrap()
                .relay,
            "b:1"
        );
    }

    #[test]
    fn bad_files_are_refused_with_the_line_number() {
        for (text, want) in [
            ("203.0.113.0/24", "line 1: 203.0.113.0/24 lists no relays"),
            ("nonsense a:1=5", "line 1: \"nonsense\" is not a network"),
            (
                "10.0.0.0/8 a:1",
                "line 1: \"a:1\" is not relay=milliseconds",
            ),
            ("10.0.0.0/8 a:1=fast", "line 1: \"fast\" is not a number"),
            ("10.0.0.0/8 a:1=-3", "must be a finite, non-negative"),
            ("10.0.0.0/8 a:1=NaN", "must be a finite, non-negative"),
            ("10.0.0.0/8 =5", "has no relay name"),
            ("10.0.0.0/8 a:1=5,a:1=6", "listed twice"),
            (
                "# fine\n10.0.0.0/8 a:1=5\n10.0.0.0/8 b:1=5",
                "line 3: 10.0.0.0/8 appears twice",
            ),
            ("# nothing here", "no routes found"),
        ] {
            let err = RouteTable::parse(text).unwrap_err();
            assert!(err.contains(want), "{text:?} gave {err:?}, wanted {want:?}");
        }
    }

    /// The file `relay_rtt.py routes` produced for the sample measurements in
    /// tools/relay-rtt. The Python tests pin it to what the tool really writes;
    /// this pins it to what this parser really reads, so neither side can drift.
    const GENERATED: &str = include_str!("../tools/relay-rtt/testdata/routes-sample.txt");

    #[test]
    fn the_file_the_measurement_tool_writes_is_the_file_this_parser_reads() {
        let t = RouteTable::parse(GENERATED).unwrap();
        // home, school (IPv4 and IPv6), and the two default lines.
        assert_eq!(t.rule_count(), 5);
        let healthy = relays(&[
            "hk.example.com:31107",
            "sh.example.com:31107",
            "fra.example.com:31107",
        ]);
        let pick = |a: &str, b: &str| t.choose(&healthy, ip(a), ip(b)).unwrap();

        // Both at home: hk 40 + 40 beats sh 96 + 96.
        assert_eq!(
            pick("203.0.113.5", "203.0.113.99").relay,
            "hk.example.com:31107"
        );
        // home <-> school: hk 40 + 82 = 122, sh 96 + 14 = 110.
        let c = pick("203.0.113.5", "198.51.100.7");
        assert_eq!(
            (c.relay.as_str(), c.cost_ms),
            ("sh.example.com:31107", 110.0)
        );
        // The school's IPv6 network is in the table too.
        assert_eq!(
            pick("2001:db8:1::7", "2001:db8:1::8").relay,
            "sh.example.com:31107"
        );
        // Two strangers fall through to the generated default.
        assert_eq!(pick("192.0.2.1", "192.0.2.2").relay, "sh.example.com:31107");
    }

    #[test]
    fn comments_blank_lines_and_trailing_comments_are_ignored() {
        let t = RouteTable::parse("\n# c\n\n10.0.0.0/8 a:1=5 # trailing\n").unwrap();
        assert_eq!(t.rule_count(), 1);
    }
}
