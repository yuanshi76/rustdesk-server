#!/usr/bin/env python3
"""relay_rtt.py - layer-A latency campaign for candidate RustDesk relay nodes.

Measures TCP connect time (SYN -> SYN/ACK, i.e. about one round trip) from the
machine it runs on to every relay node, and aggregates results from several
sites into a (site x relay) matrix.

Standard library only; Python >= 3.8; runs on Windows, Linux and macOS.

  probe : run on each site (home, school, laptop ...), ideally several times
          across the day, including the evening peak.
  report: run anywhere on the collected CSV files.

Limits (read before trusting the numbers):
  * Connect time approximates RTT only if nothing on the path answers the SYN
    locally. A transparent proxy / TUN-mode VPN / some security suites do, and
    then every node looks ~1 ms. Run with the proxy off, or check that values
    differ between nodes by plausible amounts.
  * It measures the path to the relay's *listening port*, not throughput.
    Packet loss and jitter are reported because they often matter more than
    median RTT on congested long-haul routes.
  * A relayed session crosses two legs. `report --pair A B` ranks nodes by the
    sum of the two sites' medians; it does not model the real session.
"""
import argparse
import csv
import datetime as dt
import glob
import ipaddress
import os
import socket
import statistics
import sys
import time
from collections import defaultdict

FIELDS = ["ts_utc", "site", "node", "host", "port", "ip", "n", "ok",
          "loss_pct", "min_ms", "median_ms", "p90_ms", "jitter_ms", "error",
          "utc_offset_min"]


# ----------------------------------------------------------------- helpers
def read_nodes(path):
    """nodes file: one node per line `id host port [region]`, '#' comments.
    Separator: whitespace or comma."""
    nodes = []
    with open(path, encoding="utf-8") as f:
        for ln, line in enumerate(f, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            parts = line.replace(",", " ").split()
            if len(parts) < 3:
                sys.exit(f"{path}:{ln}: need `id host port [region]`")
            try:
                port = int(parts[2])
            except ValueError:
                sys.exit(f"{path}:{ln}: bad port {parts[2]!r}")
            nodes.append({"id": parts[0], "host": parts[1], "port": port,
                          "region": parts[3] if len(parts) > 3 else ""})
    if not nodes:
        sys.exit("no nodes in " + path)
    ids = [n["id"] for n in nodes]
    if len(set(ids)) != len(ids):
        sys.exit("duplicate node ids in " + path)
    return nodes


def percentile(sorted_vals, p):
    if not sorted_vals:
        return None
    k = (len(sorted_vals) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(sorted_vals) - 1)
    return sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (k - lo)


def connect_ms(ip, port, family, timeout):
    s = socket.socket(family, socket.SOCK_STREAM)
    s.settimeout(timeout)
    t0 = time.perf_counter()
    try:
        s.connect((ip, port))
        return (time.perf_counter() - t0) * 1000.0, None
    except OSError as e:
        return None, e.__class__.__name__
    finally:
        s.close()


def resolve(host, port):
    """Resolve once per run so DNS time never enters the measurement."""
    infos = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    # prefer IPv4 for comparability across sites
    infos.sort(key=lambda i: 0 if i[0] == socket.AF_INET else 1)
    fam, _, _, _, sa = infos[0]
    return fam, sa[0]


# Proxy and TUN tools that hijack DNS ("fake-IP" mode) answer every name with an
# address from this benchmarking block and then terminate the connection on the
# local machine. Whatever is measured against such an address is the loopback
# interface of your own proxy, never the node. Timing alone only suggests this;
# the address is conclusive.
FAKE_IP_NET = ipaddress.ip_network("198.18.0.0/15")


def is_fake_ip(ip):
    try:
        return ipaddress.ip_address(ip) in FAKE_IP_NET
    except ValueError:
        return False


def summarise(samples, n):
    ok = sorted(samples)
    row = {"n": n, "ok": len(ok), "loss_pct": round(100.0 * (n - len(ok)) / n, 1)}
    if ok:
        row["min_ms"] = round(ok[0], 2)
        row["median_ms"] = round(statistics.median(ok), 2)
        row["p90_ms"] = round(percentile(ok, 0.9), 2)
    return row


def jitter(seq):
    """Mean absolute difference of consecutive successful samples (time order)."""
    d = [abs(b - a) for a, b in zip(seq, seq[1:])]
    return round(statistics.mean(d), 2) if d else None


# ------------------------------------------------------------------- probe
def probe_once(a, nodes, quiet=False, warn=True):
    """One run against every node: write its CSV and return (rows, path).
    `quiet` skips the table; `warn` prints the DNS and proxy warnings, which a
    repeating run wants once, not every time."""
    ts = dt.datetime.now(dt.timezone.utc)
    out = a.out or os.path.join(
        "results", f"{a.site}-{ts.strftime('%Y%m%dT%H%M%SZ')}.csv")
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)

    resolved, rows, series = {}, [], {}
    for nd in nodes:
        try:
            resolved[nd["id"]] = resolve(nd["host"], nd["port"])
        except OSError as e:
            resolved[nd["id"]] = None
            if warn:
                print(f"[{nd['id']}] DNS failure: {e}", file=sys.stderr)
        series[nd["id"]] = []
        r = resolved[nd["id"]]
        if warn and r is not None and is_fake_ip(r[1]):
            print(f"[{nd['id']}] WARNING: {nd['host']} resolved to {r[1]}, an address "
                  "range used by proxy/TUN 'fake-IP' DNS. These numbers measure your "
                  "own proxy, not the node. Turn it off and run again.",
                  file=sys.stderr)

    # interleave nodes per round so a transient event hits all nodes alike
    errs = defaultdict(lambda: defaultdict(int))
    for i in range(a.samples):
        for nd in nodes:
            r = resolved[nd["id"]]
            if r is None:
                continue
            ms, err = connect_ms(r[1], nd["port"], r[0], a.timeout)
            series[nd["id"]].append(ms)
            if err:
                errs[nd["id"]][err] += 1
        if i + 1 < a.samples:
            time.sleep(a.interval)

    now = ts.strftime("%Y-%m-%dT%H:%M:%SZ")
    offset_min = int(dt.datetime.now().astimezone().utcoffset().total_seconds() // 60)
    if not quiet:
        print(f"{'node':<14}{'ip':<18}{'loss%':>6}{'min':>9}{'median':>9}"
              f"{'p90':>9}{'jitter':>9}")
    for nd in nodes:
        r = resolved[nd["id"]]
        row = {"ts_utc": now, "site": a.site, "node": nd["id"],
               "host": nd["host"], "port": nd["port"],
               "ip": r[1] if r else "", "n": a.samples, "ok": 0,
               "loss_pct": 100.0, "min_ms": "", "median_ms": "",
               "p90_ms": "", "jitter_ms": "", "error": "",
               "utc_offset_min": offset_min}
        if r is None:
            row["error"] = "dns"
        else:
            seq = series[nd["id"]]
            row.update(summarise([x for x in seq if x is not None], a.samples))
            j = jitter([x for x in seq if x is not None])
            row["jitter_ms"] = "" if j is None else j
            if errs[nd["id"]]:
                row["error"] = ";".join(f"{k}x{v}" for k, v in
                                        sorted(errs[nd["id"]].items()))
        rows.append(row)
        if not quiet:
            f = lambda k: ("-" if row[k] in ("", None) else f"{row[k]:.1f}")
            print(f"{nd['id']:<14}{row['ip']:<18}{row['loss_pct']:>6}"
                  f"{f('min_ms'):>9}{f('median_ms'):>9}{f('p90_ms'):>9}"
                  f"{f('jitter_ms'):>9}")

    with open(out, "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, fieldnames=FIELDS)
        w.writeheader()
        w.writerows(rows)
    return rows, out


def run_summary(rows):
    """One line for a repeating run: each node's median, and anything wrong."""
    parts = []
    for r in rows:
        if r["median_ms"] in ("", None):
            parts.append(f"{r['node']} DOWN")
        else:
            lossy = f" ({float(r['loss_pct']):.0f}% lost)" if float(r["loss_pct"]) else ""
            ms = float(r["median_ms"])
            parts.append(f"{r['node']} {ms:.0f} ms{lossy}" if ms >= 10
                         else f"{r['node']} {ms:.1f} ms{lossy}")
    return "  ".join(parts)


def probe_repeatedly(a, nodes):
    """Run again and again, so a three-day campaign is one command, not a
    scheduler. Every run is written as soon as it finishes, so stopping at any
    moment, or the machine going to sleep, loses nothing already measured."""
    if a.out:
        sys.exit("--out cannot be combined with --repeat-every: every run writes "
                 "its own file in results/")
    every = a.repeat_every * 60.0
    end = time.time() + a.repeat_for * 3600.0
    stop_at = dt.datetime.fromtimestamp(end).strftime("%a %d %b %H:%M")
    print(f"Measuring {len(nodes)} node(s) from '{a.site}' every "
          f"{a.repeat_every:g} min until {stop_at} (about {a.repeat_for:g} h).\n"
          "Leave this window open and the computer awake. Ctrl-C stops it; "
          "every finished run is already saved.\n")
    runs, results_dir, first = 0, "results", True
    next_at = time.monotonic()
    try:
        while True:
            rows, path = probe_once(a, nodes, quiet=True, warn=first)
            first, runs = False, runs + 1
            results_dir = os.path.dirname(path) or "."
            print(f"{dt.datetime.now().strftime('%a %H:%M')}  run {runs:<4} "
                  f"{run_summary(rows)}", flush=True)
            next_at += every
            delay = next_at - time.monotonic()
            if delay < 0:
                # Slept through one or more slots: carry on from now, no burst.
                next_at, delay = time.monotonic(), 0.0
            if time.time() + delay >= end:
                break
            time.sleep(delay)
    except KeyboardInterrupt:
        print("\nStopped.")
    print(f"\n{runs} run(s) saved in {results_dir}/")


def cmd_probe(a):
    nodes = read_nodes(a.nodes)
    if a.repeat_every:
        return probe_repeatedly(a, nodes)
    rows, out = probe_once(a, nodes)
    print(f"\nwrote {out}")


# ------------------------------------------------------------------ report
def expand(patterns):
    """Windows' cmd and PowerShell pass `results/*.csv` through unexpanded, so do it
    here. A name that exists is used as it is; a pattern that matches nothing is an
    error, not a silently empty report."""
    paths = []
    for pat in patterns:
        if os.path.exists(pat) or not any(c in pat for c in "*?["):
            paths.append(pat)
            continue
        found = sorted(glob.glob(pat))
        if not found:
            sys.exit(f"no files match {pat!r}. Is this the folder that holds "
                     "the results?")
        paths += found
    return paths


def load(paths):
    rows = []
    for p in expand(paths):
        with open(p, newline="", encoding="utf-8") as fh:
            for r in csv.DictReader(fh):
                rows.append(r)
    return rows


def num(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def aggregate(rows, min_ok_frac=0.5):
    """(site, node) -> dict of aggregated stats over runs."""
    g = defaultdict(list)
    for r in rows:
        g[(r["site"], r["node"])].append(r)
    agg = {}
    for key, rs in g.items():
        meds = [num(r["median_ms"]) for r in rs if num(r["median_ms"]) is not None]
        p90s = [num(r["p90_ms"]) for r in rs if num(r["p90_ms"]) is not None]
        loss = [num(r["loss_pct"]) for r in rs if num(r["loss_pct"]) is not None]
        jit = [num(r["jitter_ms"]) for r in rs if num(r["jitter_ms"]) is not None]
        agg[key] = {
            "runs": len(rs),
            "median": statistics.median(meds) if meds else None,
            "worst_run_median": max(meds) if meds else None,
            "p90": statistics.median(p90s) if p90s else None,
            "loss": statistics.mean(loss) if loss else 100.0,
            "jitter": statistics.mean(jit) if jit else None,
            "usable": bool(meds) and len(meds) >= min_ok_frac * len(rs),
        }
    return agg


def fmt(x, nd=0):
    return "-" if x is None else f"{x:.{nd}f}"


def fake_ip_cells(rows):
    """{(site, node)} whose resolved address is in the fake-IP range."""
    return {(r["site"], r["node"]) for r in rows if is_fake_ip(r.get("ip", ""))}


def local_hour(row):
    """Hour of day (0-23) at the site when the run started, or None for a CSV
    written before the UTC offset was recorded (those cannot be placed)."""
    off = num(row.get("utc_offset_min"))
    if off is None:
        return None
    try:
        t = dt.datetime.strptime(row["ts_utc"], "%Y-%m-%dT%H:%M:%SZ")
    except (KeyError, ValueError):
        return None
    return (t + dt.timedelta(minutes=off)).hour


def parse_hours(spec):
    """'19-23' -> {19,20,21,22,23}. Wraps midnight: '22-2' -> {22,23,0,1,2}."""
    try:
        lo, hi = (int(x) for x in spec.split("-"))
        if not (0 <= lo <= 23 and 0 <= hi <= 23):
            raise ValueError
    except ValueError:
        sys.exit(f"bad --peak-hours {spec!r}; expected something like 19-23")
    hours, h = {lo}, lo
    while h != hi:
        h = (h + 1) % 24
        hours.add(h)
    return hours


def rank_pair(agg, nodes, s1, s2, loss_penalty):
    """[(total_ms, node, stats_s1, stats_s2)] best first. A relayed session
    crosses both legs, so a node costs the sum of the two sites' medians plus a
    penalty per percent of loss."""
    scored = []
    for n in nodes:
        g1, g2 = agg.get((s1, n)), agg.get((s2, n))
        if not g1 or not g2 or g1["median"] is None or g2["median"] is None:
            continue
        total = g1["median"] + g2["median"] + loss_penalty * (g1["loss"] + g2["loss"])
        scored.append((total, n, g1, g2))
    scored.sort(key=lambda t: (t[0], t[1]))
    return scored


def print_view(rows, a, title=None, min_runs=0):
    """Matrix, flags and (with --pair) the ranking for one set of rows.
    Returns the ranking, or None without --pair."""
    agg = aggregate(rows)
    sites = sorted({k[0] for k in agg})
    nodes = sorted({k[1] for k in agg})
    fake = fake_ip_cells(rows)

    if title:
        print(f"\n=== {title} ===")
    print("Median connect time, ms (median over runs); loss% in brackets\n")
    print(f"{'node':<14}" + "".join(f"{s:>18}" for s in sites))
    for n in nodes:
        line = f"{n:<14}"
        for s in sites:
            g = agg.get((s, n))
            line += f"{'-':>18}" if g is None else \
                f"{fmt(g['median'], 1) + ' [' + fmt(g['loss'], 1) + ']':>18}"
        print(line)
    print("\nruns per site: " + ", ".join(
        f"{s}={max(v['runs'] for k, v in agg.items() if k[0] == s)}"
        for s in sites))

    flags = []
    for s, n in sorted(fake):
        flags.append(f"{s}->{n}: resolved into 198.18.0.0/15, the fake-IP range of "
                     "proxy/TUN DNS. This cell measures your own proxy and is "
                     "worthless; discard it.")
    for (s, n), g in sorted(agg.items()):
        if g["loss"] >= a.loss_warn:
            flags.append(f"{s}->{n}: mean loss {g['loss']:.1f}%")
        if g["median"] and g["worst_run_median"] > 2 * g["median"]:
            flags.append(f"{s}->{n}: worst run {g['worst_run_median']:.0f} ms vs "
                         f"median {g['median']:.0f} ms (time-of-day effect?)")
        if g["median"] is not None and g["median"] < 2.0:
            flags.append(f"{s}->{n}: median {g['median']:.1f} ms is implausibly "
                         "low unless the node is local; check for a proxy/TUN")
        if not g["usable"]:
            flags.append(f"{s}->{n}: most runs failed; node unreachable?")
        if g["runs"] < min_runs:
            flags.append(f"{s}->{n}: only {g['runs']} run(s) in this period "
                         f"(want >= {min_runs}); do not trust this cell")
    if flags:
        print("\nFlags:")
        for fl in flags:
            print("  - " + fl)

    if not a.pair:
        return None
    s1, s2 = a.pair
    print(f"\nRanking for a session between '{s1}' and '{s2}' "
          f"(sum of medians + {a.loss_penalty:.0f} ms per 1% mean loss):")
    scored = rank_pair(agg, nodes, s1, s2, a.loss_penalty)
    if not scored:
        print("  no node has data from both sites")
        return scored
    best = scored[0][0]
    for total, n, g1, g2 in scored:
        tag = "  <-- best" if total == best else (
            "  <-- within margin of best" if total <= best * (1 + a.margin) else "")
        print(f"  {n:<14}{total:7.0f}  ({g1['median']:.0f} + {g2['median']:.0f}"
              f", loss {g1['loss']:.1f}/{g2['loss']:.1f}%){tag}")
    return scored


def cmd_report(a):
    rows = load(a.files)
    if not rows:
        sys.exit("no rows")

    if not a.periods:
        print_view(rows, a)
        if a.pair:
            print("\nCaveat: this ranks TCP connect time to the listener, not real "
                  "session quality. Treat a margin of <20 % as a tie.")
        return

    # Evening congestion on long-haul paths can reorder the ranking, and a single
    # median over all runs hides it unless more than half the runs are affected.
    # Split by the SITE'S local hour instead.
    peak_hours = parse_hours(a.peak_hours)
    peak, off, unplaced = [], [], 0
    for r in rows:
        h = local_hour(r)
        if h is None:
            unplaced += 1
        elif h in peak_hours:
            peak.append(r)
        else:
            off.append(r)
    if unplaced:
        print(f"note: {unplaced} row(s) have no utc_offset_min (older CSV) and "
              "cannot be placed in a period; they are left out.")
    if not peak:
        sys.exit(f"no run falls inside the peak hours {a.peak_hours} (site-local). "
                 "The campaign has to include the evening; run more probes then.")
    if not off:
        sys.exit("every run is inside the peak hours; probe at other times too, "
                 "or there is nothing to compare against.")

    r_peak = print_view(peak, a, f"PEAK (site-local {a.peak_hours})", a.min_runs)
    r_off = print_view(off, a, "OFF-PEAK (the rest of the day)", a.min_runs)

    if a.pair and r_peak and r_off:
        print("\n=== Does the ranking depend on the time of day? ===")
        bp, bo = r_peak[0][1], r_off[0][1]
        offp = {n: t for t, n, _, _ in r_off}
        for t, n, _, _ in r_peak:
            if n in offp:
                print(f"  {n:<14} off-peak {offp[n]:6.0f} ms -> peak {t:6.0f} ms "
                      f"({100.0 * (t - offp[n]) / offp[n]:+.0f}%)")
        if bp != bo:
            print(f"\n  YES: best at peak is '{bp}', best off-peak is '{bo}'. "
                  "This is evidence for time-of-day-aware selection.")
        else:
            print(f"\n  NO: '{bp}' is best in both periods.")
    print("\nCaveat: this ranks TCP connect time to the listener, not real "
          "session quality. Treat a margin of <20 % as a tie.")


# ------------------------------------------------------------------ routes
def read_sites(path):
    """sites file: `label CIDR[,CIDR ...]`, '#' comments. Returns
    [(label, [network, ...])]. A CIDR with host bits set is accepted and
    normalised, so `203.0.113.77/24` means 203.0.113.0/24."""
    sites, seen = [], set()
    with open(path, encoding="utf-8") as f:
        for ln, line in enumerate(f, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            parts = line.replace(",", " ").split()
            if len(parts) < 2:
                sys.exit(f"{path}:{ln}: need `label CIDR [CIDR ...]`")
            label, nets = parts[0], []
            if label in seen:
                sys.exit(f"{path}:{ln}: site {label!r} appears twice")
            seen.add(label)
            for text in parts[1:]:
                try:
                    nets.append(ipaddress.ip_network(text, strict=False))
                except ValueError:
                    sys.exit(f"{path}:{ln}: {text!r} is not a network such as "
                             "203.0.113.0/24")
            sites.append((label, nets))
    if not sites:
        sys.exit("no sites in " + path)
    return sites


def relay_names(rows):
    """node id -> the `host:port` string hbbs must be started with, or a clash."""
    names = {}
    for r in rows:
        name = f"{r['host']}:{r['port']}".lower()
        prev = names.setdefault(r["node"], name)
        if prev != name:
            sys.exit(f"node {r['node']!r} was measured as both {prev} and {name}; "
                     "the CSVs mix two different nodes files")
    return names


def site_values(rows, site, a, notes):
    """node -> milliseconds for one site, by the chosen basis.

    A static table cannot know the time of day, so the default is the WORSE of
    peak and off-peak: a relay that is excellent at 10:00 and poor at 21:00 is
    ranked by its 21:00. `all` ranks by the overall median instead, which hides an
    evening-only collapse; `peak` ranks by the evening alone."""
    mine = [r for r in rows if r["site"] == site]

    def values(subset):
        out = {}
        for (_, node), g in aggregate(subset).items():
            if g["usable"] and g["median"] is not None:
                out[node] = (g["median"] + a.loss_penalty * g["loss"], g["runs"])
        return out

    overall = values(mine)
    if a.basis == "all":
        return {n: v for n, (v, _) in overall.items()}

    hours = parse_hours(a.peak_hours)
    peak = [r for r in mine if local_hour(r) in hours]
    off = [r for r in mine if local_hour(r) is not None and local_hour(r) not in hours]
    vp, vo = values(peak), values(off)
    out = {}
    for node, (v_all, _) in overall.items():
        have_p = node in vp and vp[node][1] >= a.min_runs
        have_o = node in vo and vo[node][1] >= a.min_runs
        if a.basis == "peak" and have_p:
            out[node] = vp[node][0]
        elif a.basis == "worst" and have_p and have_o:
            out[node] = max(vp[node][0], vo[node][0])
        else:
            out[node] = v_all
            notes.append(f"{site}->{node}: fewer than {a.min_runs} runs in "
                         f"{'the evening' if a.basis == 'peak' or not have_p else 'off-peak'}"
                         f"; used the overall median instead of the {a.basis} basis")
    return out


def cmd_routes(a):
    rows = load(a.files)
    if not rows:
        sys.exit("no rows")
    sites = read_sites(a.sites)
    names = relay_names(rows)
    notes = []

    # Never write a number measured through a proxy.
    fake = fake_ip_cells(rows)
    if fake:
        for s_, n in sorted(fake):
            notes.append(f"{s_}->{n}: resolved into 198.18.0.0/15 (proxy/TUN "
                         "fake-IP); that measurement was discarded")
        rows = [r for r in rows if (r["site"], r["node"]) not in fake]

    measured = {r["site"] for r in rows}
    for label in sorted(measured - {l for l, _ in sites}):
        notes.append(f"site {label!r} has measurements but is not in {a.sites}; "
                     "it gets no line")

    per_site, lines = {}, []
    for label, nets in sites:
        if label not in measured:
            notes.append(f"site {label!r} is in {a.sites} but has no usable "
                         "measurements; it gets no line")
            continue
        vals = site_values(rows, label, a, notes)
        if not vals:
            notes.append(f"site {label!r}: no node answered reliably; it gets no line")
            continue
        per_site[label] = vals
        lines.append((label, nets, vals))

    default = None
    if a.default == "average" and per_site:
        common = set.intersection(*(set(v) for v in per_site.values()))
        if common:
            default = {n: sum(v[n] for v in per_site.values()) / len(per_site)
                       for n in common}
        else:
            notes.append("--default average: no relay was measured from every site")
    elif a.default not in ("none", "average"):
        if a.default in per_site:
            default = per_site[a.default]
        else:
            sys.exit(f"--default {a.default!r} is not a site with measurements")

    def entry(vals):
        order = sorted(vals.items(), key=lambda kv: (kv[1], kv[0]))
        return ",".join(f"{names[n]}={v:.1f}" for n, v in order)

    out = ["# Relay routing table, generated by relay_rtt.py routes.",
           f"# basis: {a.basis}"
           + (f" (peak hours {a.peak_hours}, site-local)" if a.basis != "all" else "")
           + f"; loss counted as {a.loss_penalty:g} ms per 1%.",
           "# Each relay must be written exactly as hbbs was started with it (-r).",
           "# Format: network  relay=ms[,relay=ms ...]", ""]
    for label, nets, vals in lines:
        out.append(f"# {label}")
        for net in nets:
            out.append(f"{net}  {entry(vals)}")
    if default:
        out += ["", "# anything else: " + ("the average of all sites"
                if a.default == "average" else f"as {a.default}")]
        out += [f"0.0.0.0/0  {entry(default)}", f"::/0  {entry(default)}"]
    if not lines:
        sys.exit("nothing to write: no site had usable measurements\n"
                 + "\n".join("  - " + n for n in notes))
    text = "\n".join(out) + "\n"
    with open(a.out, "w", encoding="utf-8") as fh:
        fh.write(text)

    print(f"wrote {a.out}: {len(lines)} site(s)"
          + (", plus a default" if default else ""))
    for label, _, vals in lines:
        order = sorted(vals.items(), key=lambda kv: kv[1])
        print(f"  {label:<14}" + "  ".join(f"{names[n]} {v:.0f} ms" for n, v in order))
    if notes:
        print("\nNotes:")
        for n in notes:
            print("  - " + n)
    print("\nCheck it on the server with:  test-relay <ip-at-site-A> <ip-at-site-B>")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("probe", help="measure from this machine")
    p.add_argument("--nodes", required=True, help="nodes file")
    p.add_argument("--site", required=True, help="label for this machine/location")
    p.add_argument("--samples", type=int, default=20)
    p.add_argument("--interval", type=float, default=0.5, help="seconds between rounds")
    p.add_argument("--timeout", type=float, default=3.0)
    p.add_argument("--out", help="CSV path (default results/<site>-<utc>.csv)")
    p.add_argument("--repeat-every", type=float, metavar="MINUTES",
                   help="keep measuring, once every this many minutes, instead of "
                        "once. Each run is written as its own file")
    p.add_argument("--repeat-for", type=float, default=72, metavar="HOURS",
                   help="with --repeat-every, stop after this long (default 72 = "
                        "3 days)")
    p.set_defaults(fn=cmd_probe)

    r = sub.add_parser("report", help="aggregate CSV files")
    r.add_argument("files", nargs="+")
    r.add_argument("--pair", nargs=2, metavar=("SITE_A", "SITE_B"))
    r.add_argument("--loss-warn", type=float, default=2.0)
    r.add_argument("--loss-penalty", type=float, default=10.0,
                   help="ms added per 1%% mean loss in ranking")
    r.add_argument("--margin", type=float, default=0.20)
    r.add_argument("--periods", action="store_true",
                   help="split runs into peak and off-peak by the site's local hour "
                        "and rank each separately")
    r.add_argument("--peak-hours", default="19-23",
                   help="site-local hours counted as peak, inclusive, e.g. 19-23 "
                        "or 22-2 (default 19-23)")
    r.add_argument("--min-runs", type=int, default=3,
                   help="with --periods, flag a cell with fewer runs than this")
    r.set_defaults(fn=cmd_report)

    t = sub.add_parser("routes", help="turn measurements into hbbs's routing table")
    t.add_argument("files", nargs="+", help="CSV files from `probe`")
    t.add_argument("--sites", required=True,
                   help="file mapping each site label to its public network(s)")
    t.add_argument("--out", default="relay_routes.txt")
    t.add_argument("--basis", choices=["worst", "peak", "all"], default="worst",
                   help="which figure ranks a relay (default: the worse of peak "
                        "and off-peak)")
    t.add_argument("--peak-hours", default="19-23")
    t.add_argument("--min-runs", type=int, default=3,
                   help="runs needed in a period before trusting it")
    t.add_argument("--loss-penalty", type=float, default=10.0,
                   help="ms added per 1%% mean loss")
    t.add_argument("--default", default="none",
                   help="line for clients matching no site: none, average, or a "
                        "site label to copy")
    t.set_defaults(fn=cmd_routes)

    a = ap.parse_args()
    if getattr(a, "samples", 1) < 1:
        ap.error("--samples must be >= 1")
    a.fn(a)


if __name__ == "__main__":
    main()
