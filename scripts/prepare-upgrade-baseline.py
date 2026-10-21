"""Download the pinned public release host for the disposable upgrade rehearsal."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import zipfile

REPOSITORY = "Teens-in-Times/peri-cloud"
TAG = "v0.7.1"
COMMIT = "ebde31d2bd7de794750bc9e187e1cdec5a5c4619"

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, required=True, help="New baseline directory")
args = parser.parse_args()
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=False)
windows = os.name == "nt"
asset = f"peri-cloud-{TAG}-{'windows-x86_64.zip' if windows else 'linux-x86_64.tar.gz'}"
subprocess.run(["gh", "release", "download", TAG, "--repo", REPOSITORY,
                "--pattern", asset, "--pattern", "SHA256SUMS", "--dir", str(output)], check=True)
checksums = {}
for line in (output / "SHA256SUMS").read_text("utf-8").splitlines():
    value, name = line.split(maxsplit=1)
    checksums[name.lstrip("*")] = value
archive = output / asset
actual = hashlib.sha256(archive.read_bytes()).hexdigest()
if actual != checksums.get(asset):
    raise SystemExit("Baseline archive checksum mismatch")
name = "peri-cloud-host" + (".exe" if windows else "")
if windows:
    with zipfile.ZipFile(archive) as source:
        build = json.loads(source.read("BUILD.json"))
        binary = source.read(name)
else:
    with tarfile.open(archive) as source:
        build = json.load(source.extractfile("BUILD.json"))
        binary = source.extractfile(name).read()
if build["commit"] != COMMIT or build["version"] != TAG.removeprefix("v"):
    raise SystemExit("Baseline source identity mismatch")
if not binary.startswith(b"MZ" if windows else b"\x7fELF"):
    raise SystemExit("Baseline binary platform mismatch")
target = output / name
target.write_bytes(binary)
if not windows:
    target.chmod(0o755)
provenance = {"tag": TAG, "commit": COMMIT, "archive_sha256": actual,
              "binary_sha256": hashlib.sha256(binary).hexdigest()}
(output / "baseline.json").write_text(json.dumps(provenance, indent=2) + "\n", encoding="utf-8")
print(json.dumps(provenance))
