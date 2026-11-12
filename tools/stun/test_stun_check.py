"""Tests for stun_check.py against a minimal STUN responder on loopback.

    python3 -m unittest discover -s tools/stun -v
"""
import io
import os
import socket
import struct
import sys
import threading
import unittest
from contextlib import redirect_stdout

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import stun_check  # noqa: E402


class FakeStun:
    """Answers each Binding Request with an XOR-MAPPED-ADDRESS (or a plain
    MAPPED-ADDRESS) naming the requester, as a real server does."""

    def __init__(self, xor=True, ipv6=False):
        family = socket.AF_INET6 if ipv6 else socket.AF_INET
        self.sock = socket.socket(family, socket.SOCK_DGRAM)
        self.sock.bind(("::1" if ipv6 else "127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.xor = xor
        self.last_from = None
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                data, addr = self.sock.recvfrom(2048)
            except OSError:
                return
            txid = data[8:20]
            self.last_from = addr
            ip, port = addr[0], addr[1]
            if ":" in ip:
                fam, raw = 2, socket.inet_pton(socket.AF_INET6, ip)
            else:
                fam, raw = 1, socket.inet_pton(socket.AF_INET, ip)
            if self.xor:
                key = struct.pack("!I", stun_check.MAGIC) + (txid if fam == 2 else b"")
                raw = bytes(b ^ key[i] for i, b in enumerate(raw))
                port ^= stun_check.MAGIC >> 16
                attr_type = stun_check.XOR_MAPPED_ADDRESS
            else:
                attr_type = stun_check.MAPPED_ADDRESS
            value = struct.pack("!BBH", 0, fam, port) + raw
            attr = struct.pack("!HH", attr_type, len(value)) + value
            head = struct.pack("!HHI", stun_check.BINDING_SUCCESS, len(attr), stun_check.MAGIC)
            self.sock.sendto(head + txid + attr, addr)

    def close(self):
        self.sock.close()


class Parse(unittest.TestCase):
    def test_entries_as_the_client_accepts_them(self):
        p = stun_check.parse_entry
        self.assertEqual(p("stun:stun.example.com:19302"), ("stun", "stun.example.com", 19302))
        self.assertEqual(p("stun:stun.example.com"), ("stun", "stun.example.com", 3478))
        self.assertEqual(p("stun://stun.example.com:3478"), ("stun", "stun.example.com", 3478))
        self.assertEqual(p("turn://user:p@ss@turn.example.com:443"), ("turn", "turn.example.com", 443))
        self.assertEqual(p("stun:[2001:db8::1]:3478"), ("stun", "2001:db8::1", 3478))
        self.assertEqual(p("198.51.100.7"), ("stun", "198.51.100.7", 3478))


class Query(unittest.TestCase):
    def test_xor_mapped_address_ipv4(self):
        s = FakeStun()
        try:
            addr, ms = stun_check.query("127.0.0.1", s.port)
            self.assertEqual(addr, f"127.0.0.1:{s.last_from[1]}")
            self.assertGreaterEqual(ms, 0)
        finally:
            s.close()

    def test_plain_mapped_address(self):
        s = FakeStun(xor=False)
        try:
            addr, _ = stun_check.query("127.0.0.1", s.port)
            self.assertEqual(addr, f"127.0.0.1:{s.last_from[1]}")
        finally:
            s.close()

    def test_xor_mapped_address_ipv6(self):
        try:
            s = FakeStun(ipv6=True)
        except OSError:
            self.skipTest("no IPv6 loopback")
        try:
            addr, _ = stun_check.query("::1", s.port)
            self.assertEqual(addr, f"[::1]:{s.last_from[1]}")
        finally:
            s.close()

    def test_no_answer_is_a_timeout(self):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind(("127.0.0.1", 0))  # bound, never answers
        try:
            with self.assertRaises(TimeoutError):
                stun_check.query("127.0.0.1", sock.getsockname()[1], timeout=0.6)
        finally:
            sock.close()


class Main(unittest.TestCase):
    def run_main(self, args):
        out = io.StringIO()
        with redirect_stdout(out):
            code = stun_check.main(args)
        return code, out.getvalue()

    def test_exit_status_and_turn_entries_skipped(self):
        s = FakeStun()
        try:
            code, out = self.run_main(["--from-option", f"stun:127.0.0.1:{s.port}, turn://u:p@127.0.0.1:3478"])
            self.assertEqual(code, 0, out)
            self.assertIn("ok", out)
            self.assertIn("skip", out)
        finally:
            s.close()
        dead = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        dead.bind(("127.0.0.1", 0))
        try:
            code, out = self.run_main(["--timeout", "0.6", f"stun:127.0.0.1:{dead.getsockname()[1]}"])
            self.assertEqual(code, 1)
            self.assertIn("FAIL", out)
        finally:
            dead.close()


if __name__ == "__main__":
    unittest.main()
