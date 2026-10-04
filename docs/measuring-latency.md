# Measuring latency to your relay servers

A step-by-step guide. You do this **once**, from the places where your RustDesk
clients are, and it takes about three days of waiting but only a few minutes of
your time.

**What it tells you:** how fast each of your relay servers is from each of those
places, so you can decide whether a second relay is worth having and which one
each place should use.

## Before you start

You need:

1. **Python 3.8 or newer** on one computer at each place. Check with
   `python3 --version` (on Windows, `py --version`). If you do not have it, install
   it from python.org and tick "Add Python to PATH".
2. **One file**, `relay_rtt.py`. It uses nothing but Python itself. Download it:

   ```
   curl -O https://raw.githubusercontent.com/yuanshi76/rustdesk-server/main/tools/relay-rtt/relay_rtt.py
   ```

   On Windows PowerShell use `curl.exe -O` with the same address, or open the
   address in a browser and save the page as `relay_rtt.py`.
3. **A list of your relay servers**, in a text file called `nodes.txt` (below).

A "place" is anywhere your RustDesk clients run: your home computer, a school
computer, an office. If you cannot install Python on a locked-down machine, any
computer on the **same network** will do, because what is measured is the network.

## Step 1: write `nodes.txt`

One line per relay server: a name you choose, its address, and a port.

```
# name   address               port
hk       hk.example.com        21117
sh       sh.example.com        21117
fra      fra.example.com       21117
```

- **The address** is what clients use to reach that server.
- **The port** is the one clients reach, so if your `hbbr` is published on a
  different port on the host, write that one.
- **You do not need `hbbr` running on a server yet.** The test only opens a
  connection to a port and closes it, and the delay to a machine is the same
  whatever it is listening on. For a server you are only *considering*, any open
  port will do, such as 22 (SSH). I expect this to hold but have not checked every
  provider; if a firewall treats ports differently the number could be off, so
  re-check with the real `hbbr` port once it is running.
- **If you have only one relay today,** list it, and list any server you are
  thinking of adding. Measuring a single relay still tells you how good it is from
  each place.

## Step 2: at each place, take a quick look

1. **Turn off any VPN or proxy on that computer.** This matters more than
   anything else. Some of them answer on the computer itself, so every server looks
   half a millisecond away and the numbers mean nothing. The tool warns you when it
   sees this.
2. Put `relay_rtt.py` and `nodes.txt` in one folder, open a terminal there, and run
   (use any short name for the place, such as `home` or `school`):

   ```
   python3 relay_rtt.py probe --nodes nodes.txt --site home
   ```

   It takes about 10 seconds and prints something like this (example numbers):

   ```
   node          ip                 loss%      min   median      p90   jitter
   hk            203.0.113.10         0.0     38.1     39.4     44.0      1.9
   sh            198.51.100.20        0.0     95.0     96.2    101.3      2.4
   fra           192.0.2.30           5.0    210.5    214.0    230.8      6.1
   ```

   **How to read it:** *median* is the typical delay in milliseconds, which is what
   you care about. Lower is better. *loss%* is how many attempts got no answer: more
   than a few percent is a problem. A server that shows `-` everywhere and `100.0`
   loss was not reachable at all.
3. **While you are at that place, write down its public address.** Open
   https://ifconfig.me in a browser, or run `curl https://ifconfig.me`, and note
   the number (for example `203.0.113.57`). You will need it in step 5, and you can
   only get it while you are there.

## Step 3: leave it running

One quick look is not enough: the evening is when long-distance links are slowest,
and the ranking can change. So let it run for three days, once every half hour:

```
python3 relay_rtt.py probe --nodes nodes.txt --site home --repeat-every 30 --repeat-for 72
```

It prints one line per run and saves each run as its own file in a folder called
`results`:

```
Measuring 3 node(s) from 'home' every 30 min until Wed 07 Oct 14:02 (about 72 h).
Leave this window open and the computer awake. Ctrl-C stops it; every finished run is already saved.

Sun 14:02  run 1    hk 39 ms  sh 96 ms  fra 214 ms (5% lost)
Sun 14:32  run 2    hk 38 ms  sh 97 ms  fra 211 ms
```

- **Keep the computer awake and the window open.** On a Mac, start it as
  `caffeinate -i python3 relay_rtt.py ...`. On Windows, set Power options to never
  sleep while plugged in. A laptop with its lid closed will sleep.
- **It is fine to stop and start.** Press Ctrl-C any time. Every finished run is
  already saved, and only the run in progress is lost. If you want less than three
  days, use `--repeat-for 24` for one day.
- **Do the same at every place,** using a different `--site` name each time.

## Step 4: bring the results together

Copy the `results` folders from every place into one folder on one computer. The
files are small, and they hold server addresses and delays, **not your own public
address**.

## Step 5: write `sites.txt`

Now tell the tool which public address each place uses, from the numbers you wrote
down. Make a file called `sites.txt`:

```
# name     public network
home       203.0.113.0/24
school     198.51.100.0/24
```

To turn an address into a network, replace the last number with `0` and add `/24`:
`203.0.113.57` becomes `203.0.113.0/24`. (A home connection can change address
now and then, so it is worth checking again in a few months.)

## Step 6: read the results

```
python3 relay_rtt.py report results/*.csv --pair home school --periods
```

(Use your own place names after `--pair`. With only one place, leave `--pair`
out.) It prints a table of every server from every place, split into evening and
the rest of the day, and finishes by saying whether the ranking changes.

**The question it answers: is a second relay worth it?** Look at the totals it
prints for the pair. If the best server is **within about 20%** of the second best,
for every pair of places that matters, you gain very little: leave things as they
are. If one server is clearly better, or the ranking flips in the evening, go on.

## Step 7: make the routing table

```
python3 relay_rtt.py routes results/*.csv --sites sites.txt --out relay_routes.txt
```

This writes the file the server reads. It ranks each server by its **worse** figure
of evening and daytime, adds a penalty for lost packets, and **leaves out any
measurement taken through a proxy**. Read the "Notes" it prints at the end: they
list anything it could not trust.

Then follow steps 6 to 8 of [relay-routing.md](relay-routing.md) to give the file to
the server and check it.

## If something looks wrong

| You see | It means |
|---|---|
| `WARNING: ... fake-IP` | A VPN or proxy is still on. Turn it off and run again. The numbers from that run are worthless. |
| Every delay under about 2 ms | The same problem: something on your computer is answering for the servers. |
| `100.0` loss and `-` for one server | That server is not reachable on that port. Check the port, and the firewall or security group of the server. |
| `DNS failure` | The address in `nodes.txt` is misspelled or does not exist. |
| `python3: command not found` on Windows | Use `py` instead of `python3`. |
| `no files match 'results/*.csv'` | You are not in the folder that contains `results`. |
| One run missing in the list | The computer was asleep. A few gaps do no harm. |
