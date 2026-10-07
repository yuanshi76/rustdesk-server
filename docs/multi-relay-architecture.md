# How several relays fit together: machines, ports, keys, the API server

This is the plain version. For choosing which relay is used see
[geo-routing.md](geo-routing.md) (by where a device is) and
[switching-relays.md](switching-relays.md) (one relay for everyone).

## Who runs what

| Machine | Runs | Does |
|---|---|---|
| **Main server** | `hbbs`, usually `hbbr` too, the API server if you use it | Knows every device, introduces two devices to each other, **chooses the relay** for each session. |
| **Other relay servers** | **only `hbbr`**, from the same image | Pass encrypted data between the two devices of a session. They know nothing about devices, IDs, the main server or each other. |

Devices need only the main server's address and key. They are handed a relay
address by the main server for each session. Relays hold no state worth backing up.

## Ports

- **Main server:** 21116 (tcp and udp), 21115, 21118, and 21117 and 21119 for its own
  `hbbr`. In the three-container compose file, `hbbr` and the API server use
  `network_mode: "service:hbbs"`, which makes them share `hbbs`'s network. That is why
  all the port mappings are written under `hbbs`: 21117 and 21119 in that list belong
  to `hbbr`. `hbbs` is not listening anywhere else.
- **Each other relay server:** its relay port (21117 by default) and its websocket port
  (the relay port plus 2, 21119 by default), reachable from the internet. If you map
  them to other host ports, such as 31107, then **that** host port is the address you
  write in `hbbs`'s relay list. Open both in the machine's firewall and cloud security
  group.
- **Main server to relays:** every 3 seconds `hbbs` opens a plain outbound connection
  to each relay's relay port to see whether it is alive, and stops using a relay that
  does not answer. So the relay port must be reachable **from the main server's
  container** as well as from your devices. A relay that is open to devices but closed
  to the main server is treated as down and is never chosen. Nothing else runs between
  the main server and the relays, and nothing needs to.

## Keys

Only `hbbs` has a real key pair (`id_ed25519` and `id_ed25519.pub` in its data folder).
Clients hold its **public** key in their **Key** field, and the encrypted handshake is
with `hbbs`. A relay does not have an identity of its own.

A relay's `-k` is a password: a string it compares with the key a client presents in its
relay request, and a client presents the key in its own Key field, which is `hbbs`'s
public key.

| Relay started with | Result |
|---|---|
| no `-k` (the default) | Accepts any client. Anyone who learns the relay's address can use its bandwidth. |
| `-k <full text of hbbs's id_ed25519.pub>` | Accepts only clients whose Key is that text. **Use this on relay servers.** |
| `-k _` or `-k -` | Makes `hbbr` load, or create, a key pair named `id_ed25519` **in its own folder**. On the main server that folder is shared with `hbbs`, so it is the same key and works. **On any other machine it is a different key, and every client is refused** with `Relay authentication failed ... invalid key` in the relay's log. Do not use it on remote relays. |

Neither program creates a new key daily. The key file is created once, when it does not
exist, and reused. A relay container with no persistent folder creates a new one at
every restart, which would look like that.

## The API server

The API server does not talk to `hbbs` or `hbbr` for any of this, and relays change
nothing for it.

- **Device IDs:** a client makes its ID and registers it with `hbbs`, which stores it in
  `db_v2.sqlite3`. Relays never see IDs, only a one-time code per session.
- **Online status:** `hbbs` knows a device is online because the device checks in every
  15 seconds. The API server keeps its own device list from what clients report to it.
  Neither depends on which relay a session used.
- **The API's "relay server" setting is one value. Leave it empty** with several relays.
  Anything in it is copied into the config strings it hands out, and a machine with its
  own relay set ignores the main server's choice.
- **The "server control" page** reaches only the `hbbs` and `hbbr` on its own machine, over
  loopback. For a remote relay use `docker exec` on that machine.
- **Login tokens** are checked by `hbbs` on its own; see [api-server.md](api-server.md).

## What this plan does not do

- **It does not know whether a relay works for a particular device.** `hbbs` checks relays
  only from the main server. If the chosen relay answers there but one device cannot
  reach it, sessions for that device keep going to it, because nothing tells `hbbs` they
  failed. Fix it for that device's network with a routing-table line
  ([relay-routing.md](relay-routing.md)), or send everyone elsewhere with `use-relay`.
- **The main server is a single point of failure.** If it is down, new sessions cannot
  start. Sessions already running continue, because `hbbs` is not on their data path.
- **Near on the map is not always fast.** Different carriers connect to each other in
  different places.
