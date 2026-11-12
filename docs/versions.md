# Which version to run, and staying on v0.4.x

| Line | Status | What it is |
|---|---|---|
| **v0.4.x** | **stable**, recommended | Geo routing, the routing table, `use-relay`, the monthly database updater, upstream's fixes. Latest: **v0.4.2**. Fixes continue on the `release/0.4` branch. |
| **v0.5.x** | **preview** | v0.4.2 plus optional WebRTC signalling ([webrtc.md](webrtc.md)), not yet proven with real clients. Marked as a pre-release on GitHub. |

v0.5.0 changes nothing unless you set `WEBRTC=Y`, but it is a new line, so v0.4.x stays
available and supported until v0.5.x has been in real use for a while.

## Pin a version

Always name the version in the image, never `latest`:

```yaml
    image: ghcr.io/yuanshi76/rustdesk-server:v0.4.2
```

Every release's images are tagged once and **never moved or deleted**: `v0.4.2` will
always be the same image. (`latest` does not move on its own either, but naming the
version is what makes a deployment repeatable.)

## Switching between v0.4.x and v0.5.x

Change the image tag on the machines you want to switch, then recreate:

```bash
docker compose up -d --force-recreate
```

Both directions work. v0.4.2 and v0.5.0 use the same key files, the same `db_v2.sqlite3`
and the same settings files; nothing on disk changes between them. The only new setting in
v0.5.0, `WEBRTC`, is ignored by v0.4.x. Relay machines (`hbbr` only) gain nothing from
v0.5.0 and can stay on v0.4.2 either way.

## Keeping your own copy

The images live on GitHub's container registry and each release also carries its Linux
binaries. If you want a copy that does not depend on GitHub at all, save the image you run
to a file:

```bash
docker pull ghcr.io/yuanshi76/rustdesk-server:v0.4.2
docker save ghcr.io/yuanshi76/rustdesk-server:v0.4.2 | gzip > rustdesk-server-v0.4.2.tar.gz
```

and later, on any machine with Docker:

```bash
gunzip -c rustdesk-server-v0.4.2.tar.gz | docker load
```

`docker save` stores the image for the machine's own architecture (amd64 or arm64); save on
each kind of machine you run. Keep the file with your backups ([backup.md](backup.md)).

The source of every release is the git tag of the same name, so it can always be built
again:

```bash
git clone https://github.com/yuanshi76/rustdesk-server.git
cd rustdesk-server
git checkout v0.4.2
git submodule update --init --recursive
cargo build --release --locked
```

The binaries are then in `target/release/` (`hbbs`, `hbbr`, `rustdesk-utils`). The
release's own zip files on GitHub hold the same binaries, built for Linux.

## How the lines are maintained

- `main` carries the newest line (v0.5.x).
- `release/0.4` carries v0.4.x. A fix that matters to v0.4.x users (a security fix from
  upstream, for example) is applied there too and released as v0.4.3, v0.4.4, ...
- Tags are never moved or deleted once a release is published.
