# Backing up the server

What makes your server *your* server is a few files: the key pair clients trust
(`id_ed25519`, `id_ed25519.pub`), the peer table (`db_v2.sqlite3`), and the API server's
database. Lose the key pair and every client has to be given a new Key. Back them up.

`tools/backup/rustdesk-backup.sh` does it in one command:

- saves everything in the folders you name, keeping their layout;
- copies SQLite databases with SQLite's own online backup, so a copy taken while the
  server is running is consistent, and checks each copy's integrity;
- skips the GeoIP database (`*.mmdb`, about 130 MB, downloaded again by the updater);
- writes one dated archive, `rustdesk-YYYYMMDD-HHMMSS.tar.gz`, **readable by its owner
  only**, and checks it reads back;
- keeps the newest 14 (`KEEP`) and deletes older ones, only after the new one is written;
- optionally copies each new archive to another machine (`REMOTE`).

It needs `python3` on the host (for the SQLite copy). It exits non-zero on any failure, so
cron mail or `systemctl status` shows it.

**The archive contains your server's private key.** Keep it, and every copy, where only
you can read it.

## Set it up

On the main server, in the folder that holds your compose file and the `data` and
`api_data` folders:

```bash
mkdir -p tools && curl -fsSLo tools/rustdesk-backup.sh \
  https://raw.githubusercontent.com/yuanshi76/rustdesk-server/main/tools/backup/rustdesk-backup.sh
chmod 700 tools/rustdesk-backup.sh
DATA_DIRS="./data ./api_data" BACKUP_DIR=./backups tools/rustdesk-backup.sh
ls -l backups/
```

Run it nightly from cron (`crontab -e` as the user that owns the folders; replace the path):

```
30 3 * * * cd /root/docker/rustdesk && DATA_DIRS="./data ./api_data" BACKUP_DIR=./backups tools/rustdesk-backup.sh >> backups/backup.log 2>&1
```

## Keep a copy on another machine

A backup on the same disk does not survive the disk. To copy each archive to another
machine over SSH (your relay server, for example):

1. On the main server, as the user cron runs as, make a key without a passphrase and
   allow it on the other machine:
   ```bash
   ssh-keygen -t ed25519 -f ~/.ssh/rustdesk_backup -N ""
   ssh-copy-id -i ~/.ssh/rustdesk_backup.pub backup@relay-b.example.com
   ssh backup@relay-b.example.com 'mkdir -p rustdesk-backups && chmod 700 rustdesk-backups'
   ```
2. Tell SSH to use that key for that host, in `~/.ssh/config`:
   ```
   Host relay-b.example.com
       User backup
       IdentityFile ~/.ssh/rustdesk_backup
   ```
3. Add `REMOTE=relay-b.example.com:rustdesk-backups/` to the cron line.

The remote copies are not pruned; delete old ones there now and then.

## Restore

```bash
docker compose down
mkdir -p restore && tar -xzf backups/rustdesk-YYYYMMDD-HHMMSS.tar.gz -C restore
cp -a restore/rustdesk-*/data/. ./data/
cp -a restore/rustdesk-*/api_data/. ./api_data/
docker compose up -d
```

Since v0.4.1, `hbbs` refuses to start if `id_ed25519` and `id_ed25519.pub` do not belong
together, so restore both from the same archive. **Try a restore once** on a spare machine
or folder: a backup that has never been restored is a hope, not a backup.

## Settings

| Variable | Default | |
|---|---|---|
| `DATA_DIRS` | `./data ./api_data` | folders to save, space separated; missing ones are skipped, none existing is an error |
| `BACKUP_DIR` | `./backups` | where archives go |
| `KEEP` | `14` | how many archives to keep in `BACKUP_DIR` |
| `REMOTE` | *(none)* | `scp` destination for each new archive |
| `EXCLUDE` | `*.mmdb *.mmdb.* .*.loaded *.tmp` | file name patterns to skip |
