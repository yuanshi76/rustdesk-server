#!/usr/bin/env python3
"""Check that STUN servers answer, and show the public address each one sees.

    python3 stun_check.py                                   # the RustDesk client's defaults
    python3 stun_check.py stun:stun.example.com:3478 stun:198.51.100.7:3478
    python3 stun_check.py --from-option "stun:a.example.com:3478,turn://u:p@b.example.com:3478"

Run it on the devices that will use the servers, with any VPN or proxy off, and
compare the addresses: every working server should report the same public address
for one machine. Only STUN over UDP is tested (the `stun:` entries); `turn:` and
`turns:` entries are listed and skipped. Python 3.8+, standard library only.
"""
import argparse
import os
import socket
import struct
import sys
import time

# The servers the RustDesk client uses when its ice-servers option names no STUN
# server (hbb_common src/webrtc.rs, DEFAULT_ICE_SERVERS).
CLIENT_DEFAULTS = [
    "stun:stun.cloudflare.com:3478",
    "stun:stun.l.google.com:19302",
    "stun:stun.antisip.com:3478",
    "stun:stun.nextcloud.com:443",
]

MAGIC = 0x2112A442
BINDING_REQUEST = 0x0001
BINDING_SUCCESS = 0x0101
MAPPED_ADDRESS = 0x0001
XOR_MAPPED_ADDRESS = 0x0020


def parse_entry(entry):
    """`stun:host[:port]`, `stun://host[:port]`, or a bare `host[:port]`, as the client
    accepts them. Returns (scheme, host, port)."""
    entry = entry.strip()
    scheme = "stun"
    if ":" in entry and entry.split(":", 1)[0] in ("stun", "stuns", "turn", "turns"):
        scheme, entry = entry.split(":", 1)
        entry = entry[2:] if entry.startswith("//") else entry
        entry = entry.rsplit("@", 1)[-1]  # drop turn credentials
    if entry.startswith("["):
        host, _, rest = entry[1:].partition("]")
        port = rest[1:] if rest.startswith(":") else ""
    elif entry.count(":") == 1:
        host, port = entry.split(":")
    else:
        host, port = entry, ""
    return scheme, host, int(port) if port else 3478


def decode_address(attr_type, value, txid):
    family = value[1]
    port = struct.unpack("!H", value[2:4])[0]
    raw = value[4:]
    if attr_type == XOR_MAPPED_ADDRESS:
        port ^= MAGIC >> 16
        key = struct.pack("!I", MAGIC) + (txid if family == 2 else b"")
        raw = bytes(b ^ key[i] for i, b in enumerate(raw))
    if family == 1:
        return f"{socket.inet_ntop(socket.AF_INET, raw[:4])}:{port}"
    return f"[{socket.inet_ntop(socket.AF_INET6, raw[:16])}]:{port}"


def query(host, port, timeout=3.0, tries=3):
    """One STUN Binding Request (RFC 5389). Returns (address, milliseconds)."""
    infos = socket.getaddrinfo(host, port, type=socket.SOCK_DGRAM)
    family, _, _, _, addr = infos[0]
    txid = os.urandom(12)
    request = struct.pack("!HHI", BINDING_REQUEST, 0, MAGIC) + txid
    with socket.socket(family, socket.SOCK_DGRAM) as s:
        s.settimeout(timeout / tries)
        for _ in range(tries):
            start = time.monotonic()
            s.sendto(request, addr)
            try:
                data, _ = s.recvfrom(2048)
            except socket.timeout:
                continue
            ms = (time.monotonic() - start) * 1000
            msg_type, length, magic = struct.unpack("!HHI", data[:8])
            if msg_type != BINDING_SUCCESS or magic != MAGIC or data[8:20] != txid:
                raise ValueError("not a STUN success response")
            pos, found = 20, {}
            while pos + 4 <= 20 + length:
                attr_type, attr_len = struct.unpack("!HH", data[pos:pos + 4])
                found[attr_type] = data[pos + 4:pos + 4 + attr_len]
                pos += 4 + attr_len + (-attr_len % 4)
            for t in (XOR_MAPPED_ADDRESS, MAPPED_ADDRESS):
                if t in found:
                    return decode_address(t, found[t], txid), ms
            raise ValueError("response carries no mapped address")
    raise TimeoutError(f"no answer within {timeout:.0f} s")


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("servers", nargs="*", help="entries as in the client's ice-servers option")
    p.add_argument("--from-option", metavar="VALUE", help="a whole ice-servers value, comma-separated")
    p.add_argument("--timeout", type=float, default=3.0, help="seconds per server (default 3)")
    a = p.parse_args(argv)
    entries = list(a.servers)
    if a.from_option:
        entries += [e for e in a.from_option.split(",") if e.strip()]
    if not entries:
        entries = CLIENT_DEFAULTS
        print("No servers given: checking the RustDesk client's defaults.\n")
    failures = 0
    seen = set()
    for entry in entries:
        scheme, host, port = parse_entry(entry)
        label = f"{scheme}:{host}:{port}"
        if scheme != "stun":
            print(f"  skip  {label:40}  ({scheme} is not tested here)")
            continue
        try:
            mapped, ms = query(host, port, a.timeout)
            seen.add(mapped.rsplit(":", 1)[0])
            print(f"  ok    {label:40}  {ms:6.0f} ms   sees you as {mapped}")
        except Exception as e:  # noqa: BLE001 - every failure is reported, not raised
            failures += 1
            print(f"  FAIL  {label:40}  {e}")
    if len(seen) > 1:
        print("\nServers report different public addresses: a VPN, proxy or several"
              " network paths may be involved.")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
