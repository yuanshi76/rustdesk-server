#!/bin/sh
# Scans Cargo.lock for known security advisories (RustSec, via cargo-audit) and
# reports any that are not in .github/audit-known.txt, the list already reviewed.
#
# Keeps ONE issue open while there are unreviewed advisories, rewrites it when the
# list changes, and closes it when there are none. Prints the full result too, so a
# CI run can put it in its job summary. It never changes code.
#
# Environment:
#   GH_TOKEN    token for `gh` (issues: write); not needed with DRY_RUN=1
#   THIS_REPO   default $GITHUB_REPOSITORY
#   SUMMARY     a file to append a Markdown summary to (e.g. $GITHUB_STEP_SUMMARY)
#   NO_ISSUE=1  report only, never touch issues (used by CI on every push)
#   DRY_RUN=1   print what would be done, change nothing
set -eu
cd "$(git rev-parse --show-toplevel)"
THIS="${THIS_REPO:-${GITHUB_REPOSITORY:-}}"
TITLE="New security advisories in this project's dependencies"
REPORT=$(mktemp); BODY=$(mktemp); trap 'rm -f "$REPORT" "$BODY"' EXIT

# cargo audit exits non-zero when it finds anything; the JSON is what matters.
cargo audit --json > "$REPORT" 2>/dev/null || true
python3 - "$REPORT" .github/audit-known.txt "$BODY" <<'PY'
import json, sys
report, known_file, body_file = sys.argv[1:4]
d = json.load(open(report))
known = set()
for line in open(known_file):
    line = line.split("#", 1)[0].split()
    if len(line) >= 3:
        known.add((line[0], line[1], line[2]))
vulns = d.get("vulnerabilities", {}).get("list", [])
rows, new = {}, []
for x in vulns:
    a, p = x["advisory"], x["package"]
    key = (a["id"], p["name"], p["version"])
    if key in rows:
        continue
    fixed = ", ".join(x.get("versions", {}).get("patched") or []) or "no fix"
    rows[key] = {"id": a["id"], "pkg": p["name"],
                 "title": a.get("title", ""), "fixed": fixed, "url": a.get("url") or f"https://rustsec.org/advisories/{a['id']}"}
    if key not in known:
        new.append(key)
gone = sorted(k for k in known if k not in rows)
with open(body_file, "w") as f:
    if new:
        f.write(f"`cargo audit` reports **{len(new)}** advisory(ies) not yet reviewed:\n\n")
        f.write("| Advisory | Library | Version | Problem | Fixed in |\n|---|---|---|---|---|\n")
        for k in sorted(new):
            r = rows[k]
            f.write(f"| [{r['id']}](https://rustsec.org/advisories/{r['id']}) | {r['pkg']} | {k[2]} | {r['title']} | {r['fixed']} |\n")
        f.write("\nFor each: update within its version line if a fix exists there "
                "(`cargo update -p NAME --precise VERSION`, then check the lockfile changed only "
                "as intended and build with `--locked`), or add it to `.github/audit-known.txt` "
                "with the reason it has to stay.\n\n")
    else:
        f.write("No unreviewed advisories.\n\n")
    f.write(f"{len(rows)} advisory/library/version entries in total, {len(rows) - len(new)} already reviewed.\n")
    if gone:
        f.write("\nNo longer reported (remove from `.github/audit-known.txt`): "
                + ", ".join(f"{i} {p} {v}" for i, p, v in gone) + "\n")
PY
NEW=$(python3 -c "import sys; print(open('$BODY').read().count('rustsec.org/advisories/'))")
cat "$BODY"
if [ -n "${SUMMARY:-}" ]; then { echo "### Dependency audit"; echo; cat "$BODY"; } >> "$SUMMARY"; fi
[ "${NO_ISSUE:-}" = 1 ] && exit 0

act() { if [ "${DRY_RUN:-}" = 1 ]; then echo "[dry run] $*"; else "$@"; fi; }
EXISTING=$(gh issue list --repo "$THIS" --state open --search "\"$TITLE\" in:title" \
    --json number,title --jq "map(select(.title == \"$TITLE\")) | .[0].number // empty")
if [ "$NEW" -eq 0 ]; then
    [ -n "$EXISTING" ] && act gh issue close "$EXISTING" --repo "$THIS" --comment "Nothing unreviewed any more."
    exit 0
fi
if [ -z "$EXISTING" ]; then
    act gh issue create --repo "$THIS" --title "$TITLE" --body-file "$BODY"
else
    OLD=$(gh issue view "$EXISTING" --repo "$THIS" --json body --jq .body)
    if [ "$OLD" != "$(cat "$BODY")" ]; then
        act gh issue edit "$EXISTING" --repo "$THIS" --body-file "$BODY"
        act gh issue comment "$EXISTING" --repo "$THIS" --body "The list changed; updated above."
    fi
fi
