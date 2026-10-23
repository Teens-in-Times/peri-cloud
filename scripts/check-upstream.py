#!/usr/bin/env python3
"""Verify that overlaid Peri sources match the recorded upstream revision.

The manifest stores the Git blob id of every upstream file in the overlay
scope.  Local files are hashed with Git's blob algorithm after normalizing
CRLF for text files, so the check is independent of checkout line endings.

Files listed in the production patch allowlist may differ.  Everything else
must be byte-identical to upstream.
"""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_COMMIT = "e598d17872bce487355e1bfa70f077743984760f"
DEFAULT_MANIFEST = ROOT / "docs/upstream/peri-e598d178.manifest"
DEFAULT_ALLOWLIST = ROOT / "docs/upstream/production-patches.txt"

# Directories replaced or added from upstream.  peri-acp contributes prompts
# only; this distribution does not build that crate.
SCOPE = [
    "peri-acp/prompts",
    "peri-acp-types",
    "peri-agent",
    "peri-middlewares",
    "peri-model",
    "peri-process",
    "peri-resources",
    "peri-js-runtime",
    "peri-workflow",
    "peri-config",
    "peri-time",
    "peri-mcp-core",
    "mcp-packages/common",
    "mcp-packages/config",
    "mcp-packages/credentials",
    "mcp-packages/web",
    "mcp-packages/artifact",
    "mcp-packages/cron",
    "mcp-packages/workspace",
    "mcp-packages/workflow",
]


def git_blob_id(data: bytes) -> str:
    """Return the SHA-1 Git would assign to a blob with this content."""
    if b"\0" not in data[:8000]:
        data = data.replace(b"\r\n", b"\n")
    digest = hashlib.sha1()
    digest.update(f"blob {len(data)}\0".encode())
    digest.update(data)
    return digest.hexdigest()


def generate(upstream: Path, commit: str, output: Path) -> int:
    tree = subprocess.check_output(
        ["git", "-C", str(upstream), "ls-tree", "-r", "-z", commit, "--", *SCOPE]
    )
    entries = []
    for raw in tree.split(b"\0"):
        if not raw:
            continue
        meta, path = raw.split(b"\t", 1)
        _mode, kind, blob = meta.split()
        if kind == b"blob":
            entries.append((blob.decode(), path.decode()))
    entries.sort(key=lambda item: item[1])
    lines = [f"# upstream {commit}", *(f"{blob}\t{path}" for blob, path in entries)]
    output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"Wrote {len(entries)} entries to {output}")
    return 0


def read_manifest(path: Path) -> list[tuple[str, str]]:
    entries = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        blob, file_path = line.split("\t", 1)
        entries.append((blob, file_path))
    return entries


def read_allowlist(path: Path) -> set[str]:
    if not path.exists():
        return set()
    allowed = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            allowed.add(line)
    return allowed


def verify(root: Path, manifest: Path, allowlist: Path) -> int:
    entries = read_manifest(manifest)
    allowed = read_allowlist(allowlist)
    missing: list[str] = []
    drifted: list[str] = []
    allowed_drift: list[str] = []
    for expected, file_path in entries:
        target = root / file_path
        if not target.is_file():
            (allowed_drift if file_path in allowed else missing).append(file_path)
            continue
        actual = git_blob_id(target.read_bytes())
        if actual == expected:
            continue
        if file_path in allowed:
            allowed_drift.append(file_path)
        else:
            drifted.append(file_path)
    for file_path in sorted(allowed - {path for _, path in entries}):
        print(f"note: allowlist entry is outside the manifest: {file_path}")

    print(f"manifest: {len(entries)} files")
    print(f"identical: {len(entries) - len(missing) - len(drifted) - len(allowed_drift)}")
    print(f"allowed production patches: {len(allowed_drift)}")
    if missing:
        print(f"missing: {len(missing)}")
        for file_path in missing:
            print(f"  MISSING {file_path}")
    if drifted:
        print(f"unexpected drift: {len(drifted)}")
        for file_path in drifted:
            print(f"  DRIFT {file_path}")
    return 1 if missing or drifted else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--allowlist", type=Path, default=DEFAULT_ALLOWLIST)
    parser.add_argument("--generate", action="store_true", help="write the manifest from an upstream checkout")
    parser.add_argument("--upstream", type=Path, help="path to the upstream Git checkout")
    parser.add_argument("--commit", default=DEFAULT_COMMIT)
    args = parser.parse_args()

    if args.generate:
        if args.upstream is None:
            parser.error("--generate requires --upstream")
        return generate(args.upstream, args.commit, args.manifest)
    return verify(args.root, args.manifest, args.allowlist)


if __name__ == "__main__":
    sys.exit(main())
