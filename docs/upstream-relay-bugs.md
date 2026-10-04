# Two relay bugs in upstream `rustdesk-server`, ready to file

Not filed. Both exist in unmodified upstream (checked at `a7736be`, the 1.1.16
release plus later fixes), not just in this fork. Both were found while writing
tests for relay routing, and each has a test that failed first. Neither involves the
fork's features, so they are good candidates for upstream.

The fork's leak, by contrast, is the fork's own code: see `leak-report.md`.

---

## Issue 1: `hbbs` never learns that a relay died, for a connection that is already open

**Title:** The healthy-relay list is a per-connection snapshot, so long-lived
websocket connections keep handing out dead relays

**Where:** `src/rendezvous_server.rs`. The live list is `relay_servers:
Arc<RelayServers>` (field, around line 87). The health check replaces it on the main
server object (`Data::RelayServers(rs) => { self.relay_servers = Arc::new(rs); }`,
around line 273). Each accepted connection is handled on `let mut rs = self.clone();`
(around lines 1124 and 1165), which copies that `Arc` as it was when the connection
opened.

**What happens:** a connection that lives a long time keeps the relay list it was
born with. The websocket listener makes long-lived connections normal (a client keeps
one open), and a `PunchHoleRequest` on it calls `get_relay_server` on that stale
copy. If a relay stops answering, the health check removes it from the main list
within three seconds, and that connection keeps naming it indefinitely.

**Reproduce:** start `hbbs` with `-r A,B` (two reachable TCP listeners), open a
websocket, stop A, wait for the next health check, then send `PunchHoleRequest`
over the already-open websocket. It can still be told A.

**Fix:** share the list instead of copying it. `Arc<Mutex<Arc<RelayServers>>>` (read
by cloning the inner `Arc`, replaced as a whole) is enough; no connection logic
changes.

---

## Issue 2: the health check returns surviving relays in network completion order

**Title:** `check_relay_servers` reorders the relay list, so `-r` order is not
preserved after the first health check

**Where:** `check_relay_servers` in the same file. It probes every relay in its own
spawned task, and each task does `rs.lock().await.push(x)` on success; the result is
then taken as the new list.

**What happens:** the tasks finish in whatever order the network allows, so the list
comes back in completion order, not configured order. With four relays the first
check returned them as 3, 2, 1, 4. Round-robin then walks a list that is reshuffled
every three seconds, and `-r a,b,c` cannot be used to express a preference.

**Reproduce:** start `hbbs` with `-r` listing four reachable relays, wait three
seconds, and send `relay-servers` to the console.

**Fix:** push `(index, relay)` and sort by index before replacing the list.

---

## Evidence

Both are covered by `tests/relay_routing.rs` in this repository
(`a_relay_that_stops_answering_is_not_chosen_whatever_the_table_says` and
`healthy_relays_stay_in_the_order_they_were_configured`), each of which failed before
the fix. Issue 1's test needs three relays: with only one left, the picker returns it
without consulting anything else, so a two-relay test cannot show the difference.
