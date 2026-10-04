"""Tests for relay_rtt.py. Standard library only.

    python3 -m unittest discover -s tools/relay-rtt -v
"""
import contextlib
import csv
import io
import os
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from types import SimpleNamespace

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import relay_rtt as rr  # noqa: E402

TOOL = os.path.join(HERE, "relay_rtt.py")


def write_nodes(d, text):
    path = os.path.join(d, "nodes.txt")
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    return path


def row(site, node, median, ts, offset=None, loss=0.0, **kw):
    r = {"ts_utc": ts, "site": site, "node": node, "host": "h", "port": 1,
         "ip": "127.0.0.1", "n": 20, "ok": 20, "loss_pct": loss, "min_ms": median,
         "median_ms": median, "p90_ms": median, "jitter_ms": 1.0, "error": "",
         "utc_offset_min": "" if offset is None else offset}
    r.update(kw)
    return r


def write_csv(d, name, rows):
    path = os.path.join(d, name)
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=rr.FIELDS)
        w.writeheader()
        w.writerows(rows)
    return path


def run_report(argv):
    """Run `report` in-process and return (stdout, exit message or None)."""
    out, exit_msg = io.StringIO(), None
    old = sys.argv
    sys.argv = ["relay_rtt.py", "report"] + argv
    try:
        with contextlib.redirect_stdout(out):
            rr.main()
    except SystemExit as e:
        exit_msg = e.code
    finally:
        sys.argv = old
    return out.getvalue(), exit_msg


class Parsing(unittest.TestCase):
    def test_nodes_file(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_nodes(d, "# comment\n\nhk  hk.example.com  31107  hk  # trailing\n"
                               "sh,sh.example.com,31107,cn-east\n")
            nodes = rr.read_nodes(p)
        self.assertEqual([n["id"] for n in nodes], ["hk", "sh"])
        self.assertEqual(nodes[0]["port"], 31107)
        self.assertEqual(nodes[1]["region"], "cn-east")

    def test_bad_nodes_files_exit(self):
        for text in ("only two\n", "a h notaport\n", "a h 1\na h 2\n", "# nothing\n"):
            with tempfile.TemporaryDirectory() as d:
                with self.assertRaises(SystemExit, msg=text):
                    rr.read_nodes(write_nodes(d, text))

    def test_peak_hours(self):
        self.assertEqual(rr.parse_hours("19-23"), {19, 20, 21, 22, 23})
        self.assertEqual(rr.parse_hours("22-2"), {22, 23, 0, 1, 2})
        self.assertEqual(rr.parse_hours("5-5"), {5})
        for bad in ("evening", "19", "19-25", "-1-3"):
            with self.assertRaises(SystemExit, msg=bad):
                rr.parse_hours(bad)


class Statistics(unittest.TestCase):
    def test_jitter_zero_is_not_missing(self):
        # A perfectly steady series has jitter 0.0, which is a measurement, not an
        # absence. The probe once turned it into an empty cell via `or ""`.
        self.assertEqual(rr.jitter([5.0, 5.0, 5.0]), 0.0)
        self.assertIsNone(rr.jitter([5.0]))

    def test_probe_writes_zero_jitter(self):
        with tempfile.TemporaryDirectory() as d:
            nodes = write_nodes(d, "a 127.0.0.1 9\n")
            out = os.path.join(d, "o.csv")
            real = rr.connect_ms
            rr.connect_ms = lambda *a, **k: (1.0, None)
            try:
                with contextlib.redirect_stdout(io.StringIO()):
                    rr.cmd_probe(SimpleNamespace(nodes=nodes, site="s", samples=4,
                                                 interval=0, timeout=1, out=out))
            finally:
                rr.connect_ms = real
            with open(out, newline="") as f:
                got = next(csv.DictReader(f))
        self.assertEqual(got["jitter_ms"], "0.0")
        self.assertEqual(got["loss_pct"], "0.0")

    def test_percentile_and_summary(self):
        self.assertEqual(rr.percentile([1, 2, 3, 4, 5], 0.5), 3)
        s = rr.summarise([10.0, 20.0, 30.0], 4)
        self.assertEqual((s["ok"], s["loss_pct"], s["median_ms"]), (3, 25.0, 20.0))


class FakeIp(unittest.TestCase):
    def test_range(self):
        for ip in ("198.18.0.50", "198.19.255.254"):
            self.assertTrue(rr.is_fake_ip(ip), ip)
        for ip in ("198.17.255.255", "198.20.0.0", "10.0.0.1", "1.1.1.1", "", "nonsense"):
            self.assertFalse(rr.is_fake_ip(ip), ip)

    def test_probe_warns_while_the_user_can_still_act(self):
        with tempfile.TemporaryDirectory() as d:
            nodes = write_nodes(d, "hk hk.example.com 443\n")
            real_resolve, real_connect = rr.resolve, rr.connect_ms
            rr.resolve = lambda host, port: (socket.AF_INET, "198.18.0.50")
            rr.connect_ms = lambda *a, **k: (0.4, None)
            err = io.StringIO()
            try:
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
                    rr.cmd_probe(SimpleNamespace(nodes=nodes, site="s", samples=2,
                                                 interval=0, timeout=1,
                                                 out=os.path.join(d, "o.csv")))
            finally:
                rr.resolve, rr.connect_ms = real_resolve, real_connect
        self.assertIn("fake-IP", err.getvalue())
        self.assertIn("198.18.0.50", err.getvalue())

    def test_report_flags_the_cell_and_a_genuine_one_is_not_flagged(self):
        rows = [row("home", "proxied", 0.4, "2026-01-01T00:00:00Z", ip="198.18.0.50"),
                row("home", "real", 85.0, "2026-01-01T00:00:00Z", ip="203.0.113.7")]
        with tempfile.TemporaryDirectory() as d:
            text, _ = run_report([write_csv(d, "f.csv", rows)])
        self.assertIn("home->proxied: resolved into 198.18.0.0/15", text)
        self.assertNotIn("home->real: resolved into", text)


class Ranking(unittest.TestCase):
    def test_pair_ranking_sums_both_legs_and_penalises_loss(self):
        rows = []
        for site, a, b, c in (("home", 30, 50, 20), ("school", 40, 45, 25)):
            rows += [row(site, "A", a, "2026-01-01T00:00:00Z"),
                     row(site, "B", b, "2026-01-01T00:00:00Z"),
                     # C is fastest but lossy: 2% on each leg costs 2 * 2 * 10 ms
                     row(site, "C", c, "2026-01-01T00:00:00Z", loss=2.0)]
        agg = rr.aggregate(rows)
        ranked = rr.rank_pair(agg, ["A", "B", "C"], "home", "school", 10.0)
        totals = {n: t for t, n, _, _ in ranked}
        self.assertEqual(totals["A"], 30 + 40)
        self.assertEqual(totals["B"], 50 + 45)
        self.assertEqual(totals["C"], 20 + 25 + 10 * (2.0 + 2.0))
        # C has the lowest RTT on both legs but loses to A once loss is priced in.
        self.assertEqual([n for _, n, _, _ in ranked], ["A", "C", "B"])

    def test_flags(self):
        rows = [row("lab", "near", 0.4, "2026-01-01T00:00:00Z"),
                row("lab", "lossy", 80, "2026-01-01T00:00:00Z", loss=9.0)]
        text, _ = run_report([write_csv(tempfile.mkdtemp(), "a.csv", rows)])
        self.assertIn("implausibly low", text)
        self.assertIn("mean loss 9.0%", text)


class Periods(unittest.TestCase):
    """Site clock UTC+8, so local 20:00 is 12:00Z and local 10:00 is 02:00Z."""
    PEAK, OFF = "2026-01-0{}T12:00:00Z", "2026-01-0{}T02:00:00Z"

    def csv(self, d, a_peak, a_off, b=70, offset=480, runs=4, off_per_day=1):
        rows = []
        for site in ("home", "school"):
            for day in range(1, runs + 1):
                for _ in range(off_per_day):
                    rows += [row(site, "A", a_off, self.OFF.format(day), offset),
                             row(site, "B", b, self.OFF.format(day), offset)]
                rows += [row(site, "A", a_peak, self.PEAK.format(day), offset),
                         row(site, "B", b, self.PEAK.format(day), offset)]
        return write_csv(d, "r.csv", rows)

    def test_ranking_that_flips_in_the_evening_is_reported(self):
        with tempfile.TemporaryDirectory() as d:
            # A is fast all day except at peak, when it degrades past B.
            text, code = run_report([self.csv(d, a_peak=120, a_off=40), "--periods",
                                     "--pair", "home", "school"])
        self.assertIsNone(code)
        self.assertIn("YES: best at peak is 'B', best off-peak is 'A'", text)
        self.assertIn("+200%", text)  # A: 80 off-peak -> 240 at peak

    def test_stable_ranking_is_reported_as_such(self):
        with tempfile.TemporaryDirectory() as d:
            text, _ = run_report([self.csv(d, a_peak=41, a_off=40), "--periods",
                                  "--pair", "home", "school"])
        self.assertIn("NO: 'A' is best in both periods", text)

    def test_a_single_median_hides_what_periods_expose(self):
        # Evening is a few hours of a day, so most runs are off-peak: here 3 of 4.
        # The overall median is then A's good value, A still ranks first, and the
        # collapse at peak is invisible. This is what --periods exists to expose.
        with tempfile.TemporaryDirectory() as d:
            p = self.csv(d, a_peak=120, a_off=40, runs=4, off_per_day=3)
            plain, _ = run_report([p, "--pair", "home", "school"])
            split, _ = run_report([p, "--periods", "--pair", "home", "school"])
        best = [ln for ln in plain.splitlines() if "<-- best" in ln][0]
        self.assertTrue(best.strip().startswith("A "), best)
        self.assertIn("YES: best at peak is 'B', best off-peak is 'A'", split)

    def test_offset_is_applied_per_site(self):
        # Same UTC instant; local hour differs by site, so only one site is "peak".
        rows = [row("east", "A", 50, "2026-01-01T12:00:00Z", 480),   # 20:00 local
                row("west", "A", 50, "2026-01-01T12:00:00Z", -480)]  # 04:00 local
        self.assertEqual(rr.local_hour(rows[0]), 20)
        self.assertEqual(rr.local_hour(rows[1]), 4)

    def test_old_csv_without_offset_is_left_out_and_said_so(self):
        with tempfile.TemporaryDirectory() as d:
            rows = [row("home", "A", 50, "2026-01-01T12:00:00Z", offset=480),
                    row("home", "A", 50, "2026-01-01T02:00:00Z", offset=480),
                    row("home", "A", 50, "2026-01-01T12:00:00Z", offset=None)]
            text, _ = run_report([write_csv(d, "m.csv", rows), "--periods"])
        self.assertIn("1 row(s) have no utc_offset_min", text)

    def test_no_evening_data_is_an_error_not_a_silent_pass(self):
        with tempfile.TemporaryDirectory() as d:
            rows = [row("home", "A", 50, "2026-01-01T02:00:00Z", offset=480)]
            _, code = run_report([write_csv(d, "m.csv", rows), "--periods"])
        self.assertIn("no run falls inside the peak hours", str(code))

    def test_thin_period_is_flagged(self):
        with tempfile.TemporaryDirectory() as d:
            text, _ = run_report([self.csv(d, 120, 40, runs=2), "--periods"])
        self.assertIn("only 2 run(s) in this period", text)


SAMPLE_FIXTURE = os.path.join(HERE, "testdata", "routes-sample.txt")


def run_routes(d, rows, sites_text, *extra):
    """Run `routes` in-process; return (stdout, exit code, file text or None)."""
    csv_path = write_csv(d, "m.csv", rows)
    sites = write_nodes(d, sites_text)  # same plain-text writer
    out = os.path.join(d, "routes.txt")
    old, buf, code = sys.argv, io.StringIO(), None
    sys.argv = ["relay_rtt.py", "routes", csv_path, "--sites", sites, "--out", out,
                *extra]
    try:
        with contextlib.redirect_stdout(buf):
            rr.main()
    except SystemExit as e:
        code = e.code
    finally:
        sys.argv = old
    text = open(out, encoding="utf-8").read() if os.path.exists(out) else None
    return buf.getvalue(), code, text


def node_rows(site, node, host, port, peak, off, offset=480, days=4, off_per_day=3,
              loss=0.0, ip="203.0.113.7"):
    """Evening runs at 20:00 site-local and off-peak runs at 10:00, UTC+8."""
    rows = []
    for day in range(1, days + 1):
        for _ in range(off_per_day):
            rows.append(row(site, node, off, f"2026-01-0{day}T02:00:00Z", offset,
                            host=host, port=port, ip=ip, loss_pct=loss))
        rows.append(row(site, node, peak, f"2026-01-0{day}T12:00:00Z", offset,
                        host=host, port=port, ip=ip, loss_pct=loss))
    return rows


SITES = "home    203.0.113.0/24\nschool  198.51.100.0/24, 2001:db8:1::/48\n"


def sample_rows():
    rows = []
    for site, hk, sh, fra in (("home", (38, 40), (95, 96), (210, 205)),
                              ("school", (80, 82), (12, 14), (190, 185))):
        rows += node_rows(site, "hk", "hk.example.com", 31107, *hk)
        rows += node_rows(site, "sh", "sh.example.com", 31107, *sh)
        rows += node_rows(site, "fra", "fra.example.com", 31107, *fra)
    return rows


class Routes(unittest.TestCase):
    def line(self, text, net):
        return [ln for ln in text.splitlines() if ln.startswith(net)][0]

    def test_worst_of_peak_and_off_peak_beats_the_overall_median(self):
        rows = (node_rows("home", "hk", "hk.example.com", 31107, peak=120, off=40)
                + node_rows("home", "sh", "sh.example.com", 31107, peak=70, off=70))
        with tempfile.TemporaryDirectory() as d:
            _, _, worst = run_routes(d, rows, "home 203.0.113.0/24\n")
            _, _, plain = run_routes(d, rows, "home 203.0.113.0/24\n", "--basis", "all")
        # hk is brilliant by day and poor in the evening; the overall median hides
        # that, so ranked by it hk wins. A static table must not walk into it.
        self.assertEqual(self.line(plain, "203.0.113.0/24"),
                         "203.0.113.0/24  hk.example.com:31107=40.0,sh.example.com:31107=70.0")
        self.assertEqual(self.line(worst, "203.0.113.0/24"),
                         "203.0.113.0/24  sh.example.com:31107=70.0,hk.example.com:31107=120.0")

    def test_loss_is_priced_in(self):
        rows = (node_rows("home", "hk", "hk.example.com", 31107, 20, 20, loss=2.0)
                + node_rows("home", "sh", "sh.example.com", 31107, 35, 35))
        with tempfile.TemporaryDirectory() as d:
            _, _, text = run_routes(d, rows, "home 203.0.113.0/24\n")
        # hk: 20 + 2% * 10 ms = 40, which loses to sh at 35.
        self.assertIn("sh.example.com:31107=35.0,hk.example.com:31107=40.0", text)

    def test_a_proxy_measurement_is_never_written(self):
        rows = (node_rows("home", "hk", "hk.example.com", 31107, 0.4, 0.4, ip="198.18.0.9")
                + node_rows("home", "sh", "sh.example.com", 31107, 60, 60))
        with tempfile.TemporaryDirectory() as d:
            out, _, text = run_routes(d, rows, "home 203.0.113.0/24\n")
        self.assertNotIn("hk.example.com", text)
        self.assertIn("sh.example.com:31107=60.0", text)
        self.assertIn("fake-IP", out)

    def test_sites_without_data_and_data_without_sites_are_reported(self):
        rows = node_rows("home", "hk", "hk.example.com", 31107, 30, 30) \
            + node_rows("cafe", "hk", "hk.example.com", 31107, 30, 30)
        with tempfile.TemporaryDirectory() as d:
            out, _, text = run_routes(d, rows, "home 203.0.113.0/24\nschool 198.51.100.0/24\n")
        self.assertIn("site 'school' is in", out)
        self.assertIn("site 'cafe' has measurements but is not in", out)
        self.assertNotIn("198.51.100.0/24", text)

    def test_a_thin_evening_falls_back_and_says_so(self):
        rows = node_rows("home", "hk", "hk.example.com", 31107, 120, 40, days=2)
        with tempfile.TemporaryDirectory() as d:
            out, _, text = run_routes(d, rows, "home 203.0.113.0/24\n")
        self.assertIn("used the overall median", out)
        self.assertIn("hk.example.com:31107=40.0", text)  # not 120: it is not trusted

    def test_default_line_is_the_average_when_asked(self):
        with tempfile.TemporaryDirectory() as d:
            _, _, text = run_routes(d, sample_rows(), SITES, "--default", "average")
        # hk: (40 + 82) / 2 = 61 ; sh: (96 + 14) / 2 = 55
        self.assertIn("0.0.0.0/0  sh.example.com:31107=55.0,hk.example.com:31107=61.0", text)
        self.assertIn("::/0  ", text)

    def test_bad_sites_files_are_refused_with_the_line_number(self):
        for text, want in (("home\n", "need `label CIDR"),
                           ("home 203.0.113.0/99\n", "not a network"),
                           ("home 10.0.0.0/8\nhome 10.1.0.0/16\n", "appears twice")):
            with tempfile.TemporaryDirectory() as d:
                _, code, written = run_routes(d, sample_rows(), text)
            self.assertIn(want, str(code), text)
            self.assertIsNone(written, "a refused sites file must not leave a table")

    def test_the_two_languages_agree_on_the_format(self):
        # The Python tool writes the table and the Rust server reads it. This pins
        # the sample the Rust unit test parses (src/relay_routes.rs) to what the tool
        # really produces, so neither side can drift alone.
        with tempfile.TemporaryDirectory() as d:
            _, _, text = run_routes(d, sample_rows(), SITES, "--default", "average")
        if os.environ.get("UPDATE_FIXTURE"):
            with open(SAMPLE_FIXTURE, "w", encoding="utf-8") as f:
                f.write(text)
        with open(SAMPLE_FIXTURE, encoding="utf-8") as f:
            self.assertEqual(text, f.read(), "run with UPDATE_FIXTURE=1 if intended")


class EndToEnd(unittest.TestCase):
    """Real sockets on loopback: probe -> CSV -> report, through the CLI."""

    def test_probe_then_report(self):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(16)
        up = listener.getsockname()[1]

        def accept_loop():
            while True:
                try:
                    c, _ = listener.accept()
                    c.close()
                except OSError:
                    return
        threading.Thread(target=accept_loop, daemon=True).start()

        dead = socket.socket()
        dead.bind(("127.0.0.1", 0))
        down = dead.getsockname()[1]
        dead.close()  # nothing listens here now

        try:
            with tempfile.TemporaryDirectory() as d:
                nodes = write_nodes(d, f"up 127.0.0.1 {up}\ndown 127.0.0.1 {down}\n")
                out = os.path.join(d, "o.csv")
                p = subprocess.run(
                    [sys.executable, TOOL, "probe", "--nodes", nodes, "--site", "lo",
                     "--samples", "5", "--interval", "0", "--timeout", "1", "--out", out],
                    capture_output=True, text=True, timeout=60)
                self.assertEqual(p.returncode, 0, p.stderr)
                with open(out, newline="") as f:
                    got = {r["node"]: r for r in csv.DictReader(f)}
                self.assertEqual(got["up"]["loss_pct"], "0.0")
                self.assertEqual(got["down"]["loss_pct"], "100.0")
                self.assertIn("ConnectionRefusedError", got["down"]["error"])
                self.assertRegex(got["up"]["utc_offset_min"], r"^-?\d+$")

                r = subprocess.run([sys.executable, TOOL, "report", out],
                                   capture_output=True, text=True, timeout=60)
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertIn("most runs failed", r.stdout)  # the dead node is flagged
        finally:
            listener.close()


if __name__ == "__main__":
    unittest.main()
