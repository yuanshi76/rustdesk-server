# Using Tailscale alongside the servers

If your devices are on a [Tailscale](https://tailscale.com) network (a tailnet), you can
connect to them **directly over Tailscale** and keep connecting **by ID through your servers**
when Tailscale is not available. Nothing changes on the servers: they do not need to be on
the tailnet, and no setting here touches them.

You choose per connection. An entry with the device's Tailscale address goes over Tailscale;
the usual ID goes through `hbbs` and the relays as before.

## How it works

The RustDesk client treats what you type in the connect box in one of three ways (read in the
client's source, `client.rs`):

| You type | The client |
|---|---|
| an IP address, such as `100.89.203.53` or `100.89.203.53:21118` | connects **straight to that address**, port 21118 by default, without any server |
| `host.domain:port`, such as `office-pc.your-tailnet.ts.net:21118` | the same, by name (a full name **and** a port; a bare `office-pc` is treated as an ID) |
| anything else | treats it as an **ID** and goes through your servers |

**Do not write `ID@address`.** That means "find this ID using the ID server at this address",
so `528956768@100.89.203.53:21118` asks the device's direct port to act as an ID server, and
fails.

## Setup

**On each device you want to reach over Tailscale:**

1. Install Tailscale and join your tailnet. Note its address with `tailscale ip -4`, or its
   full MagicDNS name (`name.your-tailnet.ts.net`) from the Tailscale admin page.
2. In RustDesk: **Settings → Security → Enable direct IP access** (port 21118 by default). Set a
   permanent password.
3. Let port 21118 in **only from the tailnet**. Direct IP access listens on every network the
   device is on.
   - Linux with ufw: `sudo ufw allow in on tailscale0 to any port 21118 proto tcp`
   - Windows (PowerShell as administrator):
     `New-NetFirewallRule -DisplayName "RustDesk direct via Tailscale" -Direction Inbound -Protocol TCP -LocalPort 21118 -RemoteAddress 100.64.0.0/10 -Action Allow`.
     Check that no other inbound rule (for example one the RustDesk installer added for the
     program) already allows it from everywhere.
   - If your Tailscale access rules restrict traffic between devices, allow TCP 21118 there.
4. Optional: RustDesk's **IP whitelist** (Settings → Security). A connection over Tailscale
   comes from the **Tailscale address** of the machine you connect from (`100.x.y.z`), not its
   LAN or public address, so that is the address to add. The whitelist applies to every
   incoming connection, so after setting it, **also test a normal connection by ID**, or you
   may have blocked your own fallback.

**On the device you connect from:**

5. Turn Tailscale on and type `100.x.y.z` (or `name.your-tailnet.ts.net:21118`) in the connect
   box. Save it in the address book next to the device's ID entry, for example
   "Office PC (Tailscale)" and "Office PC".

**When Tailscale is not available**, use the ID entry. That goes through your servers and the
automatic relay choice exactly as before.

## When it does not connect

On the device you connect from:

```bash
# macOS / Linux
nc -vz 100.89.203.53 21118
```

```powershell
# Windows
Test-NetConnection 100.89.203.53 -Port 21118
```

- **It connects**: the network is fine; check direct IP access, the whitelist and the password
  on the other device.
- **Refused or timed out**: check that device's firewall, that RustDesk is installed and
  running as a service, and your Tailscale access rules.
- `tailscale ping 100.89.203.53` shows whether Tailscale reaches the device, and whether it is
  direct or going through Tailscale's own relays (DERP), which can be slower than your own relay.

## Limits

- Choosing Tailscale is manual: pick the entry. There is no automatic switch, because the client
  dials exactly what you typed.
- An address-book entry by address has no ID behind it, so things tied to the ID (online status,
  for example) may not show for it.
- Phones: whether a phone can be *controlled* this way depends on its RustDesk app offering
  direct IP access; not checked here.

A design that switched automatically, by making the server's name resolve to its Tailscale
address for devices on the tailnet (split DNS), was considered and dropped as too many moving
parts. It is recorded in `NOTES.md`.
