# RustDesk Server — rebased fork

A build of `hbbs` / `hbbr` that carries the features of
[`lejianwen/rustdesk-server`](https://github.com/lejianwen/rustdesk-server) on
top of current upstream [`rustdesk/rustdesk-server`](https://github.com/rustdesk/rustdesk-server),
with the websocket socket leak fixed.

[![ci](../../actions/workflows/ci.yaml/badge.svg)](../../actions/workflows/ci.yaml)

## Why this exists

The fork's websocket support put one half of every websocket connection into a
global map and never took it out. A split `WebSocketStream` keeps its socket
open until both halves are dropped, so each websocket that registered leaked one
file descriptor: the client eventually sent FIN, the kernel parked the socket in
`CLOSE_WAIT`, and `hbbs` never called `close()`.

On a 1 GB host that was about 2300 sockets and 13 MiB of heap per day, all on
port 21118, and an OOM kill every 12 to 20 days. The same bug is why clients
reconnected every few minutes: the server stopped reading the socket after the
first heartbeat while still holding the write half.

`tests/ws_leak.rs` opens 200 websockets and asserts the descriptor count comes
back to where it started. Against the fork it fails, 200 leaked out of 200;
against this build it passes with zero. It runs on every push.

A one-hour soak against the release binary, 5070 connections: descriptor count
flat at 25 the entire time, and the server's own console reported zero sockets
in `CLOSE_WAIT` and an empty websocket registry. The same soak against the fork
climbs 56 → 196 descriptors in two minutes.

[`NOTES.md`](NOTES.md) has the full diagnosis, the measurements, and every
decision taken during the rebase. [`patches/INVENTORY.md`](patches/INVENTORY.md)
says what was kept from the fork, what was dropped, and why.

## What this build adds to upstream

| | |
|---|---|
| `MUST_LOGIN` | Refuse a connection request that carries no login token |
| `RUSTDESK_API_JWT_KEY` | Verify that token as an HS256 JWT, for use with [rustdesk-api](https://github.com/lejianwen/rustdesk-api) |
| Websocket clients | `RegisterPk` / `RegisterPeer` / `OnlineRequest` over TCP and websockets, for clients 1.4.1 and newer |
| Encrypted TCP rendezvous | `KeyExchange`, then a secretbox-encrypted connection |
| `WS_IDLE_TIMEOUT` | Close a websocket that has gone silent |

Everything else is upstream's. The base is recorded in
[`UPSTREAM_VERSION`](UPSTREAM_VERSION), and `.github/workflows/upstream-watch.yaml`
opens a pull request when upstream publishes a new release.

## Images

Published to GHCR, multi-arch (`linux/amd64`, `linux/arm64`):

| Image | Contents |
|---|---|
| `ghcr.io/<owner>/rustdesk-server` | The binaries, nothing else. One process per container. |
| `ghcr.io/<owner>/rustdesk-server-s6` | `hbbs` + `hbbr` under s6 in one container |
| `ghcr.io/<owner>/rustdesk-server-s6-api` | The above plus the `lejianwen/rustdesk-api` server on 21114 |

Tags are `<upstream release>-<short revision>`, for example `1.1.16-a1b2c3d`.
**Pin one.** `latest` is never moved by an automated build, so it may be older
than you expect.

`docker-compose.yml` and `docker-compose-s6.yml` are starting points; replace
`OWNER` and the image tag.

## Publishing your own images

This repository has no `origin` remote: `fork` is `lejianwen/rustdesk-server`
and `upstream` is `rustdesk/rustdesk-server`, both read-only. To publish:

```bash
gh repo create <you>/rustdesk-server --private --source=. --remote=origin
git push -u origin main
```

Then tag a release. The `build` workflow runs the test suite, cross-compiles for
`x86_64` and `aarch64` musl, and pushes all three images to GHCR:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Or run it by hand from the Actions tab, which also offers the `move_latest`
option.

Do not push with `--tags`. This repository carries upstream's whole tag history
(`1.1.16`, `1.1.15`, …) and every one of them matches the `build` workflow's tag
trigger, so pushing them all would start a build per tag. Push the one tag you
mean. Nothing is needed beyond the repository itself — GHCR authenticates with
the built-in `GITHUB_TOKEN`. Two optional extras:

- **`UPSTREAM_SYNC_TOKEN`** (a PAT with `repo` and `workflow` scope): lets the
  upstream-watch PR trigger CI. Without it the PR is still opened, just
  unchecked, and the job logs a warning saying so.
- Make the GHCR packages public in the repository's package settings if you want
  to pull them without logging in.

## Tests

```bash
cargo test                                                  # includes the leak regression
cargo test --release --test ws_soak -- --ignored --nocapture # one-hour soak
```

`tests/smoke.rs` starts a real `hbbs` and `hbbr` with a generated key pair and
drives both halves of the rendezvous handshake - direct and relayed - over
websockets.

## License

AGPL-3.0, unchanged from upstream. Upstream copyright notices and the fork's
commit authorship are preserved.

---

Upstream's own README follows.

# RustDesk Server Program

[![build](https://github.com/rustdesk/rustdesk-server/actions/workflows/build.yaml/badge.svg)](https://github.com/rustdesk/rustdesk-server/actions/workflows/build.yaml)

[**Download**](https://github.com/rustdesk/rustdesk-server/releases)

[**Manual**](https://rustdesk.com/docs/en/self-host/)

[**Configuration & environment variables**](docs/environment-variables.md)

[**FAQ**](https://github.com/rustdesk/rustdesk/wiki/FAQ)

[**How to migrate OSS to Pro**](https://rustdesk.com/docs/en/self-host/rustdesk-server-pro/installscript/#convert-from-open-source)

Self-host your own RustDesk server, it is free and open source.

> [!IMPORTANT]
> **Need more features?** [RustDesk Server Pro](https://rustdesk.com/pricing.html) might suit you better.
>
> **Want to develop your own server?** Start with [rustdesk-server-demo](https://github.com/rustdesk/rustdesk-server-demo), a simpler starting point than this repository.

## How to build manually

```bash
cargo build --release
```

Three executables will be generated in target/release.

- hbbs - RustDesk ID/Rendezvous server
- hbbr - RustDesk relay server
- rustdesk-utils - RustDesk CLI utilities

You can find updated binaries on the [Releases](https://github.com/rustdesk/rustdesk-server/releases) page.

## Configuration

`hbbs` and `hbbr` can be configured with command-line flags, environment
variables, or an `.env` / config file. Run `hbbs --help` or `hbbr --help` to see
the available flags.

The most common options:

| Option | Flag | Env var | Applies to | Purpose |
| --- | --- | --- | --- | --- |
| Key | `-k` | `KEY` | hbbs, hbbr | `hbbs` loads/generates one by default |
| Bind address | `-b` | `BIND` | hbbs, hbbr | Local IP address to listen on (default: all interfaces; requires 1.1.17+) |
| Port | `-p` | `PORT` | hbbs, hbbr | Listening port (hbbs `21116`, hbbr `21117`) |
| Relay servers | `-r` | `RELAY-SERVERS` | hbbs | Override when the relay uses a different address or a non-standard port |
| Force relay | — | `ALWAYS_USE_RELAY` | hbbs | `Y` disables direct connections |
| Require login | `--must-login` | `MUST_LOGIN` | hbbs | Fork feature: `Y` refuses a connection request with no token |
| API JWT secret | — | `RUSTDESK_API_JWT_KEY` | hbbs | Fork feature: shared secret the login token is verified against |
| Websocket idle timeout | `--ws-idle-timeout` | `WS_IDLE_TIMEOUT` | hbbs | Fork feature: seconds before a silent websocket is closed (default `90`) |
| Log level | — | `RUST_LOG` | hbbs, hbbr | e.g. `debug` (default `info`) |

See **[docs/environment-variables.md](docs/environment-variables.md)** for the
full list of variables, the file/flag/env precedence rules, database and relay
bandwidth tuning, Docker image variables, and examples.

## Installation

Please follow this [doc](https://rustdesk.com/docs/en/self-host/rustdesk-server-oss/)
