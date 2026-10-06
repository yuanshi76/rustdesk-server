# Using several relay servers

Skip this page while you run **one** relay: with one there is nothing to choose.
It starts to matter with two or more `hbbr` servers in different places.

## What goes wrong without it

`hbbs` hands out relays round-robin and ignores where the clients are. With
several relays that means:

- a client can be given a relay on the other side of the world, and
- two attempts a moment apart by the same pair of clients can be handed
  **different** relays, so the two ends never meet.

This build fixes both:

1. **One relay per connection attempt.** The choice for a pair of clients is held
   for 30 seconds, so a retry, or the extra code path that exists only with
   `--mask`, gets the same answer. A held choice is dropped as soon as that relay
   stops answering, and when the routing table changes.
2. **A routing table you write**, saying which relay is nearest to which clients.
   It is a plain file; there is no geographic database and no live measurement.

What it cannot do: move a session that is already running. Once a session is up
its data flows between the two clients and the relay, and `hbbs` is no longer
involved, so nothing here can change it, and nothing needs to.

## What you have to fill in

Three small files. Everything else is generated.

| File | What goes in it | Where it comes from |
|---|---|---|
| `nodes.txt` | your relays: `id  host  port  [region]` | you. The port is the one **clients** reach, e.g. the host-side mapping of hbbr's 21117 |
| `sites.txt` | the places your clients are, and the public network each uses | you, with one `curl` per place (see [measuring-latency.md](measuring-latency.md)) |
| `relay_routes.txt` | the routing table | **generated** from your measurements |

Examples of the first two are in `tools/relay-rtt/` (`nodes.example.txt`,
`sites.example.txt`).

## Step by step

**1. Run `hbbr` on each relay server**, and start `hbbs` with all of them:

```
hbbs -r hk.example.com:21117,sh.example.com:21117
```

Write each relay **exactly the same way** in `nodes.txt` and in `-r`: lower case
doesn't matter, but `host:port` has to match, and `host` alone means
`host:21117`.

Leave the **Relay Server field empty on every machine that is connected to**
(the controlled side). In the client's source (`rendezvous_mediator.rs`,
`get_relay_server`) a machine with that setting uses it instead of the server's
choice, and that is the relay the other side is then told to use. The controlling
side's own setting did not appear in the connection code I read, but there is no
reason to set it either.

**2 to 5. Measure, then generate the table.** This is the part that takes three
days of waiting, and it has its own guide: **[measuring-latency.md](measuring-latency.md)**.
In short: you run one script from each place your clients are (with any VPN or
proxy off), write down each place's public address, bring the results together,
check whether a second relay is worth it, and generate `relay_routes.txt`. Come
back here for steps 6 to 8.

**6. Give the file to `hbbs`.** Put it where `hbbs` can read it and set
`RELAY_ROUTES`:

```yaml
    environment:
      - RELAY_ROUTES=/root/relay_routes.txt
    volumes:
      - ./data:/root          # relay_routes.txt lives in ./data
```

(On the s6 images the data directory is `/data`, so the path is
`/data/relay_routes.txt`.)

**7. Check it.** Ask the server what it would do for two addresses, without doing
it (how to reach the console is in `docs/environment-variables.md`):

```
printf 'test-relay 203.0.113.5 198.51.100.7' | nc -w 2 127.0.0.1 21115
```

```
relay: sh.example.com:21117 (routes)
hk.example.com:21117=122
sh.example.com:21117=110
```

Each line is a relay's **total** for that session: the sum of what it costs from
each end. `relay-routes` shows whether the file loaded and the last error.

**8. Keep it.** Edit the file and save: `hbbs` re-reads it within a few seconds,
no restart. A file with a mistake is reported once, with its line number, and the
previous table stays in force. A relay that stops answering is dropped from the
choice within a few seconds, whatever the table says, and is considered again once
it answers. (The server probes every relay once every three seconds.)

## What the table file means

```
# network            relay=milliseconds[,relay=milliseconds ...]
203.0.113.0/24       hk.example.com:21117=38,sh.example.com:21117=95
198.51.100.0/22      sh.example.com:21117=12,hk.example.com:21117=80
```

- A relayed session has two legs, one from each client to the relay, so a relay's
  cost for a session is the sum of the two ends' numbers. The cheapest healthy
  relay wins.
- The most specific network that contains an address is the one used.
- If only one end is in the table, that end decides. If neither is, the choice is
  round-robin, as without a table.
- A relay a network's line does not mention costs 1000 ms from there: it loses to
  any relay that is listed, but can still be chosen if nothing else is.
- When two relays cost the same, the one listed first in `-r` wins.

## Phones, and laptops that travel

**The better answer is [geo-routing.md](geo-routing.md):** `hbbs` places any address on a
map with a free location database and picks the nearest relay, so nothing needs
updating when an address changes. What follows is the manual alternative.

The table matches on the address `hbbs` sees, so a device whose address keeps
changing will not match a line you wrote for one place. What happens then:

- **One end listed, one not** (a travelling laptop connecting to your office machine):
  the unlisted end is ignored and the relay nearest the listed end is used. For many
  setups that is already the right answer.
- **Neither end listed** (a phone connecting to a travelling laptop): relays are taken
  in turn, which may be a far one.

To cover the second case, add a catch-all line for any address not listed elsewhere,
saying which relay you want by default. Put your preferred default relay first in the
`hbbs` relay list as well, because ties go to the earliest:

```
0.0.0.0/0   hk.example.com:21117=0, sh.example.com:21117=20, us.example.com:21117=60
::/0        hk.example.com:21117=0, sh.example.com:21117=20, us.example.com:21117=60
```

The first line is for IPv4 addresses and the second for IPv6 (most phone networks
use both). A specific line always beats the catch-all where it applies. If one end
is listed and the other is a roamer, the two add up as usual, so keep the catch-all's
numbers small compared with your specific lines, or it will outweigh them.

Phone networks do not give a device a fixed address, so the table cannot know which
relay is nearest a phone on the move. If you need a particular device to always use a
particular relay wherever it is, that has to key on the device's ID instead of its
address, which is not built.

## Things to know

- **`hbbs` matches on the address it sees.** Clients behind a reverse proxy are
  matched on the forwarded `X-Real-IP`; if your proxy does not send it, every
  client looks like the proxy and gets the same relay. Websocket clients should
  not reach `hbbs` except through a proxy that overwrites that header.
- **Every client behind one network address shares one line.** A school or an
  office is one entry, however many machines it has.
- **`hbbs` is still a single point of failure.** Relays can come and go; it
  cannot.
- **A relay is "healthy" if it accepts a TCP connection** from `hbbs` every three
  seconds. If every relay fails that check, `hbbs` keeps the last list rather than
  using none.
- **To retire a relay**, take it out of the `-r` list: restart `hbbs`, or change
  the list live with the `relay-servers` console command (not remembered across a
  restart). Then wait for the sessions on it to end and stop it. Removing it from
  the routing table alone does **not** retire it: a relay the table does not
  mention still costs 1000 ms and can still be chosen. Running sessions are never
  moved.
- **The settings:** `RELAY_ROUTES` (the file), `RELAY_PIN_TTL` (seconds a
  decision is held, default 30, `0` turns holding off). Without `RELAY_ROUTES` the
  only change from a stock server is the holding.
