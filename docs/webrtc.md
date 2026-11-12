# WebRTC connections (optional)

RustDesk clients from **1.5.0** can make a direct connection with **WebRTC**, the
technology browsers use for video calls, alongside their other ways of reaching a device.
Whichever connects first is used. WebRTC uses STUN servers to find each device's public
address and ICE to try many address pairs, so it often gets through networks where
RustDesk's own hole punching does not, and the session then goes device to device instead
of through one of your relays.

The server only passes the setup messages between the two devices: an *offer*, an
*answer*, and *ICE candidates*. RustDesk's own server supports this in its paid Pro
version; the open-source server does not. This fork adds it as an **optional module, off
by default**.

## What happens when it is off, or does not work

Nothing changes. With `WEBRTC` unset the server behaves exactly as before. With it on,
a connection where WebRTC cannot be used still works the usual way:

- a device that does not offer WebRTC (setting off, or a client older than 1.5.0) is
  handled exactly as before;
- if WebRTC fails, or is slower, the client uses its other methods: direct connection, or
  a relay chosen by this server (geo routing, the routing table and `use-relay` are
  untouched).

## Turn it on

**1. On the main server**, add to `hbbs`'s environment and recreate the containers:

```yaml
environment:
  - WEBRTC=Y
```

The start-up log says `WEBRTC=Y`.

**2. On the devices you connect *from***: WebRTC is **off by default** in the client when
it uses your own server (as UDP and IPv6 hole punching are). Turn it on in the client's
settings; the option is stored as `enable-webrtc`. The exact label in the settings screen
has not been checked here.

Both devices need client **1.5.0 or later**. The device you connect *to* needs no setting.

## Check that it works

On the main server:

```bash
docker run --rm --network container:rustdesk_hbbs busybox:stable sh -c "printf 'webrtc' | nc -w 2 127.0.0.1 21115"
```

```
WEBRTC: Y
attempts in progress: 0
offers forwarded: 3
answers forwarded: 3
candidates to peer: 21
candidates to controller: 18
candidates dropped: 0
```

After one connection attempt with WebRTC on, `offers forwarded` and `answers forwarded`
go up. If only `offers` go up, the device you connect to is older than 1.5.0. If nothing
goes up, the device you connect from has WebRTC off. Whether WebRTC actually *won* the race
is in the connecting client's log: its "Hole Punched" line names the method.

## STUN and TURN servers

The clients use public STUN servers by default (Cloudflare, Google, and two others).
Some of them may be unreachable from mainland China. If WebRTC seldom succeeds there, run a
STUN server of your own (for example `coturn`) on one of your machines and set it in the
clients' `ice-servers` option. A TURN server (`turn://user:password@host:port`) also lets
WebRTC work when nothing direct does, but then it is a relay too, and your own `hbbr` relays
already do that job. Neither is part of this project.

## Limits

- **Not tested with real clients here.** It follows the protocol in the clients' source and
  is tested with simulated clients, including a desktop-style encrypted TCP connection. Try
  it before relying on it.
- **Two devices on the same LAN** are introduced through a different message that has no
  room for an offer, so WebRTC is not used between them; they connect directly anyway.
- **Candidates for the connecting device are accepted only from the other device's IP
  address** as the server knows it. If that device reaches the server over IPv4 for one
  connection and IPv6 for another, its candidates are dropped and WebRTC falls back.
- **The protocol is new and still changing in the clients.** The fields are read as raw
  protobuf fields, because this server is built against the protocol version upstream's
  server uses, which predates WebRTC. When upstream adopts a newer one, this module
  switches to it.

## Privacy and abuse limits

ICE candidates list each device's local network addresses. The clients only send them over
an encrypted connection to the server. The server forwards candidates only between the two
devices of an attempt it set up itself in the last minute, toward the connecting device
only from the other device's address, at most 128 each way.
