# Choosing the nearest relay by where a device is

**The goal:** every device uses the same settings, and the main server (`hbbs`) sends
each connection to the relay nearest the devices, wherever they are, including a laptop
on a business trip or a phone on mobile data. You write down where each relay is, once.
No table of addresses to keep up to date, and nothing to measure.

This needs fork version **v0.4.0** or later of the `hbbs` image.

## How it works

`hbbs` sees the public address of each device. A free location database turns that
address into an approximate place (a latitude and longitude). `hbbs` measures the
distance from that place to each relay and picks the one with the smallest total for
the two ends of the session. Distance is turned into a rough number of milliseconds
(about 1 ms per 50 km); it only has to rank the relays, not predict a ping.

It is a fallback behind your routing table. For each end of a session:

1. If your [routing table](relay-routing.md) has a line for its address, that line is used.
2. Otherwise its place from the database is used.
3. If neither knows it (a private address, or one missing from the database), that end
   is ignored. If neither end is known, relays are taken in turn.

## What you need

### 1. The location database

DB-IP publishes a free one, "IP to City Lite", with no account needed. It is updated
monthly. Use the **MMDB** format and the **city** version; the country-only version
cannot tell Hong Kong from Shanghai.

The download link is built from the year and month:

```
https://download.db-ip.com/free/dbip-city-lite-YYYY-MM.mmdb.gz
```

For October 2026, `dbip-city-lite-2026-10.mmdb.gz`; it is about 60 MB. At the start of a
month the new file may not be up yet; use last month's. The page is
<https://db-ip.com/db/download/ip-to-city-lite>.

**Licence:** Creative Commons Attribution 4.0. You may use it, and you must credit
DB-IP.com for the data. This project's README carries that credit; keep it if you
redistribute anything.

On the machine that runs `hbbs`, in the folder you already mount as its data folder
(the one that holds `db_v2.sqlite3`):

```bash
cd ./data
curl -fLO https://download.db-ip.com/free/dbip-city-lite-2026-10.mmdb.gz
gunzip dbip-city-lite-2026-10.mmdb.gz
mv dbip-city-lite-2026-10.mmdb geo.mmdb
```

**Updating it later:** DB-IP publishes a new file every month, and you can have that
done for you, see [Keeping it up to date](#keeping-it-up-to-date) below. By hand:
download and unpack under a different name, then `mv` it over `geo.mmdb`. Rename,
never copy or unpack straight over the old file, because `hbbs` has the file open and
the replacement must not change it underneath. `hbbs` notices the new file within a few
seconds and needs no restart.

### 2. Where your relays are

A small text file, `relay_locations.txt`, in the same folder. One line per relay: the
address exactly as in `hbbs`'s relay list, then latitude and longitude:

```
# relay                      latitude, longitude
hk.example.com:21117         22.32, 114.17     # Hong Kong
sh.example.com:21117         31.23, 121.47     # Shanghai
us.example.com:21117         37.77, -122.42    # San Francisco
```

To find a place's coordinates, search "<city> latitude longitude", or right-click the
spot on a map. The nearest city is accurate enough. Latitude is first; north and east
are positive, south and west negative. A relay missing from this file is treated as
very far away and is chosen only if nothing else can be.

### 3. Tell `hbbs` where they are

```yaml
environment:
  - GEO_DB=/data/geo.mmdb
  - RELAY_LOCATIONS=/data/relay_locations.txt
```

(`/data` is the data folder in the s6 images; in the classic image it is `/root`. Write
the path as the container sees it.) Both are needed. Restart `hbbs` once; after that,
edits to either file are picked up on their own.

On the clients, as before, fill in only the ID server and the Key, and leave **Relay
server** empty. A machine whose Relay server field is filled in ignores all of this.

## Keeping it up to date

The images carry a small updater, `rustdesk-utils geo-update`. It checks once a day
and downloads only when DB-IP has published a new month's file (about once a month,
so a few hundred MB a year at most). It writes the file under a temporary name, checks
that it is a real City database, and only then renames it into place; a failed or
damaged download is discarded and the old file keeps being used. It remembers which
month it has in `geo.mmdb.version` beside the database. `hbbs` picks up the new file
within a few seconds.

It checks the certificate of `download.db-ip.com` (the images include root
certificates for this).

**s6 images** (`rustdesk-server-s6`, `docker-compose-s6.yml`): just turn it on. This
also saves you from setting `GEO_DB`, which defaults to `/data/geo.mmdb`:

```yaml
environment:
  - GEO_AUTO_UPDATE=Y
  - RELAY_LOCATIONS=/data/relay_locations.txt
```

The first download happens when the container starts, so for the first minute `hbbs`
logs that it has no database and takes relays in turn; then it switches over.

**Classic image** (no shell, one process per container): run the updater as a third
container from the same image, sharing the data folder:

```yaml
  rustdesk_geo_update:
    image: ghcr.io/yuanshi76/rustdesk-server:v0.4.0   # the same tag as hbbs
    command: rustdesk-utils geo-update --loop --out /root/geo.mmdb
    volumes:
      - ./data:/root
    restart: unless-stopped
```

and give `hbbs` `GEO_DB=/root/geo.mmdb` and `RELAY_LOCATIONS=/root/relay_locations.txt`
as before.

**One-off or from cron:** `docker exec rustdesk_server rustdesk-utils geo-update`
(s6 image) updates now if a new month is out; add `--force` to download again even if the
month is already installed.

If a download fails, the log says why (for example no outside access, or DB-IP's
site being down), the current file stays, and it tries again six hours later.

## Which machine runs what

Only the main server runs `hbbs`, so only the main server needs the database, the
locations file, the updater and these settings. The other relay servers run **only
`hbbr`**, from the same image (see `docker-compose-relay-node.yml`); they know nothing
about locations, the main server, or each other. The relay code (`hbbr`) has not
changed in this fork's routing work, so relay machines on `v0.2.0` or later keep
working, but running the same version everywhere is simpler. The main server can
also run an `hbbr` of its own and list it among the relays.

## Check that it works

Run these on the main server (replace `rustdesk-server` with your container name). The
first shows what was loaded; the second asks where `hbbs` would send a session between two
addresses, using your own public addresses (`curl ifconfig.me` on a device shows it):

```bash
docker exec rustdesk-server sh -c "printf 'relay-routes' | nc -w 2 127.0.0.1 21115"
docker exec rustdesk-server sh -c "printf 'test-relay 203.0.113.5 198.51.100.7' | nc -w 2 127.0.0.1 21115"
```

The second prints the chosen relay, the reason (`geo`), the estimated milliseconds to
each relay, and where each address was placed (`located 203.0.113.5: 22.28,114.15`).
**Look at that place.** If it is a different city from where the device really is, the
database has the wrong idea about that address, and nothing here can fix it.

## What to expect

- **Accuracy is about city or country level.** Fine for "Asia, Europe or America", and
  usually for "Hong Kong or Shanghai". Mobile networks are the weakest: an address can
  be placed where the carrier's equipment is, not where the phone is.
- **A phone roaming abroad** often gets its home carrier's address, so it is placed at
  home. Likewise a device on a VPN is placed where the VPN exits, which is also where
  its traffic enters the internet.
- **A catch-all routing line** (`0.0.0.0/0`, see [relay-routing.md](relay-routing.md))
  claims every address, so the location is never consulted. Use one or the other.
- **If the database or the locations file is missing or broken,** `hbbs` says so in its
  log (and in `relay-routes`), keeps the previous one if it had one, and otherwise falls
  back to taking relays in turn. A connection is never refused for it.
- **This is not load balancing.** Two devices in the same city always get the same
  relay. For "send everyone to relay X today", use `use-relay` ([switching-relays.md](switching-relays.md));
  it overrides all of this.

## Status

Tested with a small test database, and also by hand with the real DB-IP file
(`dbip-city-lite-2026-10`, 127 MB unpacked) and a real `hbbs` with three stand-in relays
on one machine. Well-known addresses were placed in the right city (Google DNS in
California, Baidu DNS in Beijing, Alibaba DNS in Hangzhou), a Beijing and Hangzhou pair
went to the Hong Kong relay and a California pair to the San Jose one, private
addresses were not placed, and the whole `hbbs` process used about 24 MB with the
database loaded. Anycast addresses (the same address served from many cities, such as
Google's IPv6 DNS) are placed arbitrarily, as for any database. Not yet run with a real
client or separate relay servers. If the first `test-relay` above shows a place that
makes no sense for one of your own addresses, say so.
