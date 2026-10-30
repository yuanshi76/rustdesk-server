#!/bin/sh
# How far this fork's code has drifted from the upstream commit it is based on, and
# a format check that touches only the files the fork owns.
#
#   tools/fork-footprint.sh              print the footprint
#   tools/fork-footprint.sh --fmt-check  also fail if a fork-owned file is not rustfmt-clean
#
# Why: every line the fork changes in an upstream file is a line an upstream change can
# conflict with. Fork features belong in their own files (relay_select.rs, relay_geo.rs,
# ...) with a one-line call from upstream's code, and upstream's own lines are never
# reformatted, which is why the format check skips them. See NOTES.md, "Fork hygiene".
set -eu
cd "$(git rev-parse --show-toplevel)"
. ./UPSTREAM_VERSION
BASE="$UPSTREAM_COMMIT"
if ! git cat-file -e "${BASE}^{commit}" 2>/dev/null; then
    git fetch -q --depth=1 https://github.com/rustdesk/rustdesk-server.git "$BASE"
fi

upstream_files=$(mktemp); trap 'rm -f "$upstream_files"' EXIT
git ls-tree -r --name-only "$BASE" -- src tests > "$upstream_files"

echo "Upstream base: $BASE"
echo
echo "Upstream files the fork changes (lines added + removed):"
git diff --numstat "$BASE" HEAD -- src tests | while read -r add del path; do
    if grep -qxF "$path" "$upstream_files"; then
        printf '  %6d  %s\n' $((add + del)) "$path"
    fi
done | sort -rn
echo
echo "Fork-owned files (not in upstream):"
fork_owned=$(git ls-files -- 'src/*.rs' 'tests/*.rs' 'tests/**/*.rs' | while read -r f; do
    grep -qxF "$f" "$upstream_files" || echo "$f"
done)
for f in $fork_owned; do echo "  $f"; done

if [ "${1:-}" = "--fmt-check" ]; then
    echo
    echo "rustfmt --check on fork-owned files only:"
    # shellcheck disable=SC2086
    rustfmt --edition 2021 --check $fork_owned && echo "  clean"
fi
