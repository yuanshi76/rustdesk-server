# The API server, and how it relates to `hbbs` and `hbbr`

Short answer: **user management lives entirely in the API server, and it is
separate from the relay and rendezvous side.** `hbbs` and `hbbr` do not know who
your users are. They are tied together by one shared secret and one loopback
console, both described below.

This was checked in the code of `lejianwen/rustdesk-api` and by running it against
this server, not taken from its README.

## What it is

A separate Go program (MIT licence) with its own database (SQLite by default,
MySQL or PostgreSQL optionally), serving HTTP on port 21114. Its source has
controllers for: password login, OAuth/OIDC and LDAP, users and groups, per-user
address books with shared collections and rules, a device list, login and
connection/file-transfer audit logs, and an admin web UI.

Only two kinds of caller talk to it: **clients** (login, address book, device
info, heartbeat, audit reports) and **admins** (the web UI). `hbbs` and `hbbr`
never call it.

## What connects it to `hbbs`

### 1. A login token, checked by `hbbs` on its own

When a client logs in, the API returns an `access_token`. A client that is logged
in sends it with every connection request, and with `MUST_LOGIN=Y` `hbbs`
refuses a request that carries none. `hbbs` never asks the API whether the token
is good; it checks it itself, using a secret both sides share. Whether that check
can work depends on the API's own setting:

| API has `RUSTDESK_API_JWT_KEY` | `hbbs` has `RUSTDESK_API_JWT_KEY` | What happens |
|---|---|---|
| yes | yes, **the same** | Works. The API issues a signed token (HS256, claims `user_id` and `exp`, valid 7 days by default) and `hbbs` verifies the signature and expiry. |
| **no** | yes | **Every logged-in client is refused** with "Token error, please log out and log back in!". Without a key the API issues a 32-character hex string, which is not a token `hbbs` can verify. |
| yes | no | `hbbs` accepts any non-empty string, so the login is only checked for being present. |
| no | no | The same: any non-empty string gets through. |

Rows 1 and 2, a missing token and a tampered token, were run for real against the
API image and a real `hbbs`. Rows 3 and 4 follow from the code. Both sides read the
same environment variable name, so in the `s6-api` image, which runs both in one
container, setting it once covers both. In separate containers, set the same value
on each.

**A caveat, inferred rather than run:** `hbbs` checks a token's signature and
expiry and nothing else, and it never calls the API. Logging a user out or disabling
them in the API deletes the API's own record of the token, but a copy of the signed
token will still pass `hbbs` until it expires. To lock someone out at once, change
the shared key on both sides, which logs everybody out.

### 2. The "server control" page, over loopback

The admin UI can send console commands to `hbbs` and `hbbr`. It does this by
connecting to **`[::1]` and then `127.0.0.1`** on `hbbs`'s port minus one (21115)
and on `hbbr`'s relay port (21117). Loopback only, so it works only when the API
shares a network namespace with them: the `network_mode: "service:hbbs"` in the
three-container compose, or the `s6-api` image. It cannot reach a relay on another
machine.

The commands it offers are its own preset list; anything else, such as
`test-relay`, `relay-routes` or `ws-peers`, can be added as a custom command in
its UI. It reads one reply of up to 1 KB, which fits all of them.

### 3. The server settings it displays

The admin "server config" page shows `id-server`, `relay-server`, `api-server` and
the public key, for an admin to copy into clients. The key can be read straight from
`hbbs`'s `id_ed25519.pub` (`key-file`). **`relay-server` is a single value.** With
more than one relay, leave it empty (`RUSTDESK_API_RUSTDESK_RELAY_SERVER` unset):
in the client's source a controlled machine that has its own relay setting uses it
instead of the server's choice, so a config string with one relay filled in, copied
onto the machines you connect to, defeats the routing table.

## What it does not share with `hbbs`

- **No database.** `hbbs` keeps its peer table in `db_v2.sqlite3`; the API keeps its
  own. They are not synchronised.
- **Two separate device lists.** The API's list is what clients reported to it; the
  server's list is who registered with `hbbs`. The same machine appears in both
  only because the client told both.
- **Audit logs come from the clients.** The API's connection log is posted by the
  client (`/api/audit/conn`), not recorded by `hbbs` or `hbbr`.
- **Nothing about relays.** `hbbr` and the API do not know each other exist, and
  adding relays changes nothing on the API side beyond the setting above.

## With several relays

Nothing about users or login changes. There is still one API server and one `hbbs`,
and relays are interchangeable plumbing. The two things to get right are the empty
`relay-server` setting above and the shared JWT key if you use `MUST_LOGIN`.

## What to back up

The API's data directory (`/app/data` in the images): users, address books, logs.
The server's identity (`id_ed25519`, `id_ed25519.pub`) and `db_v2.sqlite3` are
`hbbs`'s and are a separate backup.
