# NOTES

Working log for rebasing `lejianwen/rustdesk-server` onto current upstream and
fixing the websocket socket leak. Every number below was measured in this
repository; nothing is quoted from the task brief without re-checking it.

Statement labels follow the brief: **VERIFIED** = measured here, **INFERRED** =
derived from something verified.

---

## Phase 0 — Divergence measurement

Measured 2026-09-16 against `fork/forapi` (`fb8b5b9`) and `upstream/master`
(`a7736be`).

| Metric | Value |
|---|---|
| Fork-only commits (`upstream/master..fork/forapi`) | **38** (30 non-merge) |
| Upstream commits the fork is missing (`fork/forapi..upstream/master`) | **43** |
| Diff in `src/` + `libs/` (`upstream/master...fork/forapi`) | **5 files, +483 / −132** |
| Whole-tree diff | 25 files, +1589 / −704 |

`src/` breakdown:

```
 libs/hbb_common          |   2 +-      (submodule pointer only)
 src/jwt.rs               |  54 +++++   (new file)
 src/lib.rs               |   1 +
 src/main.rs              |   3 +-
 src/rendezvous_server.rs | 555 ++++++++++++++++++++++++++-----------
```

**Decision gate result: PROCEED.** The source divergence is 483 added lines
across 5 files — the "few hundred lines across a handful of files" case, not the
"thousands of lines" case. It does touch the rendezvous loop, but as a bounded
set of additions (JWT check, `MUST_LOGIN`, TCP/WS registration, key exchange, ws
peer registry) rather than a rewrite.

Only 8 of the 30 non-merge fork commits touch `src/`; two more touch nothing but
the version in `Cargo.toml`, and the rest are docs, CI and Docker packaging.

### Version facts (VERIFIED, from the GitHub API and the trees)

| | Official | Fork |
|---|---|---|
| Latest release | `1.1.16` (2026-07-20) | `v0.1.2` (2025-09-01) |
| Default branch | `master` (`Cargo.toml` version `1.1.17`, unreleased) | `forapi` (`Cargo.toml` version `1.1.14`) |
| Last push | 2026-08-07 | 2025-09-01 |

The fork branched from the 1.1.14 era and has not been updated in ~12 months.

### Submodule situation (VERIFIED)

`.gitmodules` declares one submodule, `libs/hbb_common` →
`https://github.com/rustdesk/hbb_common`. All three pointers of interest live on
that same upstream repo — **the fork does not maintain its own hbb_common**:

| Tree | `libs/hbb_common` pointer | Date |
|---|---|---|
| upstream tag `1.1.16` | `83419b6` | 2025-03-06 |
| `fork/forapi` | `d6b1497` | 2025-08-27 |
| `upstream/master` | `69cea8d` | 2026-07-26 |

Ancestry is linear: `83419b6` → `d6b1497` → `69cea8d`. So the fork's pointer is
*newer* than the one the 1.1.16 release tag ships, and upstream master's is
newer still.

There is one wrinkle in the fork's history worth recording, because it breaks the
obvious `format-patch`/`am` workflow: commit `9bae660` ("feat: Add KeyExchange
and Encrypted tcpstream") committed `libs/hbb_common` as a **plain directory**
with local edits to `src/tcp.rs` and `protos/rendezvous.proto`, and commits
`5aabb03` / `95b18ee` later restored it as a submodule. The local hbb_common
edits were therefore discarded by the fork itself, and the final `forapi` tree
depends only on stock upstream `hbb_common@d6b1497`. Verified: `git diff
upstream/master...fork/forapi -- libs/` shows nothing but the submodule pointer.

Consequence: `git cherry-pick` of `9bae660` aborts with "untracked working tree
files would be overwritten" because the commit's tree has real files where the
current tree has a gitlink. See "Patch series construction" below for how this
was handled.

### Base selection: `upstream/master` (`a7736be`), not the `1.1.16` tag

The brief asks for "1.1.16 or later". `upstream/master` is later, and three
things on it matter:

1. `109d9a2` "fix more UDP reflection/amplification" — disables handling of
   `PunchHoleSent` and `LocalAddr` over UDP. Not in 1.1.16. This is a hardening
   fix for a publicly exposed port.
2. `hbb_common` `69cea8d` includes an upstream security fix (aligned buffer
   layout, hbb_common PR #574) and is a strict superset of `d6b1497`, which is
   what the fork's patches were written against. Basing on the `1.1.16` tag
   would have *regressed* the submodule pointer by five months relative to the
   fork.
3. `0d915e6` protobuf 3.7.2 and `91fb928` (i32 overflow: peers offline 24.9–49.7
   days reported online) — the latter is in 1.1.16 already.

`a7736be` is an immutable commit, so the base is still exactly reproducible.
`UPSTREAM_VERSION` records both the release tag the base descends from
(`1.1.16`) and the exact base commit.

Upstream master also already contains the fork's `09ff39c` "127.0.0.1 is not
loopback" fix, verbatim (`let ip = try_into_v4(addr).ip();` in
`handle_listener2`). That patch is therefore obsolete.

---

## Patch series construction

`git format-patch upstream/master..fork/forapi -o patches/` was produced for
triage and the result is recorded in `patches/INVENTORY.md`. The patches were
**not** replayed with `git am`, for two reasons, both recorded above:

* the `9bae660` de-submodularization episode makes the series unappliable as-is;
* upstream has since refactored the same regions of `src/rendezvous_server.rs`
  (UDP handlers removed, `PUNCH_REQS` dedupe added, `REG_TIMEOUT` retyped to
  `i64`), so the 1.1.14-era hunks do not correspond to current code.

Instead each KEEP feature was re-applied as one coherent commit on top of
`upstream/master`, committed with `--author` set to the original fork author and
an `Origin:` trailer naming the fork commit it derives from. Authorship is
preserved; the hunks are current. This is a deliberate deviation from the
brief's `format-patch`/`am` instruction and is the only one.

---

## Root cause of the websocket socket leak

**VERIFIED by reading `fork/forapi`'s `src/rendezvous_server.rs`; confirmed by
measurement in `tests/ws_leak.rs` (see "Leak measurements").**

Plain language: *the fork put half of each websocket connection into a global
map and never took it out again, so the operating system was never told to close
those connections.*

In detail. A `tokio_tungstenite::WebSocketStream` is `split()` into a read half
and a write half; the underlying `TcpStream` is closed only when **both** halves
are dropped. Upstream relies on this: `handle_listener_inner` owns both halves
for the lifetime of the connection, and where it does hand the write half
("sink") to a registry — `tcp_punch`, for delivering a punch-hole reply that
arrives on a different connection — it removes it again when the connection ends:

```rust
if sink.is_none() {
    self.tcp_punch.lock().await.remove(&try_into_v4(addr));
}
```

The fork added a *second* registry, `ws_map: Arc<Mutex<HashMap<SocketAddr,
Sink>>>`, so that a punch-hole request could be pushed to a peer that is only
reachable over its websocket. On a successful `RegisterPk` over ws it does:

```rust
if ws {
    // for ws, we can only get addr when register_pk
    if let Some(sink) = sink.take() {
        self.ws_map.lock().await.insert(try_into_v4(addr), sink);
    }
}
```

and it never added the matching removal. The only `ws_map.remove` in the fork is
in `handle_tcp_punch_hole_request`, i.e. it fires only if some *other* peer later
happens to punch a hole to this one.

So for every websocket connection that completes a `RegisterPk`:

1. the write half moves into `ws_map` and stays there for the process lifetime;
2. the read loop exits (see below), dropping the read half;
3. `sink.is_none()` is now true, so the cleanup that runs removes an entry from
   `tcp_punch` — a different map, which never had an entry for this address;
4. the write half in `ws_map` keeps the `TcpStream` alive. The client eventually
   gives up and sends FIN. The kernel moves the socket to `CLOSE_WAIT` and waits
   for a `close()` that never comes.

That is exactly the production census: `CLOSE_WAIT` accumulating **only** on
21118 (ws), none on 21116 (UDP rendezvous, no sinks involved), ~13 MiB/day of
heap for the retained sink buffers, and `CLOSE_WAIT` rather than `FIN_WAIT`
because the *client* is always the side that closes.

**The 4-minute reconnect churn has the same root cause.** `handle_tcp` returns
`bool`, and the caller breaks the read loop on `false`. The fork's new
`RegisterPeer` and `OnlineRequest` arms fall through to the function's trailing
`false`, so the server stops reading the websocket the first time a client sends
a heartbeat — while still holding the write half in `ws_map`. From the client's
side the connection goes silent but is never closed, so it times out and
reconnects, and each reconnect leaks one more socket. There is one bug here, not
two.

This is a **fork bug, not an upstream bug**. Upstream has no `ws_map`; its ws
connections own both halves and close cleanly. Nothing needs to be reported to
`rustdesk/rustdesk-server` for the leak itself.

### The fix

`ws_map` is replaced by `ws_peers: Arc<StdMutex<HashMap<SocketAddr, WsPeer>>>`,
which holds an `mpsc::UnboundedSender<RendezvousMessage>` — a *channel into* the
connection task — instead of the socket's write half. The invariant restored is
"one task owns the socket, for the whole life of the socket":

* the connection task keeps both halves and `select!`s between reading the
  socket and receiving pushed messages;
* the registry entry is removed by a `Drop` guard, so it goes away on normal
  exit, on error, and on panic;
* each entry carries a connection serial, so a reconnect from the same
  `ip:port` (or the same `ip:0` after `X-Real-IP` rewriting) cannot have its
  fresh entry removed by the older connection's cleanup;
* pushing to a peer whose receiver is gone removes the stale entry and falls
  back to the UDP path, as before;
* `handle_tcp` returns `true` for websocket connections after any successfully
  handled message, so heartbeats no longer tear down the connection;
* a websocket read-idle timeout (`WS_IDLE_TIMEOUT` seconds, default 90) bounds
  the lifetime of a connection whose peer has gone away silently, and a ws Ping
  is sent after 30 s of read idleness.

Defence in depth, per the brief: even if a future change leaked a registry
entry, the entry no longer owns a socket, so it can no longer leak an fd.

---

## Non-trivial conflict resolutions and semantic decisions

Recorded per patch in `patches/INVENTORY.md`. The ones that are semantic rather
than textual:

1. **`MUST_LOGIN` vs upstream's own auth.** Checked: upstream `a7736be` has no
   login-required feature and no JWT handling — `grep -i 'must_login\|jwt\|
   login'` over `upstream/master:src/` finds only the licence-key check in
   `handle_punch_hole_request`. The fork's `MUST_LOGIN` therefore does not
   collide with an upstream equivalent; it layers on top of the licence-key
   check, in the same function, after it.

2. **UDP `PunchHoleSent` / `LocalAddr` removal.** Upstream `109d9a2` disabled
   these over UDP. The fork (based on 1.1.14) still handles them. Keeping
   upstream's behaviour: the fork's patches do not touch those arms, and the
   supported path for the fork's own websocket clients is TCP/WS anyway.

3. **`REG_TIMEOUT` is now `i64`.** The fork's `peers_online_state` compares
   `elapsed` as `i32`. Re-applied against the `i64` type (upstream `91fb928`
   fixed a 24.9-day overflow here); using the fork's `i32` would have
   reintroduced that bug.

4. **`Sink` gained encryption state.** The fork wraps both sink variants to
   carry an optional `tcp::Encrypt`. With the registry now holding channels
   rather than sinks, encryption state stays inside the owning connection task,
   which is where it belongs — a pushed message is encrypted by the task that
   owns the key, not by whoever happened to call `push`.

5. **`get_symetric_key_from_msg` panicked on malformed input.** The fork's
   version does `ex.keys[0].to_vec().try_into().unwrap()` and
   `panic!("Error while opening the seal key")`, all reachable from an
   unauthenticated remote peer. Re-applied as fallible: length-checked, returning
   `None`, connection closed. Under the old code a panic mid-`handle_tcp` was
   also a leak path, since the sink was already in `ws_map`.

6. **`RequestRelay` was never routed to websocket-only peers.** The fork routed
   punch-hole requests through its websocket map but left `RequestRelay` going
   out over UDP via `Data::Msg`, which cannot reach a peer that only has a
   websocket. Found by `tests/smoke.rs`. Fixed with the same fallback the
   punch-hole path uses. This completes the websocket feature rather than adding
   a new one, so it is inside the brief's "no new features" line, but it is a
   deliberate change beyond what the fork shipped and is called out here for
   that reason.

---

## Leak measurements

`tests/ws_leak.rs`: 200 websocket connections, each registering and then closing
client-side; count this process's descriptors before and after.

| Build | Result | Descriptors leaked |
|---|---|---|
| `fork/forapi` (`fb8b5b9`) — the production artifact | **FAIL** | **200 of 200** (baseline 24 → 224) |
| `upstream/master` (`a7736be`), unmodified | PASS | 0 |
| `main` at `3f5362d` (fork's ws design re-applied, pre-fix) | **FAIL** | **200 of 200** (baseline 23 → 223) |
| `main` at `cfb1c6c` (fix applied) | PASS | 0 (baseline 22 → 22) |

The upstream row is the answer to Phase 3a: **the leak is the fork's, not
upstream's.** There is nothing to report to `rustdesk/rustdesk-server` about it.
One descriptor per connection, exactly, which is what a registry that takes the
write half and never gives it back should produce.

The upstream control run used the same test with one line relaxed: upstream
answers `NOT_SUPPORT` to a `RegisterPk` over a websocket, so the assertion
accepts `OK` or `NOT_SUPPORT` there. The connection still goes through the same
accept, handle and close path, which is what is being measured.

### Soak

`tests/ws_soak.rs` runs the release `hbbs` as a child process under websocket
churn and samples the child's RSS and descriptor count.

**The first version of this test passed against the leaking build**, which makes
it worthless, and it is worth recording why because both mistakes are easy to
repeat:

1. It registered a public key only on the first connection and heartbeated after
   that. The fork stashes the write half on a successful `RegisterPk` and
   nowhere else, so a heartbeat-only soak never touches the leaking path.
2. It reused one source address per client. The registry is keyed by address, so
   each registration *replaced* the previous entry and dropped the sink it was
   holding — the leak hid behind its own bookkeeping. In production every
   reconnect arrives from a fresh ephemeral port, so every connection is a
   distinct key and nothing is ever freed. This is also why the production
   census showed 2327 `CLOSE_WAIT` entries from only five client IPs: distinct
   ports, distinct keys.

Both were caught by running the soak against `fork/forapi` and seeing it pass.
Corrected, it registers on every connection from a distinct source address, at a
reconnect interval above the six seconds that the `RegisterPk` rate limiter
allows.

Two minutes, ten clients, 170 connections:

| Build | Descriptors | RSS |
|---|---|---|
| `fork/forapi` | 56 → 196 | 15488 → 17568 kB |
| this build | 25 → 25 | 15232 → 15296 kB |

The fork's curve is the production sawtooth, about 25× faster because the churn
is about 25× faster.

### Soak results

Sixty minutes against the release `hbbs` as a child process, ten synthetic
clients, **5070 connections**, 120 samples, commit `24a424c` — whose `src/` is
identical to the tip except `map_or(false, f)` written as `is_some_and(f)`.

```
fds   25 -> 25          (first quarter mean 25.0, last quarter mean 25.0)
RSS   15376 -> 17392 kB (first quarter mean 15659, last quarter mean 17364)
```

**The descriptor count did not move once in an hour.** Compare the same test
against the fork, where it climbed 56 → 196 in two minutes.

Live census of the same process at ~52 minutes, through its own console, next to
the production census from the brief:

```
                        this build (5070 conns)   production (~29 h)
sockets in CLOSE-WAIT            0                      2327
sockets LISTEN                   3                         7
ws peers registered              0                         -
ip-blocker entries            4390                         -
```

The RSS is the only number that is not flat. **An earlier version of this section
attributed all of it to `IP_BLOCKER` ("the whole of the growth"). That was an
inference from the entry count, and a control run on 2026-10-04 does not support
it.** What is actually known:

| Run | Connections | RSS | Descriptors |
|---|---|---|---|
| 60 min, every connection a new source address (macOS) | 5070 | 15376 -> 17392 kB (+2.0 MB, ~0.4 kB per connection) | 25 -> 25 |
| 10 min in CI (Linux) | 850 | 11654 -> 11986 kB (+0.3 MB) | 18, with two single-sample spikes |
| **control**: 20 min, each client cycling through a **bounded** pool of 50 addresses, so the per-IP table cannot grow past 500 entries (macOS) | 1690 | 14768 -> 15344 kB (+0.6 MB): flat for the first ~12 min, then a step | 25 -> 25 |

If `IP_BLOCKER` were all of the growth, the control's RSS should have stopped
rising once its pool filled, about six minutes in. It kept rising, by a similar
amount per connection. So `IP_BLOCKER` is not the whole explanation, and I do not
know what the rest is. It is stepwise rather than steady, which is also what a
malloc that does not return pages looks like on macOS, and 20 minutes is too short
and RSS too coarse to tell slow growth from that.

What can be said, INFERRED: even taking the worst slope seen (~0.4 kB per
connection) at the production host's observed rate of about 2300 connections a day
gives under 1 MB a day, against the 13 MB a day of the leak. From a ~15 MB start
that is roughly 200 days to reach the ~205 MB at which `hbbs` was OOM-killed. That
is arithmetic on two short runs, not a measurement of production, and it assumes
the fixed build no longer churns connections at the old rate.

What would settle it: a bounded-pool run of several hours on **Linux**, which is
what production runs and where RSS behaves differently, or a heap profile. Not
done. `SOAK_IP_POOL=50` in `tests/ws_soak.rs` is the control to use.

The descriptor count is the leak signal, and it did not move in any run.

`ws peers: 0` is the registry the fix rewrote, empty after thousands of
connections, which is the property the whole change is about.

### Images, built locally

All three Dockerfiles were built here for `linux/amd64` and `linux/arm64` with
`docker buildx` (Colima on an arm64 Mac), rather than trusted to CI. That caught
a real error: `ARG API_IMAGE` was declared inside the s6 builder stage, but a
Dockerfile ARG is only usable in a `FROM` if it is declared in the global scope
before the first `FROM`, so `docker/Dockerfile.api` failed outright with
`base name (${API_IMAGE}) should not be blank`. Fixed and rebuilt.

What has **not** been run here: the GitHub Actions workflows themselves. They
need a GitHub repository, so their first real execution will be in the user's
repo. The parts that could be checked locally were: every workflow file parses
as YAML, the Dockerfiles build on both platforms, the rebase mechanic the
watcher depends on (below), and the whole test suite the CI jobs invoke.

### Upstream-watch rebase, simulated

The watcher's core mechanic was exercised locally rather than trusted: a
synthetic upstream release was built by committing a change to
`src/rendezvous_server.rs` on top of `a7736be` and tagging it `1.1.17-sim`, then

```
git rebase --onto 1.1.17-sim $UPSTREAM_COMMIT sim-sync
```

replayed all 17 fork commits cleanly, with the synthetic upstream commit
preserved in the history below them. The workflow around it — the GitHub API
call, the PR, the issue on conflict — has not been executed, because that needs
a GitHub repository to run in.

---

## Test inventory

| File | What it holds the line on |
|---|---|
| `tests/ws_leak.rs` | The leak itself. 200 connections, descriptor count back to baseline. |
| `tests/ws_soak.rs` | Same property over an hour against a real process, with RSS. `#[ignore]`d. |
| `tests/ws_features.rs` | A websocket survives repeated heartbeats, answers an online query, and receives a pushed punch-hole request without losing its connection. |
| `tests/ws_idle.rs` | A silent websocket is closed rather than held forever. |
| `tests/must_login.rs` | `MUST_LOGIN=Y` refuses no token, a non-JWT token, and a token signed with the wrong secret; lets a valid one through. |
| `tests/jwt_tests.rs` | Bad signature, expiry and garbage are rejected. |
| `tests/key_exchange.rs` | The two-phase encrypted TCP rendezvous: phase 1 is signed by the server key, a tampered signature does not verify, and a message encrypted with the sealed symmetric key round-trips. Also four malformed phase 2 messages, each of which reached a `panic!` or an `unwrap()` in the fork's version. |
| `tests/smoke.rs` | Key generation and validation, the four expected listeners, and both halves of the rendezvous handshake - direct punch hole and relay - end to end over websockets against a real `hbbs` and `hbbr`. |

What the smoke test does **not** cover: a real RustDesk client establishing a
session, direct or relayed. That needs the client, which was not available here.
The synthetic exchange drives the same server code paths in the same order, but
it is not the same evidence, and it should not be reported as if it were.

---

## Deviations from the brief

1. The patch series is **not** replayed with `git am`. Reasons in "Patch series
   construction"; authorship is preserved with `--author` plus an `Origin:`
   trailer on every re-applied commit.
2. The base is `upstream/master` (`a7736be`), not the `1.1.16` tag. Reasons in
   "Base selection".
3. `main` deliberately contains one commit that fails its own test (`3f5362d`),
   so that "the regression test fails against the pre-fix build" is reproducible
   forever with `git checkout 3f5362d && cargo test --test ws_leak`, on this
   base and not only against the year-old fork.
4. Three small changes beyond a straight re-application, each because leaving
   them alone would have shipped a defect: the JWT secret is no longer printed
   to stdout, the KeyExchange handler no longer panics on remote input, and
   `RequestRelay` is routed to websocket peers.
5. One dependency was added that this crate's code does not use: `openssl` with
   the `vendored` feature. `hbb_common` at `69cea8d` pulls `native-tls`, and
   cross's musl images ship no OpenSSL for the target, so without it the CI
   cross-builds do not compile at all. See the commit message for the two
   independent confirmations.

---

## Part B groundwork (2026-10-04)

The plan (`rustdesk-improvement-plan-v2.md`, brief v2) came from a separate chat
session that worked from a different snapshot of the server (upstream commit
`bac9548`, partly truncated) and said to re-check everything against 1.1.16. All of
it below was re-checked against *this* tree: upstream `a7736be` plus the series.
Labels as before: VERIFIED = read or run here, INFERRED = drawn from verified
facts.

### Relay selection: the plan's claims against the code

| Plan says | Here |
|---|---|
| Round-robin; both peers' IPs ignored | VERIFIED. `get_relay_server(&self, _pa, _pb)`; one global counter. |
| Health check every 3 s, only with more than one relay | VERIFIED, with two details the plan lacks. The probe is a bare TCP connect (`FramedStream::new`), so every `hbbr` takes a connection every 3 s. And the live list is replaced only when the healthy subset is **non-empty**: if every relay fails the check, the previous list, dead nodes included, stays in use. |
| `rs` sets the list at runtime | VERIFIED. `Data::RelayServers0` -> `parse_relay_servers` sets the configured and the live list at once; the next tick narrows it. |
| Geo scaffolding: unknown | VERIFIED: **none**. `reload-geo(rg)` appears in the help text and has no handler; `test-geo(tg)` just calls `get_relay_server`. No geo or MaxMind code in `src/`. |
| Admin interface reachable how? | VERIFIED: loopback sources only, on the NAT-test port (ID port - 1), via `handle_listener2` and `is_loopback()`. See "Operability" below for what that means per image. |
| A second call site in the `RelayResponse` arm | VERIFIED, and narrower than the plan says. It fires only when `rr.relay_server == local_ip`, and `local_ip` is `""` unless `--mask` is set, while the arm's own guard requires `relay_server` to be non-empty. So **without `--mask` it cannot fire**, and neither can the first call site's LAN override (`peer_is_lan ^ is_lan`), which needs the mask too. |
| No WebRTC fields in the OSS server | VERIFIED. Nothing in `src/` or the pinned `hbb_common` protos. |

### What this does to the "pinning" requirement

INFERRED from verified facts. `hbbr` pairs a session's two TCP connections itself,
by uuid (the `PEERS` map in `relay_server.rs`); `hbbs` is not on the data path. So
an established session cannot be moved by anything `hbbs` does: changing the relay
list or the scores affects new connection attempts only, by construction.

That makes the plan's acceptance test - "change the relay list and scores during a
session; the session keeps flowing through the same node" - pass on **any** build,
the stock one included. It cannot tell a working pin from no pin. What pinning can
protect is consistency within **one attempt**: the first decision against the
`RelayResponse` substitution (reachable only with `--mask`), and a client retry
with a fresh uuid, which is a second attempt and a second selection. The brief's
other test, that the pin test must fail on a build whose second call site still
round-robins, is the discriminating one; the "established session" test should be
kept as documentation, not as evidence. Worth confirming with the user that this
is what they meant to protect before any of it is built.

### Secure TCP (Phase 7)

Compared with upstream PR #689 (+51/-8, open since Jul 29) and #706 (+429/-16,
open, updated Sep 26). Neither is merged, so nothing supersedes ours and ours
stays. Both upstream PRs generate the ephemeral key pair **per connection**;
the fork generated it once per process, despite a comment saying otherwise.

The wire format was checked against the real client, not a PR's description:
`rustdesk/src/common.rs` (`key_exchange`, `create_symmetric_key_msg`) and
`hbb_common` main (`set_negotiated_key`, `kx_version_for`).

- VERIFIED by reading: the client takes the first frame, requires exactly one
  signed key, verifies it against the server's long-term key, answers with
  `[its ephemeral pk, key sealed under a zero nonce]`, which is what
  `Encrypt::decode` inverts.
- VERIFIED by reading: against a server that advertises no version (ours), the
  client computes `picked = min(0, 1) = 0` and calls `set_key(key)`, i.e.
  `Encrypt::new(key)`, the scheme this server uses. **Kx v1 needs no server change.**
- VERIFIED by reading, and new: a set top bit on the offered X25519 key makes the
  client demand `signed_params`, which a v0 server never sends. libsodium never
  produces such a key; the test now asserts it so a later change cannot quietly
  break v1 clients.
- The pinned `hbb_common` (`69cea8d`) has none of the v1 API; `hbb_common` main
  does. `src/tcp.rs` - the stream `Encrypt` and its nonce counters - is
  byte-identical between the fork's old pointer (`d6b1497`) and ours, so the bump
  carries no cipher change. The three "symmetric crypt" commits in that range only
  touch `config.rs`, the local config encryption.

Two defects found in the ported handshake and fixed, each with a test that failed
first (commit `09cdcc7`):

1. **Not ephemeral.** One key pair per process, so every connection was offered the
   same key. Now one per connection, spent on that connection's first frame.
2. **Receive state lived on the sink.** A `PunchHoleRequest` moves the sink into
   `tcp_punch` while the read loop keeps going, and from then on the loop found no
   sink, skipped decryption and parsed ciphertext as protobuf. The brief says
   encryption state must persist on the held connection; the reply side did, the
   receive side did not. Receive state now lives in the loop and send state with
   the sink: one owner per direction, so nonce counters cannot desynchronise, and
   no `Arc<Mutex<..>>` as #706 needed.

**Not verified: any real client session.** What exists is a reading of the client's
source and a test that replays its logic. The 1.4.x / 1.5.0 matrix and the
`--deploy` check in Phase 7 need real clients, and are blocked on that.

### Per-IP registration limiter (upstream behaviour, found by a failed control run)

`check_ip_blocker` counts registrations per source address and refuses the 31st
unless a full 60 s passes with none counted: the timestamp is refreshed on every
counted request, so an address that registers more often than once a minute is
blocked after 31, and recovers only after a minute of quiet. VERIFIED: a soak that
reused one address per client, registering every 7 s, was refused at 210 s = 30
registrations, and every driver died.

INFERRED, not observed: anything that makes many clients present one address is
exposed to this. Two cases matter here. A reverse proxy in front of the websocket
port that does not forward `X-Real-IP` makes every client look like the proxy.
And many clients behind one NAT, such as a school, share an address. The first
needs the proxy to pass the header, which this server already trusts
unvalidated (upstream issue #634, so the port must not be reachable directly).

### `hbb_common`

Pinned `69cea8d` (2026-07-26), 315 commits past the pointer tag 1.1.16 ships. They
include security fixes: zstd output cap, aligned-allocation layout, `BytesCodec`
`reserve` bound, symmetric-nonce fixes in config encryption. No WebRTC fields;
Stage B of Phase 10 would need a deliberate bump to a revision that has them.

### Operability: reaching the admin console

VERIFIED against the published `v0.2.0` images (arm64):

| Image | Route |
|---|---|
| s6 (busybox) | `docker exec <c> sh -c "printf 'ws-peers' \| nc -w 2 127.0.0.1 21115"` works. Through the published port it is correctly refused: the source is not loopback inside the container. |
| classic (`FROM scratch`) | `docker exec` cannot run **anything**: no shell, no `nc`; the image holds three binaries. Works with a throwaway container sharing the namespace: `docker run --rm --network container:<name> busybox:stable sh -c "printf '...' \| nc -w 2 127.0.0.1 21115"`. |

So on the classic image - the reference deployment - the console, and with it `rs`
(the drain step in Phase 8), `ws-peers`, `ib` and `must-login`, is unreachable by
the obvious route. Documented in `docs/environment-variables.md`.

### The independent chat run

A second session ran the same brief in another environment and left its notes.
Where it overlaps this repo it agrees: same root cause in the same code (`ws_map`;
the exit path cleaned `tcp_punch` but not `ws_map`), the same observation that
only the `RegisterPk` path leaks (idle, `RegisterPeer`, `PunchHoleRequest` and
`RequestRelay` do not), and a regression test that fails before and passes after.
Its fix is narrower (a connection id on each `ws_map` entry) than the channel
design used here, and it did not reach the rebase, the soak on a real process, the
workflows or the images. It listed as open things already done here: the
heartbeat falling through to `false`, the `ws_map` entry being consumed by the
first push, the JWT secret printed by `generate_token`, the secure-TCP handshake
untested.

One of its observations was acted on: `MUST_LOGIN=Y` without
`RUSTDESK_API_JWT_KEY` accepts any non-empty token. Behaviour unchanged, since
refusing to start would break setups that run that way; hbbs now warns at startup
and when `must-login Y` is sent to the console.

### The measurement tool, and this machine

`tools/relay-rtt/` (Phase 9). Reviewed, fixed and tested; see its README. Running
it against real hosts rather than loopback found that **the machine used for this
session has a fake-IP TUN proxy active**: `github.com` resolved to `198.18.0.50`
and `1.1.1.1` and `8.8.8.8` "answered" in 0.3-0.4 ms, which is the proxy answering
locally. The tool now warns on that address range. The campaign must not be run
from a machine in that state.

### What is still open

| Item | State |
|---|---|
| Part A item 6: upstream-watch opens a PR on a simulated release | **Unproven.** Query, comparison, keepalive and the PAT have run for real; the rebase-and-open-PR path has not. Needs a throwaway tag, branch and PR in the repository, which is the owner's call. |
| Phase 6: report the leak | Drafted (`docs/leak-report.md`), unfiled. The leak is the fork's, not upstream's. |
| Phase 7 matrix, `--deploy` check, Phase 10 | Need real 1.4.x and 1.5.0 clients. |
| Phase 9 campaign | Tool ready. Needs the real node list and the sites to run from. Everything in Phase 8 waits on it, per the plan's own decision rule: if the best node is within ~20% of the second best for every pair that matters, relay selection is not worth building. |
| Phase 8 | Not started, deliberately. Read-first is done (above). |
