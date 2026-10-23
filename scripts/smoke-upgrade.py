"""Exercise old/new host binaries with disposable accounts and SQLite backups only.

No production state/configuration input is accepted. All state is freshly created
beneath a NEW output directory. Network listeners and model endpoint are loopback.
"""
import argparse
import hashlib
import http.cookiejar
import json
import os
from pathlib import Path
import secrets
import socket
import sqlite3
import subprocess
import time
import urllib.error
import urllib.request


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def port():
    with socket.socket() as connection:
        connection.bind(("127.0.0.1", 0))
        return connection.getsockname()[1]


def backup(source, destination):
    require(not destination.exists(), "Backup destination already exists")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(source.as_uri() + "?mode=ro", uri=True) as reader:
        with sqlite3.connect(destination) as writer:
            reader.backup(writer)
            require(writer.execute("PRAGMA integrity_check").fetchone()[0] == "ok", "Invalid backup")


def inspect(path):
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
        require(db.execute("PRAGMA integrity_check").fetchone()[0] == "ok", "Invalid database")
        require(not db.execute("PRAGMA foreign_key_check").fetchall(), "Broken foreign keys")
        tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        # Dynamic login/rate records intentionally change during authentication.
        preserved = {}
        for name in ("principals", "account_roles", "account_invites", "native_grants",
                     "identity_devices", "channel_identities", "cloud_sessions", "cloud_turns",
                     "web_messages", "device_connections", "user_models"):
            rows = db.execute(f'SELECT * FROM "{name}"').fetchall()
            preserved[name] = sorted(rows, key=repr)
        return {
            "schema": db.execute("PRAGMA user_version").fetchone()[0],
            "application_id": db.execute("PRAGMA application_id").fetchone()[0],
            "tables": tables,
            "preserved": preserved,
        }


class Host:
    def __init__(self, binary, folder, state):
        self.binary, self.folder, self.state = binary, folder, state
        self.process = None
        self.origin = f"http://127.0.0.1:{port()}"
        self.cookies = http.cookiejar.CookieJar()
        self.client = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(self.cookies))

    def __enter__(self):
        self.folder.mkdir()
        config = {
            "state_dir": str(self.state), "listen": self.origin.removeprefix("http://"),
            "public_origin": self.origin, "bootstrap_env": "PERI_UPGRADE_FIXTURE_BOOTSTRAP",
            "model": {"api_base": "http://127.0.0.1:1/v1",
                      "api_key_env": "PERI_UPGRADE_FIXTURE_MODEL", "model": "fixture-no-inference"},
            "devices": [], "qq": None,
        }
        config_path = self.folder / "host.json"
        config_path.write_text(json.dumps(config), encoding="utf-8")
        env = os.environ.copy()
        env.update(PERI_UPGRADE_FIXTURE_BOOTSTRAP=BOOTSTRAP,
                   PERI_UPGRADE_FIXTURE_MODEL="fixture-no-real-key", RUST_LOG="warn")
        self.log = (self.folder / "host.log").open("wb")
        self.process = subprocess.Popen([str(self.binary), "--config", str(config_path)],
                                        cwd=self.folder, env=env, stdout=self.log, stderr=self.log)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                require(self.process.poll() is None, "Host exited during startup; inspect local fixture log")
                try:
                    self.get("/healthz")
                    return self
                except (OSError, urllib.error.URLError):
                    time.sleep(0.1)
            raise RuntimeError("Host startup deadline")
        except BaseException:
            self.stop()
            raise

    def request(self, route, body=None):
        headers = {"Origin": self.origin}
        payload = None
        if body is not None:
            payload = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
            headers["X-Peri-CSRF"] = next((c.value for c in self.cookies if c.name == "peri_csrf"), "")
        with self.client.open(urllib.request.Request(self.origin + route, data=payload, headers=headers),
                              timeout=10) as response:
            raw = response.read()
            return json.loads(raw) if raw and "json" in response.headers.get("Content-Type", "") else raw

    def get(self, route):
        return self.request(route)

    def post(self, route, body):
        return self.request(route, body)

    def login(self):
        self.post("/api/login", {"login": "preview-owner", "password": PASSWORD})
        require(self.get("/api/account")["principal"]["display_name"] == "Preview Owner", "Account not preserved")

    def shutdown(self):
        self.post("/api/service/shutdown", {})
        require(self.process.wait(timeout=20) == 0, "Unclean host shutdown")

    def stop(self):
        try:
            if self.process is not None and self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=10)
        finally:
            self.log.close()

    def __exit__(self, *_):
        self.stop()


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--baseline", type=Path, required=True, help="Verified old host binary")
parser.add_argument("--candidate", type=Path, required=True, help="Candidate host binary")
parser.add_argument("--output", type=Path, required=True, help="New disposable fixture directory")
parser.add_argument("--baseline-schema", type=int, default=9)
parser.add_argument("--candidate-schema", type=int, default=25)
args = parser.parse_args()
baseline, candidate, output = args.baseline.resolve(), args.candidate.resolve(), args.output.resolve()
require(baseline.is_file() and candidate.is_file(), "Host binary missing")
output.mkdir(parents=True, exist_ok=False)
BOOTSTRAP, PASSWORD = secrets.token_urlsafe(40), secrets.token_urlsafe(24)
LEGACY_MEMORY = "Synthetic legacy memory: prefers afternoon check-ins."
old_state = output / "baseline-state"
with Host(baseline, output / "baseline-start", old_state) as host:
    host.post("/api/setup", {"bootstrap_token": BOOTSTRAP, "login": "preview-owner",
                             "display_name": "Preview Owner", "password": PASSWORD})
    host.login()
    invite = host.post("/api/admin/invites", {})
    host.post("/api/register", {"invite_code": invite["code"], "login": "preview-member",
                                "display_name": "Preview Member", "password": PASSWORD})
    preferences = host.get("/api/user-data")["data"]
    preferences["instructions"] = "Synthetic preference: concise replies."
    preferences["memory"] = LEGACY_MEMORY
    # Schema 9's predecessor preference type includes the separate memory field.
    saved = host.post("/api/user-data", preferences)
    expected_preferences = {k: saved[k] for k in ("revision", "instructions")}
    host.shutdown()

before = inspect(old_state / "cloud.sqlite")
require(before["schema"] == args.baseline_schema, "Unexpected baseline schema")
require(len(before["preserved"]["principals"]) == 2, "Missing fixture accounts")
snapshot = output / "backup" / "cloud.sqlite"
backup(old_state / "cloud.sqlite", snapshot)
candidate_state = output / "candidate-state"
backup(snapshot, candidate_state / "cloud.sqlite")
with Host(candidate, output / "candidate-start", candidate_state) as host:
    host.login()
    require(host.get("/api/preferences")["data"] == expected_preferences, "Preferences lost in migration")
    after = inspect(candidate_state / "cloud.sqlite")
    require(after["schema"] == args.candidate_schema, "Candidate migration not complete")
    require(after["application_id"] == before["application_id"], "Database lineage changed")
    require(after["preserved"] == before["preserved"], "Legacy account/authorization/history rows changed")
    require("agent_task_approval_presentations" in after["tables"], "Approval migration missing")
    require("cloud_archive_entries" in after["tables"], "Context archive migration missing")
    with sqlite3.connect((candidate_state / "cloud.sqlite").as_uri() + "?mode=ro", uri=True) as db:
        memory = db.execute("SELECT content,source_json FROM memory_items WHERE deleted=0").fetchall()
        require(len(memory) == 1 and memory[0][0] == LEGACY_MEMORY
                and json.loads(memory[0][1])["kind"] == "legacy_import", "Legacy memory import lost provenance")
    host.shutdown()

with Host(candidate, output / "candidate-restart", candidate_state) as host:
    host.login()
    require(inspect(candidate_state / "cloud.sqlite")["preserved"] == before["preserved"], "Restart changed legacy rows")
    host.shutdown()

restored = output / "restored-state"
backup(snapshot, restored / "cloud.sqlite")
with Host(baseline, output / "baseline-restored", restored) as host:
    host.login()
    data = host.get("/api/user-data")["data"]
    require(all(data[k] == value for k, value in expected_preferences.items()), "Restored preferences differ")
    require(data["memory"] == LEGACY_MEMORY, "Restored legacy memory differs")
    host.shutdown()
require(inspect(restored / "cloud.sqlite")["schema"] == args.baseline_schema, "Backup did not restore old schema")
report = {
    "result": "pass", "baseline_schema": before["schema"], "candidate_schema": after["schema"],
    "fixture_accounts": 2, "backup_integrity": "ok", "candidate_restart": "pass",
    "old_binary_backup_restore": "pass", "model_endpoint": "unreachable loopback fixture",
    "legacy_memory_import": "pass",
    "production_state_used": False,
    "nonempty_legacy_tables": [name for name, rows in before["preserved"].items() if rows],
    "limitation": "Synthetic account/preferences fixture; device grants/history tables are empty. Task recovery is tested separately.",
    "baseline_sha256": hashlib.sha256(baseline.read_bytes()).hexdigest(),
    "candidate_sha256": hashlib.sha256(candidate.read_bytes()).hexdigest(),
}
(output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
print(json.dumps(report))
