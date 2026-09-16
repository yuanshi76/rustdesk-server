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
| `main` at `cb5cd1f` (fork's ws design re-applied, pre-fix) | **FAIL** | **200 of 200** (baseline 23 → 223) |
| `main` at `363c664` (fix applied) | PASS | 0 (baseline 22 → 22) |

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
clients, **5070 connections**, 120 samples, commit `09cb4d9` — whose `src/` is
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

The RSS is the only number that is not flat, and it is `IP_BLOCKER`, not a leak.
That map is keyed by source IP and is pruned only after a day, and this test
gives *every connection* a distinct source address on purpose — that is what
makes the leak visible at all. 4390 entries for 4400-odd connections at the
moment of the census, roughly 2 MB, which is the whole of the growth. Production
sees five client IPs and would accumulate five entries. Measured rather than
assumed: the console's `ib` command printed the count.

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
3. `main` deliberately contains one commit that fails its own test (`cb5cd1f`),
   so that "the regression test fails against the pre-fix build" is reproducible
   forever with `git checkout cb5cd1f && cargo test --test ws_leak`, on this
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
