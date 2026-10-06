# Switching between several relay servers with one client configuration

**The goal:** every client is set up once, and you choose which relay server
everyone uses by running one command on the main server. No client is edited again.

This needs fork version **v0.3.1** or later of the `hbbs` image.

## How it fits together

- The **main server** is the one running `hbbs` (the ID server). Clients already point
  at it. It also decides which relay each connection uses.
- Each **relay server** runs `hbbr`. They can be on other machines. See
  `docker-compose-relay-node.yml`.

## One-time setup

**1. On the main server**, list every relay in `hbbs`'s relay list, with the address
clients reach it on (a public IP or domain, and the port):

```yaml
# s6 images (docker-compose-s6.yml): the RELAY variable
environment:
  - RELAY=hk.example.com:21117,sh.example.com:21117

# classic image (docker-compose.yml): the -r option on hbbs
command: hbbs -r hk.example.com:21117,sh.example.com:21117
```

**2. On every client**, in the network settings, fill in only the **ID server** and
the **Key**. Leave **Relay server** *empty*. This matters most on the machines you
connect **to**: if their Relay server field is filled in, it overrides the main
server's choice. You connect with the numeric ID exactly as before.

**3. On the relay machines**, open port 21117 (and 21119) in the firewall. If you
want the relays to check the key, start `hbbr` with the same key as `hbbs`.

## Choosing the relay

Run this on the main server (replace `rustdesk-server` with your `hbbs` container name):

```bash
docker exec rustdesk-server sh -c "printf 'use-relay hk.example.com:21117' | nc -w 2 127.0.0.1 21115"
```

From then on **every new connection** uses that relay. Sessions already running are
not interrupted. The choice is saved in a file called `relay_preferred`, in the same
folder as `db_v2.sqlite3`, so it survives a restart.

| Command (after `printf '...' \| nc ...`) | What it does |
|---|---|
| `use-relay` | Shows the current choice, and whether that relay is answering |
| `use-relay HOST:PORT` | Everyone uses this relay. It must be one from the relay list that is answering; anything else is refused and nothing changes |
| `use-relay auto` | Back to automatic: relays are taken in turn (or by the routing table, if you set one up) |
| `test-relay 1.2.3.4` | Shows which relay a connection would get right now, and why |

If the `classic` image is used (no shell inside), reach the console the way
[environment-variables.md](environment-variables.md#reaching-the-console-from-a-container)
describes.

If the chosen relay stops answering, new connections go to the other relays instead of
failing, and `use-relay` says "NOT answering". When it comes back, it is used again.

## Check that it worked

After the first command, make one connection and look at the `hbbs` log:

```bash
docker logs rustdesk-server 2>&1 | grep "relay for" | tail -3
```

It should say `... (preferred)` and name the relay you chose. If it names a different
relay, a client has its own Relay server filled in.

## What this does not do

- It is one choice for everybody. It does not give different users different relays.
  For "the nearer relay for each place", see [relay-routing.md](relay-routing.md).
- It does not measure anything. You decide which relay is closer.
- Tested here with stand-in relays on one machine, not on a real set of servers.
