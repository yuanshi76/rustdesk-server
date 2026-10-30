"""Tests for rustdesk-backup.sh: real files, a real SQLite database written to while
the backup runs, real tar archives.

    python3 -m unittest discover -s tools/backup -v
"""
import os
import sqlite3
import stat
import subprocess
import tarfile
import tempfile
import threading
import time
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "rustdesk-backup.sh")


def make_db(path, rows=10):
    db = sqlite3.connect(path)
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("CREATE TABLE IF NOT EXISTS peer (id TEXT, pk BLOB)")
    db.executemany("INSERT INTO peer VALUES (?, ?)", [(f"id{i}", b"x" * 100) for i in range(rows)])
    db.commit()
    db.close()


class Backup(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.data = os.path.join(self.root, "data")
        self.api = os.path.join(self.root, "api_data")
        self.out = os.path.join(self.root, "backups")
        os.makedirs(self.data)
        os.makedirs(self.api)
        with open(os.path.join(self.data, "id_ed25519"), "w") as f:
            f.write("secret-key\n")
        with open(os.path.join(self.data, "id_ed25519.pub"), "w") as f:
            f.write("public-key\n")
        with open(os.path.join(self.data, "relay_locations.txt"), "w") as f:
            f.write("relay.example.com:21117 22.3,114.2\n")
        with open(os.path.join(self.data, "geo.mmdb"), "wb") as f:
            f.write(b"\0" * 1024)
        make_db(os.path.join(self.data, "db_v2.sqlite3"))
        make_db(os.path.join(self.api, "rustdesk.db"))

    def tearDown(self):
        self.tmp.cleanup()

    def run_backup(self, **env):
        e = dict(os.environ)
        e.update(DATA_DIRS=f"{self.data} {self.api}", BACKUP_DIR=self.out)
        e.update(env)
        return subprocess.run(["sh", SCRIPT], env=e, capture_output=True, text=True)

    def archives(self):
        if not os.path.isdir(self.out):
            return []
        return sorted(f for f in os.listdir(self.out) if f.endswith(".tar.gz"))

    def extract(self, name):
        dest = os.path.join(self.root, "restore")
        with tarfile.open(os.path.join(self.out, name)) as t:
            t.extractall(dest)
        (top,) = os.listdir(dest)
        return os.path.join(dest, top)

    def test_saves_keys_settings_and_consistent_databases_while_they_are_written(self):
        stop = threading.Event()

        def writer():
            db = sqlite3.connect(os.path.join(self.data, "db_v2.sqlite3"), timeout=30)
            n = 0
            while not stop.is_set():
                db.execute("INSERT INTO peer VALUES (?, ?)", (f"w{n}", b"y" * 500))
                db.commit()
                n += 1
            db.close()

        t = threading.Thread(target=writer)
        t.start()
        try:
            r = self.run_backup()
        finally:
            stop.set()
            t.join()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        (name,) = self.archives()
        mode = stat.S_IMODE(os.stat(os.path.join(self.out, name)).st_mode)
        self.assertEqual(mode, 0o600, "the archive holds the private key and must be owner-only")

        top = self.extract(name)
        for rel in ["data/id_ed25519", "data/id_ed25519.pub", "data/relay_locations.txt",
                    "data/db_v2.sqlite3", "api_data/rustdesk.db"]:
            self.assertTrue(os.path.isfile(os.path.join(top, rel)), rel)
        self.assertFalse(os.path.exists(os.path.join(top, "data/geo.mmdb")), "GeoIP database should be skipped")
        self.assertFalse(any(f.endswith(("-wal", "-shm")) for f in os.listdir(os.path.join(top, "data"))))
        for rel in ["data/db_v2.sqlite3", "api_data/rustdesk.db"]:
            db = sqlite3.connect(os.path.join(top, rel))
            self.assertEqual(db.execute("PRAGMA integrity_check").fetchone()[0], "ok", rel)
            self.assertGreaterEqual(db.execute("SELECT count(*) FROM peer").fetchone()[0], 10, rel)
            db.close()
        with open(os.path.join(top, "data/id_ed25519")) as f:
            self.assertEqual(f.read(), "secret-key\n")

    def test_keeps_only_the_newest_archives(self):
        for _ in range(3):
            r = self.run_backup(KEEP="2")
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
            time.sleep(1.1)  # archive names are per second
        names = self.archives()
        self.assertEqual(len(names), 2, names)

    def test_a_damaged_database_fails_and_removes_nothing(self):
        r = self.run_backup()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        before = self.archives()
        time.sleep(1.1)
        with open(os.path.join(self.data, "db_v2.sqlite3"), "r+b") as f:
            f.seek(100)
            f.write(b"\xff" * 4000)  # header kept, contents wrecked
        for suffix in ("-wal", "-shm"):
            p = os.path.join(self.data, "db_v2.sqlite3" + suffix)
            if os.path.exists(p):
                os.remove(p)
        r = self.run_backup(KEEP="1")
        self.assertNotEqual(r.returncode, 0, "a damaged database must not pass")
        self.assertEqual(self.archives(), before, "a failed run must not prune or add archives")
        self.assertFalse(any(f.endswith(".part") for f in os.listdir(self.out)))

    def test_no_existing_folder_is_an_error(self):
        r = self.run_backup(DATA_DIRS=os.path.join(self.root, "nope"))
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.archives(), [])

    def test_bad_keep_is_refused(self):
        for keep in ("0", "two", ""):
            if keep == "":
                continue  # empty means the default
            r = self.run_backup(KEEP=keep)
            self.assertNotEqual(r.returncode, 0, keep)

    def test_copies_each_new_archive_to_the_remote(self):
        remote = os.path.join(self.root, "remote")
        os.makedirs(remote)
        r = self.run_backup(REMOTE=remote + "/")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(os.listdir(remote), self.archives())


if __name__ == "__main__":
    unittest.main()
