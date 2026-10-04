# relay-rtt

Measures TCP connect time (about one round trip) from the machine it runs on to each
of your relay servers, aggregates runs from several places, and writes the routing
table `hbbs` reads. Python 3.8+, standard library only; Windows, Linux, macOS.

**To use it, follow [`docs/measuring-latency.md`](../../docs/measuring-latency.md).**
That is the step-by-step guide. This page is the reference.

## Commands

| | |
|---|---|
| `probe --nodes nodes.txt --site NAME` | one run, about 10 seconds, saved to `results/` |
| `probe ... --repeat-every MINUTES [--repeat-for HOURS]` | keep going (default 72 h); each run is its own file; Ctrl-C is safe. No cron or Task Scheduler needed |
| `report results/*.csv [--pair A B] [--periods]` | the matrix, flags, and a ranking for a pair of places; `--periods` splits evening from the rest of the day |
| `routes results/*.csv --sites sites.txt --out relay_routes.txt` | writes the table for `hbbs` |

Wildcards like `results/*.csv` are expanded by the tool, because Windows' `cmd` and
PowerShell do not.

## Why the numbers can be wrong

- **A proxy or TUN answering for the server.** Tools in "fake-IP" mode hand out
  addresses from `198.18.0.0/15` and terminate the connection on your own machine, so
  every server looks about half a millisecond away. The probe warns on that range and
  the report flags the cell; `routes` never writes such a measurement. A full-tunnel
  VPN is different: it gives plausible numbers that describe the VPN, and nothing in
  the tool can see it. Turn them off.
- **Connect time is not throughput.** A server can answer quickly and have a thin pipe.
- **A relayed session crosses two legs.** `--pair A B` ranks servers by the sum of the
  two places' medians plus 10 ms per 1% loss. That is a model, not the real session.
  Treat a margin under 20% as a tie.
- **DNS is resolved once per run** and excluded from the timing.

## Why `--periods`

If evening is a few hours of a day, most runs are off-peak, and a single median per
server is the off-peak value: an evening-only collapse is invisible. `--periods`
places every run in its own place's local time (using the UTC offset each CSV
records), ranks peak (default 19-23, `--peak-hours`) and the rest separately, and says
plainly whether the best server differs. It flags any cell with fewer than
`--min-runs` (default 3) runs, since a campaign that skipped the evening has not
tested the idea. CSVs written before the offset was recorded cannot be placed; they are
left out and counted.

## `routes`: how a value is chosen

Per place and server: the median connect time plus `--loss-penalty` (default 10 ms)
per 1% mean loss. `--basis worst` (default) takes the **worse** of the evening and the
rest of the day, because a fixed table cannot know the time of day; `peak` takes the
evening alone; `all` the overall median, which hides an evening-only collapse. A
period with fewer than `--min-runs` runs falls back to the overall median, with a note.
`--default average` (or a place name) adds a line for clients in no listed place.
Sites and CSVs that do not match each other are reported, not guessed at.

## Tests

    python3 -m unittest discover -s tools/relay-rtt -v

Real sockets on loopback for the probe-to-CSV-to-report path, synthetic CSVs for the
ranking, the peak/off-peak logic and the generator, and a sample table that a Rust unit
test in `src/relay_routes.rs` also parses, so the two languages cannot drift apart.

Do not commit `results/` or `nodes.txt`; they name your servers.
