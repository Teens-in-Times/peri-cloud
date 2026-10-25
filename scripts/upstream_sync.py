#!/usr/bin/env python3
"""Prepare or apply a bounded Peri overlay; never run incoming code or resolve conflicts."""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("overlay", Path(__file__).with_name("check-upstream.py"))
overlay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(overlay)
SCOPE = tuple(overlay.SCOPE)
PIN_PATH = "docs/upstream/overlay.json"
UPSTREAM = "KonghaYao/peri"
MAX_PATCH = 32 * 1024 * 1024
MAX_FILES = 5000
SHA = re.compile(r"[0-9a-f]{40}\Z")


def git(root: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, check=check)


def clean_bytes(data: bytes) -> bytes:
    return data.replace(b"\r\n", b"\n") if b"\0" not in data[:8000] else data


def safe_path(value: str) -> str:
    path = PurePosixPath(value)
    if (not value or path.is_absolute() or path.as_posix() != value or "\\" in value
            or any(c in value for c in ':*?<>|"') or any(ord(c) < 32 or ord(c) == 127 for c in value)
            or any(p.casefold() in ("..", ".", ".git") or p.rstrip(". ") != p for p in path.parts)):
        raise ValueError("unsafe source path")
    reserved = {"con", "prn", "aux", "nul", *(f"com{i}" for i in range(1, 10)),
                *(f"lpt{i}" for i in range(1, 10))}
    if any(p.split(".")[0].casefold() in reserved for p in path.parts):
        raise ValueError("non-portable source path")
    return value


def scoped(value: str) -> bool:
    safe_path(value)
    return any(value == scope or value.startswith(scope + "/") for scope in SCOPE)


def target(root: Path, name: str) -> Path:
    safe_path(name)
    path = root
    for part in PurePosixPath(name).parts:
        path = path / part
        if path.is_symlink() or (hasattr(path, "is_junction") and path.is_junction()):
            raise ValueError("source symlink is not permitted")
    if path.exists() and not path.is_file():
        raise ValueError("source target is not a regular file")
    if not path.resolve().is_relative_to(root.resolve()):
        raise ValueError("source escaped checkout")
    return path


def read_pin(root: Path) -> dict:
    pin = json.loads(target(root, PIN_PATH).read_text("utf-8"))
    if pin.get("repository") != UPSTREAM or not SHA.fullmatch(pin.get("revision", "")):
        raise ValueError("invalid upstream pin")
    if not re.fullmatch(r"docs/upstream/peri-[0-9a-f]{8,40}\.manifest", pin.get("manifest", "")):
        raise ValueError("invalid manifest path")
    return pin


def tree(repo: Path, revision: str) -> dict[str, tuple[str, str]]:
    raw = git(repo, "ls-tree", "-r", "-z", revision, "--", *SCOPE).stdout
    result = {}
    folded = set()
    for item in raw.split(b"\0"):
        if not item:
            continue
        meta, encoded = item.split(b"\t", 1)
        mode, kind, digest = meta.decode().split()
        name = safe_path(encoded.decode("utf-8"))
        if kind != "blob" or mode not in ("100644", "100755") or not scoped(name):
            raise ValueError("unsupported upstream entry")
        if name.casefold() in folded:
            raise ValueError("case-colliding upstream paths")
        folded.add(name.casefold())
        result[name] = (mode, digest)
    if len(result) > MAX_FILES:
        raise ValueError("upstream scope exceeds file budget")
    return result


def blob(repo: Path, revision: str, name: str) -> bytes:
    data = git(repo, "show", revision + ":" + name).stdout
    if len(data) > 8 * 1024 * 1024:
        raise ValueError("source file exceeds byte budget")
    return clean_bytes(data)


def merged(ours: bytes | None, base: bytes | None, theirs: bytes | None) -> bytes | None:
    if ours == base or ours == theirs:
        return theirs
    if theirs == base:
        return ours
    if ours is None or theirs is None or base is None:
        raise ValueError("modify/delete or add/add conflict")
    if any(b"\0" in data for data in (ours, base, theirs)):
        raise ValueError("binary conflict")
    with tempfile.TemporaryDirectory(prefix="peri-overlay-") as folder:
        paths = [Path(folder) / name for name in ("ours", "base", "theirs")]
        for path, data in zip(paths, (ours, base, theirs)):
            path.write_bytes(data)
        result = subprocess.run(["git", "merge-file", "-p", *map(str, paths)], capture_output=True)
        if result.returncode:
            raise ValueError("text merge conflict")
        return result.stdout


def replace_once(data: bytes, old: str, new: str) -> bytes:
    text = data.decode("utf-8")
    if text.count(old) != 1:
        raise ValueError("provenance text changed; manual review required")
    return text.replace(old, new).encode("utf-8")


def write_report(output: Path, report: dict) -> dict:
    output.mkdir(parents=True, exist_ok=True)
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n", "utf-8")
    return report


def prepare(root: Path, repo: Path, head: str, output: Path) -> dict:
    root, repo = root.resolve(), repo.resolve()
    if output.exists() and any(output.iterdir()):
        raise ValueError("candidate output must be new or empty")
    if git(root, "status", "--porcelain", "--untracked-files=all").stdout:
        raise ValueError("source checkout must be clean")
    pin = read_pin(root)
    base = pin["revision"]
    if not SHA.fullmatch(head):
        raise ValueError("upstream head must be a full commit SHA")
    report = {"version": 1, "repository": UPSTREAM, "base_commit": git(root, "rev-parse", "HEAD").stdout.decode().strip(),
              "upstream_base": base, "upstream_head": head, "state": "unchanged", "files": [], "conflicts": []}
    if head == base:
        return write_report(output, report)
    ancestor = git(repo, "merge-base", "--is-ancestor", base, head, check=False)
    if ancestor.returncode not in (0, 1):
        raise ValueError("cannot verify upstream ancestry")
    if ancestor.returncode == 1:
        report.update(state="blocked", reason="upstream history is not a descendant of the recorded pin")
        return write_report(output, report)
    changed_names = git(repo, "diff", "--name-only", "-z", base, head).stdout.split(b"\0")
    attribution = re.compile(r"(?:licen[cs]e|copying|notice|authors|copyright)(?:[._-].*)?\Z", re.I)
    if any(attribution.fullmatch(part) or part.casefold() == "licenses"
           for name in changed_names if name for part in name.decode("utf-8").split("/")):
        report.update(state="blocked", reason="upstream license/attribution changed; manual review required")
        return write_report(output, report)
    old, new = tree(repo, base), tree(repo, head)
    manifest = dict((p, digest) for digest, p in overlay.read_manifest(target(root, pin["manifest"])))
    if manifest != {p: digest for p, (_, digest) in old.items()}:
        raise ValueError("recorded manifest does not match the actual upstream base")
    allowed = overlay.read_allowlist(root / "docs/upstream/production-patches.txt")
    if overlay.verify(root, target(root, pin["manifest"]), root / "docs/upstream/production-patches.txt"):
        raise ValueError("undeclared local drift")
    changed = [p for p in sorted(old.keys() | new.keys()) if old.get(p) != new.get(p)]
    if not changed:
        report["reason"] = "upstream changed only outside the recorded source scope"
        return write_report(output, report)
    tracked = git(root, "ls-files", "-z").stdout.decode("utf-8").split("\0")
    folded = {p.casefold(): p for p in tracked if p}
    index_modes = {}
    for entry in git(root, "ls-files", "--stage", "-z").stdout.split(b"\0"):
        if entry:
            meta, name = entry.split(b"\t", 1)
            index_modes[name.decode("utf-8")] = meta.split()[0].decode()
    plan = {}
    modes = {}
    for name in changed:
        try:
            if name.casefold() in folded and folded[name.casefold()] != name:
                raise ValueError("case collision with deployment source")
            path = target(root, name)
            if path.exists() and name not in index_modes:
                raise ValueError("incoming path collides with an untracked/ignored file")
            ours = clean_bytes(path.read_bytes()) if path.exists() else None
            original = blob(repo, base, name) if name in old else None
            incoming = blob(repo, head, name) if name in new else None
            if ours != original and name not in allowed and name in old:
                raise ValueError("undeclared local modification")
            our_mode = index_modes.get(name) if ours is not None else None
            base_mode = old.get(name, (None, None))[0]
            next_mode = new.get(name, (None, None))[0]
            if our_mode == base_mode or our_mode == next_mode:
                mode = next_mode
            elif next_mode == base_mode:
                mode = our_mode
            else:
                raise ValueError("file mode conflict")
            value = merged(ours, original, incoming)
            if value is not None and name.endswith(".rs") and len(value.splitlines()) >= 1000:
                raise ValueError("Rust source/test must remain below 1000 lines")
            if value != ours:
                plan[name] = value
            if name in new:
                modes[name] = mode
                if our_mode != mode:
                    plan[name] = value
        except ValueError as error:
            report["conflicts"].append({"path": name, "reason": str(error)})
    if report["conflicts"]:
        report.update(state="conflict", conflict_count=len(report["conflicts"]))
        report["conflicts"] = report["conflicts"][:100]
        return write_report(output, report)
    prefix = head[:12]
    manifest_name = f"docs/upstream/peri-{prefix}.manifest"
    record_name = f"docs/upstream/peri-{prefix}.md"
    if target(root, manifest_name).exists() or target(root, record_name).exists():
        raise ValueError("candidate provenance filename already exists")
    plan[manifest_name] = ("\n".join([f"# upstream {head}", *(f"{digest}\t{p}" for p, (_, digest) in sorted(new.items()))]) + "\n").encode()
    next_pin = {"repository": UPSTREAM, "revision": head, "manifest": manifest_name}
    plan[PIN_PATH] = (json.dumps(next_pin, indent=2) + "\n").encode()
    for name, old_text, new_text in (
        ("AGENTS.md", f"revision {base}.", f"revision {head}."),
        ("NOTICE", f"Base revision: {base}", f"Base revision: {head}"),
        ("README.md", f"`{base}`", f"`{head}`"),
    ):
        plan[name] = replace_once(target(root, name).read_bytes(), old_text, new_text)
    old_record = pin["manifest"].removesuffix(".manifest") + ".md"
    # Historical notes can link the same record; only advance the current source attribution.
    plan["README.md"] = replace_once(
        plan["README.md"], f"[上游升级记录]({old_record})", f"[上游升级记录]({record_name})")
    compare = f"https://github.com/{UPSTREAM}/compare/{base}...{head}"
    plan[record_name] = (f"# Peri 自动同步候选 {prefix}\n\n上游：`{base}` → `{head}`。\n\n"
        f"[上游差异]({compare})；源码范围变化 {len(changed)} 项。\n\n"
        "本候选采用确定性三方合并，保留已登记本地补丁，不自动修改依赖锁、扩大补丁白名单、解决冲突或解释产品语义。\n\n"
        "必须人工复核权限、身份、任务恢复、上下文、工作区、依赖和提示词变化；检查结果见关联 PR，不能将无冲突当作验收通过。\n\n"
        "现有 Cloud/Executor 账本与安全边界仍由本发行拥有。本流程不合并 main、不部署、不调用真实模型。\n").encode()
    # Validate every destination before writing anything. No recursive deletes or directory replacement.
    for name in plan:
        target(root, name)
    for name, value in plan.items():
        path = target(root, name)
        if value is None:
            path.unlink()
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(value)
            if name in modes and os.name != "nt":
                path.chmod(0o755 if modes[name] == "100755" else 0o644)
    if overlay.verify(root, root / manifest_name, root / "docs/upstream/production-patches.txt"):
        raise ValueError("candidate violates provenance contract")
    git(root, "add", "--", *sorted(plan))
    patch = git(root, "diff", "--cached", "--no-renames", "--binary", "--full-index").stdout
    if len(patch) > MAX_PATCH:
        raise ValueError("candidate patch exceeds byte budget")
    output.mkdir(parents=True, exist_ok=True)
    (output / "candidate.patch").write_bytes(patch)
    report.update(state="prepared", files=sorted(plan), scoped_changes=len(changed),
                  branch=f"codex/upstream-sync-{base[:12]}-{prefix}-{report['base_commit'][:8]}",
                  patch_sha256=hashlib.sha256(patch).hexdigest(), patch_bytes=len(patch))
    return write_report(output, report)


def load_report(folder: Path) -> dict:
    path = folder / "report.json"
    if path.stat().st_size > 1024 * 1024:
        raise ValueError("report exceeds byte budget")
    report = json.loads(path.read_text("utf-8"))
    if report.get("version") != 1 or report.get("repository") != UPSTREAM:
        raise ValueError("invalid report provenance")
    for field in ("base_commit", "upstream_base", "upstream_head"):
        if not SHA.fullmatch(report.get(field, "")):
            raise ValueError("invalid report identity")
    if report.get("state") not in ("prepared", "conflict", "blocked", "unchanged"):
        raise ValueError("invalid report state")
    return report


def apply(root: Path, folder: Path) -> dict:
    report = load_report(folder)
    if report["state"] != "prepared":
        raise ValueError("no prepared candidate")
    if git(root, "status", "--porcelain", "--untracked-files=all").stdout:
        raise ValueError("publisher checkout must be clean")
    if git(root, "rev-parse", "HEAD").stdout.decode().strip() != report["base_commit"]:
        raise ValueError("candidate belongs to a different source checkout")
    if read_pin(root)["revision"] != report["upstream_base"]:
        raise ValueError("candidate base pin changed")
    prefix = report["upstream_head"][:12]
    exact = {PIN_PATH, "AGENTS.md", "NOTICE", "README.md",
             f"docs/upstream/peri-{prefix}.manifest", f"docs/upstream/peri-{prefix}.md"}
    files = report.get("files", [])
    if (not files or len(files) > MAX_FILES or len(files) != len(set(files))
            or any(not (scoped(p) or p in exact) for p in files)):
        raise ValueError("candidate modifies a protected path")
    patch = (folder / "candidate.patch").read_bytes()
    if len(patch) > MAX_PATCH or hashlib.sha256(patch).hexdigest() != report.get("patch_sha256"):
        raise ValueError("candidate patch identity mismatch")
    # Git parses the diff; no shell interpolation, archive extraction, hooks or candidate execution.
    parsed = git(root, "apply", "--numstat", "-z", str((folder / "candidate.patch").resolve())).stdout
    names = [item.split(b"\t", 2)[2].decode("utf-8") for item in parsed.split(b"\0") if item]
    if sorted(names) != sorted(files):
        raise ValueError("patch paths differ from the prepared manifest")
    for name in files:
        target(root, name)
    git(root, "apply", "--check", "--index", str((folder / "candidate.patch").resolve()))
    # Reject symlink/submodule mode changes before applying to the write-credential job.
    for line in patch.splitlines():
        if line.startswith((b"new file mode ", b"new mode ")) and line.split()[-1] not in (b"100644", b"100755"):
            raise ValueError("unsupported candidate file mode")
    git(root, "apply", "--index", str((folder / "candidate.patch").resolve()))
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("fetch", "prepare", "apply"))
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--upstream", type=Path)
    parser.add_argument("--head")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "fetch":
        if args.upstream is None:
            parser.error("fetch requires --upstream")
        pin = read_pin(args.root)
        args.upstream.mkdir(parents=True, exist_ok=False)
        git(args.upstream, "init", "-q")
        git(args.upstream, "remote", "add", "origin", f"https://github.com/{UPSTREAM}.git")
        git(args.upstream, "fetch", "--no-tags", "--filter=blob:none", "origin", "main")
        if git(args.upstream, "cat-file", "-e", pin["revision"] + "^{commit}", check=False).returncode:
            git(args.upstream, "fetch", "--no-tags", "--filter=blob:none", "origin", pin["revision"])
        print(git(args.upstream, "rev-parse", "refs/remotes/origin/main^{commit}").stdout.decode().strip())
        return
    if args.command == "prepare":
        if args.upstream is None or args.head is None:
            parser.error("prepare requires --upstream and --head")
        report = prepare(args.root, args.upstream, args.head, args.output)
    else:
        report = apply(args.root, args.output)
    print(json.dumps({k: report[k] for k in ("state", "upstream_base", "upstream_head")}))


if __name__ == "__main__":
    main()
