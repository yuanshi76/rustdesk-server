# STUN and TURN servers for WebRTC

WebRTC connections ([webrtc.md](webrtc.md)) need a **STUN server**: a small public
service a device asks "what address do you see me coming from?". With that answer
each device can tell the other where to reach it through its router. STUN carries no
session data and needs almost no bandwidth.

A **TURN server** is optional. It relays a WebRTC session when no direct path exists, which
makes it a relay like your own `hbbr` servers, using its own bandwidth. Since RustDesk
already falls back to your relays, most setups do not need one.

These are settings of the **clients**, not of `hbbs`. Each device asks its own STUN servers,
so the setting belongs on every device that uses WebRTC.

## What the clients use by default

With nothing configured, RustDesk 1.5.x uses four public STUN servers (from the client's
source, `DEFAULT_ICE_SERVERS`):

```
stun:stun.cloudflare.com:3478
stun:stun.l.google.com:19302
stun:stun.antisip.com:3478
stun:stun.nextcloud.com:443
```

If at least one of them answers from a device's network, WebRTC can work there and you need
to configure nothing. **Check that first** (see [Test from each place](#test-from-each-place)).
Some of these may be unreachable from mainland China.

## The `ice-servers` option

The list is the client option **`ice-servers`**: entries separated by commas.

| Entry | Meaning |
|---|---|
| `stun:host:port` | a STUN server; the port defaults to 3478 |
| `stun:[2001:db8::1]:3478` | an IPv6 address must be in brackets |
| `turn://user:password@host:port` | a TURN server, with its user name and password |
| `turns://user:password@host:port` | the same over TLS |

Rules, from the client's source:

- **Any `stun:` entry replaces the four defaults**; it is not added to them. If you want one
  of the public servers as a backup to your own, list it too.
- With only `turn:` entries (or nothing), the defaults stay and your TURN servers are added.
- A `turn:` entry **without a user name and password is ignored**. Write it as
  `turn://user:password@host:port`; the short `turn:host:port` form has nowhere for them.
- A malformed entry is ignored with a warning in the client's log; the others still work.

### Setting it on a device

The settings screen has **no field** for this option; the switch "Enable WebRTC P2P
connection" (Settings, General, Other) turns WebRTC on, but the server list is set from
the command line, on an **installed** client, **as administrator**. Run the command once per
device; to show the current value, run it without the last argument.

```powershell
# Windows: in a Command Prompt or PowerShell opened "as administrator"
& "C:\Program Files\RustDesk\rustdesk.exe" --option ice-servers "stun:stun.cloudflare.com:3478,stun:stun.l.google.com:19302"
```

```bash
# macOS
sudo /Applications/RustDesk.app/Contents/MacOS/RustDesk --option ice-servers "stun:stun.cloudflare.com:3478,stun:stun.l.google.com:19302"

# Linux
sudo rustdesk --option ice-servers "stun:stun.cloudflare.com:3478,stun:stun.l.google.com:19302"
```

To go back to the defaults, set it to an empty value: `--option ice-servers ""`.

The command-line form is taken from the client's source (`--option` writes the setting for
the installed service); it has not been run on every platform here. Phones have no command
line, so they use the defaults.

## Third-party STUN servers

Well-known public ones, free and without an account:

| Provider | Entry |
|---|---|
| Cloudflare | `stun:stun.cloudflare.com:3478` |
| Google | `stun:stun.l.google.com:19302` (also `stun1` to `stun4.l.google.com:19302`) |

Two from different providers are better than two from one: if one provider's servers are
unreachable from a network, its others usually are too.

```
stun:stun.cloudflare.com:3478,stun:stun.l.google.com:19302
```

## Your own STUN server

You already run servers your devices can reach, so one of them can answer STUN as well.
**coturn** does this in a few megabytes of memory. A STUN-only setup holds no secrets and
relays nothing.

On the main server or a relay machine, as `docker-compose-stun.yml`:

```yaml
services:
  stun:
    container_name: stun
    image: coturn/coturn:4.18.0
    # Host networking: STUN answers with the address a request came from, and the
    # host's own network stack is the simplest way to keep UDP and those addresses
    # exactly as they arrive.
    network_mode: host
    command: -n --stun-only --listening-port=3478 --no-tls --log-file=stdout
    restart: unless-stopped
```

```bash
docker compose -f docker-compose-stun.yml up -d
docker logs stun 2>&1 | tail -5
```

Then **open UDP port 3478** in the machine's firewall and in the cloud provider's security
group (on Alibaba Cloud: the instance's security group, inbound UDP 3478).

On the devices:

```
stun:stun.example.com:3478,stun:stun.cloudflare.com:3478
```

with your server's address first, and a public one after it as a backup.

This exact command line was run with coturn 4.18.0 and answered STUN requests. Two
options found in older guides, `--no-cli` and `--no-dtls`, are **rejected** by this
version: coturn prints its help and exits, so the container stops at once. Check with
`docker ps` that it stays up.

### Adding TURN (optional)

Only if you want WebRTC to work even where nothing direct does, and are prepared for the
bandwidth. Replace the `command:` line:

```yaml
    command: >
      -n --listening-port=3478 --no-tls --log-file=stdout
      --lt-cred-mech --realm=stun.example.com
      --user=rustdesk:CHANGE-THIS-PASSWORD
      --min-port=49160 --max-port=49200
```

and open UDP 3478 and UDP 49160-49200. The devices get:

```
stun:stun.example.com:3478,turn://rustdesk:CHANGE-THIS-PASSWORD@stun.example.com:3478
```

The user name and password are stored on every device and can be read there; anyone who
has them can relay traffic through your server. Use a password made for this alone.

This setup was tested with coturn's own client: 40 messages relayed and all 40 returned,
each allocation logged as a success, and a wrong password refused.

## Test from each place

`tools/stun/stun_check.py` (Python 3.8+, no installation) sends a STUN request to each server
and shows the public address it reports:

```bash
curl -fsSLO https://raw.githubusercontent.com/yuanshi76/rustdesk-server/main/tools/stun/stun_check.py
python3 stun_check.py                                              # the clients' four defaults
python3 stun_check.py stun:stun.example.com:3478                   # your own
python3 stun_check.py --from-option "stun:a.example.com:3478,stun:stun.cloudflare.com:3478"
```

```
  ok    stun:stun.cloudflare.com:3478                270 ms   sees you as 203.0.113.5:34879
  FAIL  stun:stun.example.com:3478                  no answer within 3 s
```

Run it on the devices themselves, at home, at work and on the phone's network if you can,
**with any VPN or proxy off**. A `FAIL` for your own server usually means UDP 3478 is closed
in a firewall or security group. If working servers report different public addresses for
the same machine, a VPN or proxy is in the way.

TURN entries are listed and skipped; test TURN by enabling WebRTC and connecting. The
server-side counters are in the `webrtc` console command ([webrtc.md](webrtc.md)).
