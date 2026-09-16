# Patch inventory

Triage of `git format-patch upstream/master..fork/forapi` (32 patches, produced
2026-09-16 from `fork/forapi` = `fb8b5b9`, base `upstream/master` = `a7736be`).

Buckets, as defined by the task brief:

* **KEEP** — still needed, no upstream equivalent.
* **OBSOLETE** — upstream now does this; dropped.
* **SUPERSEDED** — upstream does something similar but differently; reconciled.
* **PACKAGING** — CI / Dockerfile / s6 / README; handled separately from source.

The patches are kept in this directory as the record of what the fork contained.
They are **not** replayed with `git am` — see "Patch series construction" in
`../NOTES.md` for why. Each KEEP item below names the commit that re-applies it
on the new base; those commits carry the original author and an `Origin:`
trailer.

---

## KEEP

| Patch | Fork commit | What it does | Why it is still needed |
|---|---|---|---|
| 0001 + 0029 (part) | `9bae660`, `b5e0484` | `KeyExchange` handling on the TCP listener: server signs a per-process Curve25519 public key with the server secret key, client seals a symmetric key to it, subsequent frames on that TCP connection are `secretbox`-encrypted. | Upstream `a7736be` still answers `NOT_SUPPORT` to anything that needs it and has no `KeyExchange` arm in `handle_tcp`; clients configured for encrypted TCP rendezvous depend on it. Re-applied with the panicking paths made fallible. 0029 contributes the missing inbound half — decrypting frames read off an encrypted TCP connection — without which the feature only worked in one direction. |
| 0009 + 0019 | `e5ac30e`, `d095ac8` | `OnlineRequest` over TCP/WS (upstream only answers it on the dedicated port-21115 listener), factored into `peers_online_state`. | Web and websocket clients have no way to ask "who is online" otherwise. 0019 is the de-duplication of the copy 0009 introduced; the two are applied as one commit. |
| 0010 | `4e37dc8` | `MUST_LOGIN` env var and `--must-login` flag; refuses a punch-hole request that carries no token; `must-login(ml)` console command. | Upstream has no login-required feature at all (`grep -i login upstream/master -- src/` is empty). This is the fork's headline feature. |
| 0014 | `bc980a3` | `src/jwt.rs`; when `RUSTDESK_API_JWT_KEY` is set, the punch-hole token is validated as an HS256 JWT. Adds `chrono`'s `serde` feature. | Pairs with `lejianwen/rustdesk-api`; without it `MUST_LOGIN` only checks that *some* token is present. `jsonwebtoken = "8"` is already an upstream dependency. Re-applied with the secret no longer printed to stdout. |
| 0025 | `872f1f9` | `RegisterPk` / `RegisterPeer` accepted over TCP and WS (upstream: UDP only), and a registry so the server can push a punch-hole request to a peer that is only reachable over its websocket. | This is the "client websocket support (client ≥ 1.4.1)" feature. **The registry is the source of the socket leak** and was re-implemented rather than copied — see `../NOTES.md`. The protocol-visible behaviour is unchanged. |

## OBSOLETE

| Patch | Fork commit | Why dropped |
|---|---|---|
| 0020 | `09ff39c` | Upstream carries the identical fix: `handle_listener2` already does `let ip = try_into_v4(addr).ip();` before `is_loopback()`. |
| 0007 | `bb45a62` | Removed a log line from `libs/hbb_common/src/tcp.rs` during the period when hbb_common was checked in as plain files. That edit no longer exists in the fork's own tree. |
| 0026, 0027 | `5aabb03`, `95b18ee` | Restore `libs/hbb_common` as a submodule after 0001 had de-submodularized it. Nothing to do on a base that never lost the submodule. |
| 0029 (part) | `b5e0484` | The `cargo fmt` and dead-code half: whitespace, trailing commas, `Option<(SocketAddr)>` → `Option<SocketAddr>`, and deletion of a commented-out `handle_tcp_punch_hole_request` draft. The re-applied code is already formatted. The *other* half of this patch is functional and is KEEP — see 0001. |
| 0002 | `10c2398` | Changes the fork's workflows from `toolchain: "1.70.0"` to `toolchain: stable`. It edits workflow files that do not exist on this base; the new CI makes its own toolchain choice. |
| 0013, 0017, 0031, 0032 | `42b836d`, `a11e363`, `f85f691`, `fb8b5b9` | Fork version bookkeeping (`Cargo.toml` back to 1.1.14, fork tag scheme). Replaced by the versioning scheme in `UPSTREAM_VERSION` + `VERSION`. |

## SUPERSEDED

| Patch | Fork commit | Upstream's version | Reconciliation |
|---|---|---|---|
| part of 0025 | `872f1f9` | Upstream `109d9a2` deliberately stopped handling `PunchHoleSent` and `LocalAddr` over UDP (reflection/amplification). The fork, on a 1.1.14 base, still handles them. | Upstream's behaviour wins. The re-applied websocket patch does not touch those arms. |
| part of 0009 | `e5ac30e` | Upstream `91fb928` retyped `REG_TIMEOUT` to `i64` to fix peers offline 24.9–49.7 days being reported online. | `peers_online_state` re-applied against `i64`; the fork's `i32` comparison would have reintroduced the overflow. |
| 0018, 0030 | `9ee99f1`, `4e63c83` | The fork's `build-test.yaml` / build tweaks predate upstream's current `build.yaml` and `ghcr.yml`. | New workflows written on upstream's as the base — see PACKAGING. |

## PACKAGING

Handled as its own commit, not as source patches.

| Patch | Fork commit | Disposition |
|---|---|---|
| 0003 | `a2b4e8e` | **Kept.** s6 service definition for `apimain`, `docker/Dockerfile` based on `lejianwen/rustdesk-api`, port 21114 exposed. Preserved as the *optional* `:s6-api` image variant; the default image no longer inherits from a third-party base. |
| 0021 | `2975151` | **Kept**, folded into the s6-api image: `WORKDIR /app`, `VOLUME /app/data` — required by `apimain`'s own layout. |
| 0004, 0005, 0006 | `b317b9e`, `7ff5eee`, `14198e7` | **Obsolete as written** (they edit the fork's old workflow files to drop i386). The i386 exclusion itself is kept: the new CI builds `linux/amd64` and `linux/arm64` only. |
| 0008, 0011, 0012, 0015, 0016, 0028 | various | README/doc churn, including deleting `README-ZH.md`. Not replayed; the new `README.md` documents the merged build from scratch and upstream's translations are left as upstream has them. |
| 0022, 0023, 0024 | `4ff3d2d`, `f8a6393`, `3d85ecc` | "chore: build fix" against workflow files that no longer exist here. Dropped. |
