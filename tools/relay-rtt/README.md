# relay-rtt — measure which relay node is closer, before building anything

Phase 9 of the plan. Python 3.8+, standard library only; Windows, Linux, macOS.

It answers one question: **do your relay nodes differ enough in latency, from the
places you actually connect from, to justify latency-aware relay selection?** If
the best node is within about 20% of the second best for every pair of sites that
matters, the answer is no, and the relay-selection work is not worth its
complexity. Run this first.

It measures TCP connect time to each relay's TCP port, which is about one round
trip. It does not measure throughput.

## Run it

1. `cp nodes.example.txt nodes.txt` and list your relays: `id host port [region]`.
   The port is the one **clients** reach, e.g. the host-side mapping of hbbr's
   21117, not the port inside the container.
2. On every site you care about — home, school, a laptop — with **any proxy, TUN
   or VPN turned off**:

       python3 relay_rtt.py probe --nodes nodes.txt --site home

   20 probes per node, written to `results/<site>-<utc>.csv`.
3. Repeat at least six times a day for three or more days, **including the
   evening** (see below). Collect every CSV in one folder, then:

       python3 relay_rtt.py report results/*.csv --pair home school --periods

## Why `--periods`

The hypothesis behind this whole exercise is that long-haul paths congest in the
evening and the ranking changes. A single median per node cannot show that: if
evening is a few hours of a day, most runs are off-peak and the median is the
off-peak value. `--periods` places every run in **its own site's local time** and
ranks peak (default 19–23, change with `--peak-hours`) and off-peak separately,
then states plainly whether the best node differs. It also flags any cell with
fewer than `--min-runs` (default 3) runs, because a campaign that never sampled
the evening has not tested the hypothesis.

CSVs written before this field existed carry no UTC offset; `--periods` leaves
them out and says how many.

## Scheduling

Every three hours, plus extra evening samples, so each site gets several
peak-hour runs a day. Cron (Linux, macOS):

    0 */3 * * *      cd ~/relay-rtt && python3 relay_rtt.py probe --nodes nodes.txt --site home
    15,45 19-22 * * * cd ~/relay-rtt && python3 relay_rtt.py probe --nodes nodes.txt --site home

Windows (**not tested here**; check it with `schtasks /Query /TN relay-rtt`):

    schtasks /Create /SC HOURLY /MO 3 /TN relay-rtt /TR "python C:\relay-rtt\relay_rtt.py probe --nodes C:\relay-rtt\nodes.txt --site school"

A laptop that sleeps will skip runs. That is fine as long as the report shows
enough of them.

## What can make the numbers wrong

- **A proxy or TUN answering for the node.** Tools in "fake-IP" mode hand out
  addresses from `198.18.0.0/15` and terminate the connection on your own
  machine, so every node looks about 0.5 ms away. The probe warns when it sees
  one of those addresses, and the report flags the cell. A full-tunnel VPN is
  different: it gives plausible-looking numbers that describe the VPN, and
  nothing in the tool can see it. Turn them off.
- **Connect time is not throughput.** A node can answer fast and still have a
  thin pipe.
- **A relayed session crosses two legs.** `--pair A B` ranks nodes by the sum of
  the two sites' medians plus 10 ms per 1% loss. That is a model, not the real
  session. Treat a margin under 20% as a tie.
- **DNS is resolved once per run** and excluded from the timing.

## Tests

    python3 -m unittest discover -s . -v

Real sockets on loopback for the probe-to-CSV-to-report path, synthetic CSVs for
the ranking and the peak/off-peak logic. It has also been run against real
remote hosts, which is how the fake-IP case was found.

Do not commit `results/` or `nodes.txt`; they name your servers.
