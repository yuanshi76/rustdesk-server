#!/bin/sh
# Back up the server's data folders into one dated archive, keep the newest N,
# and optionally copy each new archive to another machine.
#
# What it saves: everything in the folders you name, which for this project is the
# server's identity (id_ed25519, id_ed25519.pub), its peer table (db_v2.sqlite3), the
# API server's database, and small settings files (relay_locations.txt,
# relay_preferred, relay_routes.txt). SQLite databases are copied with SQLite's own
# online backup, so a copy taken while the server is writing is still consistent.
# GeoIP databases (*.mmdb) are skipped: they are large and can be downloaded again.
#
# The archive holds the server's PRIVATE key. It is written readable by its owner
# only; keep it, and any copy, somewhere only you can read.
#
# Settings (environment):
#   DATA_DIRS   folders to save, space separated   (default: "./data ./api_data")
#   BACKUP_DIR  where archives go                   (default: ./backups)
#   KEEP        how many archives to keep here      (default: 14)
#   REMOTE      optional scp destination for each new archive, e.g.
#               backup@relay-b.example.com:rustdesk-backups/
#   EXCLUDE     shell patterns to skip, space separated (default: "*.mmdb *.mmdb.* .*.loaded *.tmp")
#
# Run it from cron or a systemd timer; see docs/backup.md. Exit status is non-zero
# on any failure, and nothing is pruned unless the new archive was verified.
set -eu
umask 077

DATA_DIRS="${DATA_DIRS:-./data ./api_data}"
BACKUP_DIR="${BACKUP_DIR:-./backups}"
KEEP="${KEEP:-14}"
REMOTE="${REMOTE:-}"
EXCLUDE="${EXCLUDE:-*.mmdb *.mmdb.* .*.loaded *.tmp}"

log() { echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] rustdesk-backup: $*"; }
fail() { log "FAILED: $*"; exit 1; }

case "$KEEP" in ''|*[!0-9]*) fail "KEEP must be a number, got '$KEEP'";; esac
[ "$KEEP" -ge 1 ] || fail "KEEP must be at least 1"
command -v python3 >/dev/null 2>&1 || fail "python3 is needed to copy SQLite databases safely"

mkdir -p "$BACKUP_DIR"
STAMP=$(date -u +%Y%m%d-%H%M%S)
NAME="rustdesk-$STAMP"
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/rustdesk-backup.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT INT TERM

skip() {  # is this file name excluded?
    for pat in $EXCLUDE; do
        # shellcheck disable=SC2254
        case "$1" in $pat) return 0;; esac
    done
    return 1
}

found=0
for dir in $DATA_DIRS; do
    if [ ! -d "$dir" ]; then
        log "skipping $dir: no such folder"
        continue
    fi
    found=1
    label=$(basename "$dir")
    mkdir -p "$STAGE/$NAME/$label"
    # Walk the folder; keep its layout.
    (cd "$dir" && find . -type f) | while IFS= read -r rel; do
        rel=${rel#./}
        base=$(basename "$rel")
        if skip "$base"; then continue; fi
        case "$base" in *-wal|*-shm|*-journal) continue;; esac  # part of a live database
        src="$dir/$rel"; dst="$STAGE/$NAME/$label/$rel"
        mkdir -p "$(dirname "$dst")"
        if [ "$(head -c 15 "$src" 2>/dev/null)" = "SQLite format 3" ]; then
            python3 - "$src" "$dst" <<'PY' || exit 1
import sqlite3, sys
src, dst = sys.argv[1], sys.argv[2]
s = sqlite3.connect(f"file:{src}?mode=ro", uri=True, timeout=30)
d = sqlite3.connect(dst)
with d:
    s.backup(d)
# The copy inherits the original's write-ahead-log mode; make it one standalone file.
d.execute("PRAGMA journal_mode=DELETE")
ok = d.execute("PRAGMA integrity_check").fetchone()[0]
s.close(); d.close()
if ok != "ok":
    sys.exit(f"integrity check of the copy of {src} failed: {ok}")
PY
            # Left over by SQLite next to the copy; the copy itself is complete.
            rm -f "$dst-wal" "$dst-shm"
        else
            cp -p "$src" "$dst"
        fi
    done || fail "copying $dir"
done
[ "$found" = 1 ] || fail "none of the folders exist: $DATA_DIRS"

ARCHIVE="$BACKUP_DIR/$NAME.tar.gz"
# COPYFILE_DISABLE: macOS tar would otherwise add hidden ._ metadata files.
COPYFILE_DISABLE=1 tar -C "$STAGE" -czf "$ARCHIVE.part" "$NAME" || fail "writing $ARCHIVE"
tar -tzf "$ARCHIVE.part" >/dev/null || fail "the new archive does not read back"
mv "$ARCHIVE.part" "$ARCHIVE"
chmod 600 "$ARCHIVE"
log "wrote $ARCHIVE ($(wc -c < "$ARCHIVE" | tr -d ' ') bytes)"

if [ -n "$REMOTE" ]; then
    scp -q -o BatchMode=yes "$ARCHIVE" "$REMOTE" || fail "copying to $REMOTE (the local archive is kept)"
    log "copied to $REMOTE"
fi

# Keep the newest KEEP archives here. Only after the new one is safely written.
# shellcheck disable=SC2012
ls -1t "$BACKUP_DIR"/rustdesk-*.tar.gz 2>/dev/null | tail -n +"$((KEEP + 1))" | while IFS= read -r old; do
    rm -f "$old" && log "removed old $old"
done
log "done"
