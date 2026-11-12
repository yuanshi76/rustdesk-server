//! WebRTC signalling for RustDesk clients 1.5.0 and later. Optional: off unless
//! `WEBRTC=Y`, and when off nothing here changes what hbbs does.
//!
//! Since 1.5.0 a controlling client can race a WebRTC connection (ICE, STUN,
//! DTLS) against its other ways of reaching a peer. hbbs only carries the
//! signalling; the session itself goes peer to peer, or through a TURN server
//! the clients are configured with. Three things pass through hbbs:
//!
//! - the controller's SDP offer, in `PunchHoleRequest` (field 12), which hbbs
//!   must copy into the `PunchHole` it sends the controlled peer (field 10);
//! - the peer's SDP answer, in `PunchHoleSent` (field 8), which hbbs must copy
//!   into the `PunchHoleResponse` to the controller (field 12). An answer in a
//!   `RelayResponse` (field 12) is forwarded as part of that message already;
//! - ICE candidates, `RendezvousMessage` field 29 (`IceCandidate`: id 1,
//!   socket_addr 2, session_key 3, candidate 4). The controller sends its own on
//!   its punch connection, naming the peer by `id`. The peer sends its own on a
//!   separate connection, naming the controller by the `socket_addr` it was
//!   given in the `PunchHole`, and hbbs answers on the controller's punch
//!   connection, which therefore has to stay open after the response.
//!
//! These fields are newer than the protocol hbb_common defines at the commit
//! upstream's server pins, so they are read and written here as raw protobuf
//! fields. hbb_common stays where upstream has it. When upstream adopts a
//! protocol with them, this module should switch to the generated fields.
//!
//! Abuse limits: candidates are forwarded only between the two ends of an offer
//! hbbs itself forwarded in the last `SESSION_TTL`, toward the controller only
//! from the peer's IP address, and at most `MAX_CANDIDATES` per direction.

use crate::common::get_arg;
use hbb_common::{
    log,
    protobuf::{well_known_types::empty::Empty, Message as _, UnknownFields, UnknownValueRef},
    rendezvous_proto::*,
    try_into_v4, AddrMangle,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const PUNCH_HOLE_REQUEST_OFFER: u32 = 12;
const PUNCH_HOLE_OFFER: u32 = 10;
const PUNCH_HOLE_SENT_ANSWER: u32 = 8;
const PUNCH_HOLE_RESPONSE_ANSWER: u32 = 12;
const RELAY_RESPONSE_ANSWER: u32 = 12;
const RENDEZVOUS_ICE_CANDIDATE: u32 = 29;

const ICE_ID: u32 = 1;
const ICE_SOCKET_ADDR: u32 = 2;
const ICE_SESSION_KEY: u32 = 3;
const ICE_CANDIDATE: u32 = 4;

/// How long after its offer a WebRTC attempt may still exchange candidates. The
/// client gives up on WebRTC within seconds; this only has to outlast that.
const SESSION_TTL: Duration = Duration::from_secs(60);
const MAX_SESSIONS: usize = 4096;
/// Per direction, per attempt. A client sends a few to a few dozen.
const MAX_CANDIDATES: u32 = 128;

/// One WebRTC attempt, keyed by the controller's punch connection.
struct Session {
    peer_id: String,
    peer_addr: SocketAddr,
    until: Instant,
    to_peer: u32,
    to_controller: u32,
}

/// An ICE candidate message, as much of it as routing needs.
pub(crate) struct Ice {
    id: String,
    socket_addr: Vec<u8>,
    /// The whole `IceCandidate`, forwarded as it came.
    raw: Vec<u8>,
}

pub(crate) enum IceRoute {
    /// Controller to peer: send to the peer's registered address.
    ToPeer(SocketAddr, RendezvousMessage),
    /// Peer to controller: send on the controller's punch connection.
    ToController(SocketAddr, RendezvousMessage),
    Drop(&'static str),
}

#[derive(Clone, Default)]
pub(crate) struct WebRtcSignal {
    enabled: bool,
    sessions: Arc<Mutex<HashMap<SocketAddr, Session>>>,
    stats: Arc<Stats>,
}

#[derive(Default)]
struct Stats {
    offers: AtomicU64,
    answers: AtomicU64,
    to_peer: AtomicU64,
    to_controller: AtomicU64,
    dropped: AtomicU64,
}

fn field(fields: &UnknownFields, number: u32) -> Option<Vec<u8>> {
    match fields.get(number) {
        Some(UnknownValueRef::LengthDelimited(v)) if !v.is_empty() => Some(v.to_vec()),
        _ => None,
    }
}

fn text(fields: &UnknownFields, number: u32) -> String {
    field(fields, number)
        .and_then(|v| String::from_utf8(v).ok())
        .unwrap_or_default()
}

fn key(addr: SocketAddr) -> SocketAddr {
    try_into_v4(addr)
}

fn same_ip(a: IpAddr, b: IpAddr) -> bool {
    a.to_canonical() == b.to_canonical()
}

impl WebRtcSignal {
    pub(crate) const HELP: &'static [&'static str] = &["webrtc(wr)"];

    pub(crate) fn from_env() -> Self {
        let enabled = get_arg("webrtc").eq_ignore_ascii_case("y");
        log::info!("WEBRTC={}", if enabled { "Y" } else { "N" });
        Self {
            enabled,
            ..Default::default()
        }
    }

    #[cfg(test)]
    fn enabled() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }

    /// The offer in a controller's request, if there is one and this is on.
    pub(crate) fn offer(&self, ph: &PunchHoleRequest) -> Option<Vec<u8>> {
        if !self.enabled {
            return None;
        }
        field(ph.special_fields.unknown_fields(), PUNCH_HOLE_REQUEST_OFFER)
    }

    /// Put the offer into the `PunchHole` going to the peer, and remember the
    /// attempt. A `FetchLocalAddr` (both ends on one LAN) has no field for it, so
    /// such an attempt carries on without WebRTC.
    pub(crate) fn forward_offer(
        &self,
        msg: &mut RendezvousMessage,
        offer: Vec<u8>,
        controller: SocketAddr,
        peer_id: &str,
        peer_addr: SocketAddr,
    ) {
        let Some(rendezvous_message::Union::PunchHole(ph)) = msg.union.as_mut() else {
            return;
        };
        ph.special_fields
            .mut_unknown_fields()
            .add_length_delimited(PUNCH_HOLE_OFFER, offer);
        self.stats.offers.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        if sessions.len() >= MAX_SESSIONS {
            sessions.retain(|_, s| s.until > now);
            if sessions.len() >= MAX_SESSIONS {
                return;
            }
        }
        sessions.insert(
            key(controller),
            Session {
                peer_id: peer_id.to_owned(),
                peer_addr,
                until: now + SESSION_TTL,
                to_peer: 0,
                to_controller: 0,
            },
        );
    }

    /// The answer in a peer's `PunchHoleSent`, if this is on.
    pub(crate) fn answer(&self, phs: &PunchHoleSent) -> Option<Vec<u8>> {
        if !self.enabled {
            return None;
        }
        field(phs.special_fields.unknown_fields(), PUNCH_HOLE_SENT_ANSWER)
    }

    pub(crate) fn forward_answer(&self, resp: &mut PunchHoleResponse, answer: Vec<u8>) {
        resp.special_fields
            .mut_unknown_fields()
            .add_length_delimited(PUNCH_HOLE_RESPONSE_ANSWER, answer);
        self.stats.answers.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether a `RelayResponse` carries an answer, i.e. its controller will go on
    /// exchanging candidates on its punch connection.
    pub(crate) fn relay_response_has_answer(&self, rr: &RelayResponse) -> bool {
        self.enabled && field(rr.special_fields.unknown_fields(), RELAY_RESPONSE_ANSWER).is_some()
    }

    /// Whether the controller at `controller` has a WebRTC attempt in progress, so
    /// its punch connection must survive the response.
    pub(crate) fn keeps_route(&self, controller: SocketAddr) -> bool {
        if !self.enabled {
            return false;
        }
        let now = Instant::now();
        self.sessions
            .lock()
            .map(|s| s.get(&key(controller)).is_some_and(|s| s.until > now))
            .unwrap_or(false)
    }

    /// The controller's punch connection closed.
    pub(crate) fn forget(&self, controller: SocketAddr) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(&key(controller));
        }
    }

    /// An `IceCandidate`, if `msg` is one and this is on. The union is unset for a
    /// message type this protocol version does not know; field 29 is it.
    pub(crate) fn ice(&self, msg: &RendezvousMessage) -> Option<Ice> {
        if !self.enabled || msg.union.is_some() {
            return None;
        }
        let raw = field(
            msg.special_fields.unknown_fields(),
            RENDEZVOUS_ICE_CANDIDATE,
        )?;
        let parsed = Empty::parse_from_bytes(&raw).ok()?;
        let fields = parsed.special_fields.unknown_fields();
        if field(fields, ICE_CANDIDATE).is_none() || field(fields, ICE_SESSION_KEY).is_none() {
            return None;
        }
        Some(Ice {
            id: text(fields, ICE_ID),
            socket_addr: field(fields, ICE_SOCKET_ADDR).unwrap_or_default(),
            raw,
        })
    }

    /// Where a candidate that arrived from `from` should go.
    pub(crate) fn route(&self, ice: Ice, from: SocketAddr) -> IceRoute {
        let route = self.route_inner(ice, from);
        let counter = match &route {
            IceRoute::ToPeer(..) => &self.stats.to_peer,
            IceRoute::ToController(..) => &self.stats.to_controller,
            IceRoute::Drop(_) => &self.stats.dropped,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        route
    }

    fn route_inner(&self, ice: Ice, from: SocketAddr) -> IceRoute {
        let now = Instant::now();
        let Ok(mut sessions) = self.sessions.lock() else {
            return IceRoute::Drop("internal error");
        };
        let mut msg = RendezvousMessage::new();
        msg.special_fields
            .mut_unknown_fields()
            .add_length_delimited(RENDEZVOUS_ICE_CANDIDATE, ice.raw);
        if !ice.id.is_empty() {
            // From the controller, on its punch connection, naming the peer.
            let Some(s) = sessions.get_mut(&key(from)) else {
                return IceRoute::Drop("no WebRTC attempt from this connection");
            };
            if s.until <= now {
                return IceRoute::Drop("WebRTC attempt expired");
            }
            if s.peer_id != ice.id {
                return IceRoute::Drop("not the peer this connection offered to");
            }
            if s.to_peer >= MAX_CANDIDATES {
                return IceRoute::Drop("too many candidates");
            }
            s.to_peer += 1;
            return IceRoute::ToPeer(s.peer_addr, msg);
        }
        if ice.socket_addr.is_empty() {
            return IceRoute::Drop("names neither a peer nor a controller");
        }
        // From the peer, naming the controller by the address it was given.
        let controller = key(AddrMangle::decode(&ice.socket_addr));
        let Some(s) = sessions.get_mut(&controller) else {
            return IceRoute::Drop("no WebRTC attempt for that controller");
        };
        if s.until <= now {
            return IceRoute::Drop("WebRTC attempt expired");
        }
        if !same_ip(from.ip(), s.peer_addr.ip()) {
            return IceRoute::Drop("not from the peer that was offered to");
        }
        if s.to_controller >= MAX_CANDIDATES {
            return IceRoute::Drop("too many candidates");
        }
        s.to_controller += 1;
        IceRoute::ToController(controller, msg)
    }

    /// The `webrtc` console command.
    pub(crate) fn console(&self, cmd: &str) -> Option<String> {
        let mut words = cmd.split_whitespace();
        if !matches!(words.next(), Some("webrtc" | "wr")) {
            return None;
        }
        let now = Instant::now();
        let active = self
            .sessions
            .lock()
            .map(|s| s.values().filter(|s| s.until > now).count())
            .unwrap_or(0);
        let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Some(format!(
            "WEBRTC: {}\nattempts in progress: {}\noffers forwarded: {}\nanswers forwarded: {}\n\
             candidates to peer: {}\ncandidates to controller: {}\ncandidates dropped: {}\n",
            if self.enabled { "Y" } else { "N" },
            active,
            n(&self.stats.offers),
            n(&self.stats.answers),
            n(&self.stats.to_peer),
            n(&self.stats.to_controller),
            n(&self.stats.dropped),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ice_bytes(id: &str, socket_addr: &[u8], session_key: &str, candidate: &str) -> Vec<u8> {
        let mut m = Empty::new();
        let f = m.special_fields.mut_unknown_fields();
        if !id.is_empty() {
            f.add_length_delimited(ICE_ID, id.as_bytes().to_vec());
        }
        if !socket_addr.is_empty() {
            f.add_length_delimited(ICE_SOCKET_ADDR, socket_addr.to_vec());
        }
        f.add_length_delimited(ICE_SESSION_KEY, session_key.as_bytes().to_vec());
        f.add_length_delimited(ICE_CANDIDATE, candidate.as_bytes().to_vec());
        m.write_to_bytes().unwrap()
    }

    fn ice_msg(raw: Vec<u8>) -> RendezvousMessage {
        let mut msg = RendezvousMessage::new();
        msg.special_fields
            .mut_unknown_fields()
            .add_length_delimited(RENDEZVOUS_ICE_CANDIDATE, raw);
        // Through the wire, as hbbs would receive it.
        RendezvousMessage::parse_from_bytes(&msg.write_to_bytes().unwrap()).unwrap()
    }

    fn punch_hole() -> RendezvousMessage {
        let mut m = RendezvousMessage::new();
        m.set_punch_hole(PunchHole::default());
        m
    }

    #[test]
    fn off_by_default_means_nothing_is_read_or_kept() {
        let w = WebRtcSignal::default();
        let mut req = PunchHoleRequest::default();
        req.special_fields
            .mut_unknown_fields()
            .add_length_delimited(PUNCH_HOLE_REQUEST_OFFER, b"offer".to_vec());
        assert!(w.offer(&req).is_none());
        let msg = ice_msg(ice_bytes("peer", b"", "k", "c"));
        assert!(w.ice(&msg).is_none());
        assert!(!w.keeps_route("1.2.3.4:5".parse().unwrap()));
    }

    #[test]
    fn candidates_flow_only_within_an_offered_attempt() {
        let w = WebRtcSignal::enabled();
        let a: SocketAddr = "198.51.100.7:40000".parse().unwrap();
        let b: SocketAddr = "203.0.113.9:21116".parse().unwrap();
        let stranger: SocketAddr = "192.0.2.1:5555".parse().unwrap();

        // Before any offer, nothing is routed.
        let ice = w.ice(&ice_msg(ice_bytes("peer-b", b"", "k", "c"))).unwrap();
        assert!(matches!(w.route(ice, a), IceRoute::Drop(_)));

        let mut ph = punch_hole();
        w.forward_offer(&mut ph, b"offer".to_vec(), a, "peer-b", b);
        assert!(w.keeps_route(a));
        let Some(rendezvous_message::Union::PunchHole(p)) = &ph.union else {
            panic!()
        };
        assert_eq!(
            field(p.special_fields.unknown_fields(), PUNCH_HOLE_OFFER).unwrap(),
            b"offer"
        );

        // Controller to its peer: yes. To another id, or from another connection: no.
        let ice = w.ice(&ice_msg(ice_bytes("peer-b", b"", "k", "c"))).unwrap();
        assert!(matches!(w.route(ice, a), IceRoute::ToPeer(to, _) if to == b));
        let ice = w
            .ice(&ice_msg(ice_bytes("someone-else", b"", "k", "c")))
            .unwrap();
        assert!(matches!(w.route(ice, a), IceRoute::Drop(_)));
        let ice = w.ice(&ice_msg(ice_bytes("peer-b", b"", "k", "c"))).unwrap();
        assert!(matches!(w.route(ice, stranger), IceRoute::Drop(_)));

        // Peer to controller: from the peer's IP (any port: it opens a new
        // connection for candidates) yes, from elsewhere no.
        let mangled = AddrMangle::encode(a);
        let from_b: SocketAddr = "203.0.113.9:61000".parse().unwrap();
        let ice = w.ice(&ice_msg(ice_bytes("", &mangled, "k", "c"))).unwrap();
        assert!(matches!(w.route(ice, from_b), IceRoute::ToController(to, _) if to == a));
        let ice = w.ice(&ice_msg(ice_bytes("", &mangled, "k", "c"))).unwrap();
        assert!(matches!(w.route(ice, stranger), IceRoute::Drop(_)));

        w.forget(a);
        assert!(!w.keeps_route(a));
        let ice = w.ice(&ice_msg(ice_bytes("", &mangled, "k", "c"))).unwrap();
        assert!(matches!(w.route(ice, from_b), IceRoute::Drop(_)));
    }

    #[test]
    fn candidates_per_direction_are_capped() {
        let w = WebRtcSignal::enabled();
        let a: SocketAddr = "198.51.100.7:40000".parse().unwrap();
        let b: SocketAddr = "203.0.113.9:21116".parse().unwrap();
        w.forward_offer(&mut punch_hole(), b"o".to_vec(), a, "peer-b", b);
        for _ in 0..MAX_CANDIDATES {
            let ice = w.ice(&ice_msg(ice_bytes("peer-b", b"", "k", "c"))).unwrap();
            assert!(matches!(w.route(ice, a), IceRoute::ToPeer(..)));
        }
        let ice = w.ice(&ice_msg(ice_bytes("peer-b", b"", "k", "c"))).unwrap();
        assert!(matches!(
            w.route(ice, a),
            IceRoute::Drop("too many candidates")
        ));
    }

    #[test]
    fn a_candidate_without_key_or_body_is_not_one() {
        let w = WebRtcSignal::enabled();
        let mut m = Empty::new();
        m.special_fields
            .mut_unknown_fields()
            .add_length_delimited(ICE_ID, b"peer".to_vec());
        assert!(w.ice(&ice_msg(m.write_to_bytes().unwrap())).is_none());
        // A known message type is never mistaken for one.
        let mut known = RendezvousMessage::new();
        known.set_punch_hole(PunchHole::default());
        known
            .special_fields
            .mut_unknown_fields()
            .add_length_delimited(RENDEZVOUS_ICE_CANDIDATE, ice_bytes("p", b"", "k", "c"));
        assert!(w.ice(&known).is_none());
    }
}
