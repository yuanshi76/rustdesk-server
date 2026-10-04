# Client compatibility matrix (Phase 7)

The one part of the brief that cannot be done without real clients. 32 cells; fill
in the result for each. A cell either connects, or has a documented expected
failure, and nothing is left blank.

**Server under test:** `ghcr.io/yuanshi76/rustdesk-server*:` ______  (tag)
**Clients:** 1.4.x ______ (exact version)   1.5.0 ______ (exact version; it was a
pre-release on Sep 25, so re-run on the final build, and note that macOS clients
can update themselves silently)

Legend: `ok` connected and usable · `FAIL` did not connect · `n/a` combination
cannot occur (say why) · add what the client reported.

| Client | Signed in | UDP | WebSocket | Direct | Relayed |
|---|---|---|---|---|---|
| 1.4.x | yes | on  | on  |  |  |
| 1.4.x | yes | on  | off |  |  |
| 1.4.x | yes | off | on  |  |  |
| 1.4.x | yes | off | off |  |  |
| 1.4.x | no  | on  | on  |  |  |
| 1.4.x | no  | on  | off |  |  |
| 1.4.x | no  | off | on  |  |  |
| 1.4.x | no  | off | off |  |  |
| 1.5.0 | yes | on  | on  |  |  |
| 1.5.0 | yes | on  | off |  |  |
| 1.5.0 | yes | off | on  |  |  |
| 1.5.0 | yes | off | off |  |  |
| 1.5.0 | no  | on  | on  |  |  |
| 1.5.0 | no  | on  | off |  |  |
| 1.5.0 | no  | off | on  |  |  |
| 1.5.0 | no  | off | off |  |  |

## What each column means, and how to set it

- **Signed in** - the client is logged in to the API server, so it sends a token
  with its connection request. Only meaningful with `MUST_LOGIN=Y` or
  `RUSTDESK_API_JWT_KEY` set on the server; run both a `MUST_LOGIN=N` and a
  `MUST_LOGIN=Y` pass for the signed-in rows.
- **UDP off** - block UDP from the client to the server at a firewall. That does
  not depend on any client option name. The client should fall back to TCP, which
  is where the secure-TCP handshake matters: it is the path that failed with
  "Failed to secure tcp: deadline has elapsed" against a stock `hbbs`.
- **WebSocket** - the client option is `allow-websocket` (confirmed in the
  client's `hbb_common` source). The server's websocket listener is ID port + 2,
  so publish it, and expect it to be reached through a reverse proxy in practice:
  see the `X-Real-IP` caveat in `NOTES.md` before exposing it directly.
- **Direct / Relayed** - which path the session actually took, as the client's
  connection indicator shows. To force a relayed session the client has a
  force-relay switch; appending `/r` to the ID is the usual way (UNVERIFIED here,
  from memory, not read in the source).

## What to look at while you do it

On the server, per cell:

```bash
docker logs <hbbs> 2>&1 | tail -n 30
# websocket peers currently registered, via the console (see docs/environment-variables.md)
```

`ws peers` should rise when a websocket client registers and **fall back when it
disconnects**. A count that only climbs is the leak again.

Record any cell that behaves differently from what the table's neighbours suggest,
even if it connects.

## Not covered here

WebRTC (Phase 10), the `--deploy` flow, and Kx v1 against a live 1.5.0 client. The
last was checked by reading the client's source and replaying its logic in a test
(see `NOTES.md`, "Secure TCP"); a passing 1.5.0 row above would be the first real
evidence.
