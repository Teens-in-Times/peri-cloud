"""Record exact checkout and native binary digests alongside CI artifacts."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

directory = Path(sys.argv[1])
suffix = ".exe" if os.name == "nt" else ""
record = {
    "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "tree": subprocess.check_output(["git", "rev-parse", "HEAD^{tree}"], text=True).strip(),
    "platform": "windows-x86_64" if suffix else "linux-x86_64",
    "binaries": {name + suffix: hashlib.sha256((directory / (name + suffix)).read_bytes()).hexdigest()
                 for name in ("peri-cloud-host", "peri-executor")},
}
(directory / "SOURCE.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
