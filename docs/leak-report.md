# Draft issue for `lejianwen/rustdesk-server`

Not filed. This is the text to paste, if and when you want to report it. The
leak is in the fork, not in `rustdesk/rustdesk-server` — unmodified upstream
passes the reproduction below — so there is nothing to file against upstream for
this bug.

---

**Title:** `hbbs` leaks one socket per websocket connection (`ws_map` entries are
never removed)

**Body:**

### Summary

Every websocket connection that completes a `RegisterPk` leaks one file
descriptor for the lifetime of the process. On a small self-hosted server this
is about 2300 sockets and 13 MiB of heap per day, ending in an OOM kill every
12–20 days.

### Where

`src/rendezvous_server.rs`, in the websocket support added in `872f1f9`.

On a successful `RegisterPk` over a websocket, the connection's write half is
moved into a process-global map:

```rust
if ws {
    // for ws, we can only get addr when register_pk
    if let Some(sink) = sink.take() {
        self.ws_map.lock().await.insert(try_into_v4(addr), sink);
    }
}
```

The only `ws_map.remove` in the tree is in `handle_tcp_punch_hole_request`, so
the entry is removed only if some *other* peer later happens to punch a hole to
this one. There is no removal when the connection ends.

A `tokio_tungstenite::WebSocketStream` that has been `split()` keeps its
`TcpStream` alive until **both** halves are dropped. The read half is dropped
when `handle_listener_inner` returns; the write half sits in `ws_map` forever.
The cleanup that does exist,

```rust
if sink.is_none() {
    self.tcp_punch.lock().await.remove(&try_into_v4(addr));
}
```

removes from `tcp_punch` — a different map, which never had an entry for this
address.

Result: the client eventually gives up and sends FIN, the kernel parks the
socket in `CLOSE_WAIT`, and `hbbs` never calls `close()`.

### Related: the reconnect churn

`handle_tcp` returns `bool` and the caller breaks the read loop on `false`. The
`RegisterPeer` and `OnlineRequest` arms fall through to the function's trailing
`false`, so the server stops reading a websocket the first time the client sends
a heartbeat — while still holding the write half. From the client's side the
connection goes silent but is never closed, so it times out and reconnects, and
each reconnect leaks another socket. Same root cause, not a second bug.

### Evidence from a production host

Alibaba Cloud VPS, 2 vCPU, `MemTotal: 918016 kB`, kernel 5.10.134, 14 registered
peers.

```
Out of memory: Killed process 1962599 (hbbs)
  total-vm:229068kB, anon-rss:205064kB, file-rss:1504kB
```

Socket census ~29 h after a restart:

```
fd=2616   (2613 sockets; limit 1048576)
2327 CLOSE-WAIT
 219 ESTAB
   7 LISTEN
```

`CLOSE_WAIT` by local port: **2336 on 21118** (the websocket listener), **zero on
21116**. Five legitimate client IPs, not the Docker bridge and not the API
container. Host memory climbed linearly ~13–18 MB/day and dropped back to
exactly its starting value whenever the stack was recreated.

### Reproduction

200 websocket connections that register and then close; count the process's
descriptors before and after.

| Build | Descriptors leaked |
|---|---|
| `lejianwen/rustdesk-server@forapi` | **200 of 200** |
| `rustdesk/rustdesk-server@master`, unmodified | 0 |

One per connection, exactly.

### Fix

Put a channel into the connection's task in the registry instead of the socket's
write half, so one task owns the socket for its whole life; remove the entry from
a `Drop` guard so it goes away on every exit path including a panic; tag each
entry with a connection serial so a reconnect that has claimed the same key is
not removed by the older connection's cleanup; and return `true` from
`handle_tcp` for websockets so a heartbeat no longer ends the read loop.

I have this working, with a regression test and an hour-long soak, and am happy
to open a PR.
