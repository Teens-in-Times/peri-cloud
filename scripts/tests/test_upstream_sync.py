"""Local Git fixtures exercise source/credential boundaries; no network, models or production data."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import upstream_sync as sync
import publish_upstream_sync as publisher

SOURCE = "peri-acp-types/src/fixture.rs"


def commit(root, message="fixture"):
    sync.git(root, "add", "-A")
    sync.git(root, "commit", "-qm", message)
    return sync.git(root, "rev-parse", "HEAD").stdout.decode().strip()


def put(root, name, text):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def init(root):
    root.mkdir()
    sync.git(root, "init", "-q")
    sync.git(root, "config", "user.name", "Fixture")
    sync.git(root, "config", "user.email", "fixture@example.invalid")


class OverlayTests(unittest.TestCase):
    def setUp(self):
        scratch = SCRIPTS.parent / "target"
        scratch.mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="upstream-fixture-", dir=scratch)
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.up = self.home / "upstream"
        self.root = self.home / "deployment"
        self.out = self.home / "candidate"
        init(self.up)
        self.lines = [f"// line {i}\n" for i in range(24)]
        put(self.up, SOURCE, "".join(self.lines))
        self.base = commit(self.up)
        init(self.root)
        ours = self.lines.copy()
        ours[0] = "// deployment customization\n"
        put(self.root, SOURCE, "".join(ours))
        manifest = f"docs/upstream/peri-{self.base[:8]}.manifest"
        pin = {"repository": sync.UPSTREAM, "revision": self.base, "manifest": manifest}
        put(self.root, sync.PIN_PATH, json.dumps(pin))
        digest = sync.tree(self.up, self.base)[SOURCE][1]
        put(self.root, manifest, f"# upstream {self.base}\n{digest}\t{SOURCE}\n")
        put(self.root, "docs/upstream/production-patches.txt", SOURCE + "\n")
        put(self.root, "README.md", f"Source `{self.base}` [record]({manifest.removesuffix('.manifest')}.md)\n")
        put(self.root, "AGENTS.md", f"Upstream source: {sync.UPSTREAM}, revision {self.base}.\n")
        put(self.root, "NOTICE", f"Base revision: {self.base}\n")
        put(self.root, ".gitignore", "/target/\n*.log\n")
        self.ours = commit(self.root)

    def incoming(self, conflict=False):
        lines = self.lines.copy()
        lines[0 if conflict else -1] = "// incoming change\n"
        put(self.up, SOURCE, "".join(lines))
        return commit(self.up)

    def clone(self):
        child = self.home / "publisher"
        subprocess.run(["git", "clone", "-q", "--no-hardlinks", str(self.root), str(child)], check=True)
        return child

    def test_three_way_preserves_patch_and_publisher_applies_exact_artifact_once(self):
        result = sync.prepare(self.root, self.up, self.incoming(), self.out)
        self.assertEqual(result["state"], "prepared")
        text = (self.root / SOURCE).read_text()
        self.assertIn("deployment customization", text)
        self.assertIn("incoming change", text)
        child = self.clone()
        sync.apply(child, self.out)
        self.assertEqual((child / SOURCE).read_text(), text)
        self.assertEqual(sync.read_pin(child)["revision"], result["upstream_head"])
        with self.assertRaises(ValueError):
            sync.apply(child, self.out)

    def test_text_and_delete_conflicts_leave_all_source_untouched(self):
        before = (self.root / SOURCE).read_bytes()
        result = sync.prepare(self.root, self.up, self.incoming(True), self.out)
        self.assertEqual(result["state"], "conflict")
        self.assertEqual((self.root / SOURCE).read_bytes(), before)
        self.assertFalse(sync.git(self.root, "status", "--porcelain").stdout)
        self.assertFalse((self.out / "candidate.patch").exists())
        (self.up / SOURCE).unlink()
        head = commit(self.up)
        deleted = sync.prepare(self.root, self.up, head, self.home / "deleted")
        self.assertEqual(deleted["state"], "conflict")
        self.assertEqual((self.root / SOURCE).read_bytes(), before)

    def test_new_ignored_file_and_symlink_cannot_be_adopted(self):
        private = "peri-acp-types/src/private.log"
        put(self.root, private, "fixture-private-data")
        put(self.up, private, "incoming")
        result = sync.prepare(self.root, self.up, commit(self.up), self.out)
        self.assertEqual(result["state"], "conflict")
        self.assertEqual((self.root / private).read_text(), "fixture-private-data")
        digest = subprocess.run(["git", "-C", str(self.up), "hash-object", "-w", "--stdin"],
                                input=b"../../outside", capture_output=True, check=True).stdout.decode().strip()
        sync.git(self.up, "update-index", "--add", "--cacheinfo", f"120000,{digest},peri-acp-types/src/link")
        sync.git(self.up, "commit", "-qm", "fixture symlink")
        head = sync.git(self.up, "rev-parse", "HEAD").stdout.decode().strip()
        with self.assertRaises(ValueError):
            sync.prepare(self.root, self.up, head, self.home / "symlink")

    def test_unsafe_paths_long_source_and_manifest_drift_are_rejected(self):
        for name in ("../outside", "/outside", "peri-acp-types/../outside", "peri-acp-types/.GIT/config",
                     "peri-acp-types\\outside", "peri-acp-types/CON.rs", "peri-acp-types/a:b"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                sync.safe_path(name)
        put(self.up, "peri-acp-types/src/long.rs", "// long\n" * 1000)
        head = commit(self.up)
        self.assertEqual(sync.prepare(self.root, self.up, head, self.out)["state"], "conflict")
        pin = sync.read_pin(self.root)
        put(self.root, pin["manifest"], "# altered fixture\n")
        commit(self.root)
        with self.assertRaises(ValueError):
            sync.prepare(self.root, self.up, head, self.home / "drift")
        put(self.up, "peri-acp-types/third_party/NOTICE.md", "fixture changed attribution")
        license_head = commit(self.up)
        result = sync.prepare(self.clone(), self.up, license_head, self.home / "license")
        self.assertEqual(result["state"], "blocked")

    def test_patch_hash_and_protected_paths_are_verified_before_write(self):
        sync.prepare(self.root, self.up, self.incoming(), self.out)
        child = self.clone()
        original = (self.out / "candidate.patch").read_bytes()
        (self.out / "candidate.patch").write_bytes(original + b"tampered")
        with self.assertRaises(ValueError):
            sync.apply(child, self.out)
        self.assertFalse(sync.git(child, "status", "--porcelain").stdout)
        (self.out / "candidate.patch").write_bytes(original)
        report = sync.load_report(self.out)
        report["files"].append(".github/workflows/ci.yml")
        (self.out / "report.json").write_text(json.dumps(report), "utf-8")
        with self.assertRaises(ValueError):
            sync.apply(child, self.out)
        self.assertFalse(sync.git(child, "status", "--porcelain").stdout)

    def test_unchanged_or_an_existing_pending_review_has_no_remote_writes(self):
        sync.prepare(self.root, self.up, self.base, self.out)
        class NoneAllowed:
            def api(self, *args, **kwargs):
                raise AssertionError("no API needed for unchanged source")
        self.assertEqual(publisher.publish(self.root, self.out, NoneAllowed()), "unchanged")
        other = self.home / "other"
        sync.prepare(self.root, self.up, self.incoming(), other)
        child = self.clone()
        class Pending:
            def api(self, path, data=None, **kwargs):
                self_test.assertEqual(path, "pulls?state=open&per_page=100&page=1")
                self_test.assertIsNone(data)
                return [{"head": {"ref": publisher.PREFIX + "earlier"}, "body": publisher.MARKER}]
        self_test = self
        self.assertEqual(publisher.publish(child, other, Pending()), "pending")
        self.assertFalse(sync.git(child, "status", "--porcelain").stdout)

    def test_new_publication_pushes_only_candidate_and_opens_draft_then_dispatches(self):
        report = sync.prepare(self.root, self.up, self.incoming(), self.out)
        child = self.clone()
        calls, pushes = [], []
        class Remote:
            repository = "fixture/project"
            def api(self, path, data=None, missing=False):
                calls.append((path, data))
                if path.startswith("pulls?"): return []
                if path == "git/ref/heads/main": return {"object": {"sha": report["base_commit"]}}
                if path.startswith("git/ref/heads/"): return None
                if path == "pulls": return {"html_url": "https://example.invalid/pr/1"}
                if path.startswith("actions/workflows/ci.yml/runs?"): return {"workflow_runs": []}
                if path == "actions/workflows/ci.yml/dispatches": return None
                raise AssertionError(path)
        original_run = subprocess.run
        def isolated_run(args, **kwargs):
            self.assertEqual(args[0], "git", "publisher cannot run candidate programs")
            if "push" in args:
                pushes.append(args)
                return subprocess.CompletedProcess(args, 0, b"", b"")
            return original_run(args, **kwargs)
        with patch.object(subprocess, "run", isolated_run):
            self.assertEqual(publisher.publish(child, self.out, Remote()), "created")
        self.assertEqual(len(pushes), 1)
        self.assertEqual(pushes[0][-1], "HEAD:refs/heads/" + report["branch"])
        self.assertNotIn("--force", pushes[0])
        writes = [(name, data) for name, data in calls if data is not None]
        self.assertEqual([name for name, _ in writes], ["pulls", "actions/workflows/ci.yml/dispatches"])
        self.assertTrue(writes[0][1]["draft"])
        self.assertEqual(writes[0][1]["base"], "main")

    def test_reviewed_branch_tampering_blocks_dispatch_and_preserves_remote(self):
        report = sync.prepare(self.root, self.up, self.incoming(), self.out)
        child = self.clone()
        head = "c" * 40
        marker = f"{publisher.MARKER}{report['upstream_base']}:{report['upstream_head']} -->"
        class Changed:
            repository = "fixture/project"
            def api(self, path, data=None, **kwargs):
                if data is not None:
                    raise AssertionError("cannot write or dispatch a modified candidate")
                if path.startswith("pulls?state=open"):
                    return [{"head": {"ref": report["branch"], "sha": head}, "body": marker}]
                if path.startswith("pulls?state=closed"): return []
                if path == "git/ref/heads/main": return {"object": {"sha": report["base_commit"]}}
                if path.startswith("git/ref/heads/"): return {"object": {"sha": head}}
                if path.startswith("git/commits/"): return {"tree": {"sha": "d" * 40}}
                raise AssertionError(path)
        with self.assertRaisesRegex(ValueError, "different contents"):
            publisher.publish(child, self.out, Changed())


if __name__ == "__main__":
    unittest.main()
