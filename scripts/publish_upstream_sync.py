#!/usr/bin/env python3
"""Publish a prepared artifact using trusted scripts only; no candidate code runs here."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request

from upstream_sync import apply, git, load_report

PREFIX = "codex/upstream-sync-"
MARKER = "<!-- peri-upstream-sync:"


class GitHub:
    def __init__(self, repository: str, token: str):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
            raise ValueError("invalid destination repository")
        self.repository, self.token = repository, token

    def api(self, path: str, data: dict | None = None, missing: bool = False):
        request = urllib.request.Request(
            f"https://api.github.com/repos/{self.repository}/{path}",
            data=json.dumps(data).encode() if data is not None else None,
            headers={"Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json",
                     "User-Agent": "peri-upstream-sync", "X-GitHub-Api-Version": "2022-11-28"},
        )
        try:
            with urllib.request.urlopen(request, timeout=45) as response:
                raw = response.read()
                return json.loads(raw) if raw else None
        except urllib.error.HTTPError as error:
            if missing and error.code == 404:
                return None
            # Do not print request headers, tokens, or arbitrary response bodies.
            raise RuntimeError(f"GitHub API {error.code} at {path.split('?')[0]}") from None


def display(value) -> str:
    return json.dumps(value, ensure_ascii=True, indent=2).replace("`", "\\u0060").replace("<", "\\u003c")


def pages(github: GitHub, path: str) -> list:
    result = []
    for page in range(1, 11):
        values = github.api(path + f"&per_page=100&page={page}")
        result.extend(values)
        if len(values) < 100:
            return result
    raise ValueError("review history exceeds the bounded scan; manual triage required")


def ci_dispatch(github: GitHub, branch: str, commit: str) -> None:
    query = urllib.parse.urlencode({"branch": branch, "per_page": 100})
    runs = github.api("actions/workflows/ci.yml/runs?" + query)["workflow_runs"]
    # A prior dispatched run owns validation of this immutable head, including failures.
    if any(r["head_sha"] == commit and r["event"] == "workflow_dispatch" for r in runs):
        print("CI already dispatched for this head; use CI rerun for an explicit retry")
        return
    if len(runs) == 100:
        raise ValueError("CI history exceeds bounded scan; retry explicitly through CI")
    github.api("actions/workflows/ci.yml/dispatches", {"ref": branch})
    print("CI dispatched for immutable candidate", commit)


def publish(root: Path, folder: Path, github: GitHub) -> str:
    report = load_report(folder)
    state = report["state"]
    if state == "unchanged":
        print("No changes within the recorded overlay scope")
        return "unchanged"
    base, head = report["upstream_base"], report["upstream_head"]
    marker = f"{MARKER}{base}:{head} -->"
    if state in ("conflict", "blocked"):
        # Report once per source pair; never push conflict markers or delete customized source.
        issues = pages(github, "issues?state=all&sort=created&direction=desc")
        existing = next((i for i in issues if marker in (i.get("body") or "")), None)
        if existing:
            print("Already reported", existing["html_url"])
            return "reported"
        body = (marker + "\n\n自动同步需要人工处理；未创建源码分支或 PR。\n\n"
                f"上游：`{base}` → `{head}`。\n\n```json\n" + display({
                    "state": state, "reason": report.get("reason"), "conflicts": report.get("conflicts", []),
                    "attribution_changes": report.get("attribution_changes", []),
                    "conflict_count": report.get("conflict_count", 0)}) + "\n```\n\n"
                "请保留本地权限、身份、任务恢复与工作区语义；不要将无冲突当作验收。")
        issue = github.api("issues", {"title": f"上游同步需人工处理：{base[:8]} → {head[:8]}", "body": body})
        print("Reported", issue["html_url"])
        return "reported"
    expected = f"{PREFIX}{base[:12]}-{head[:12]}-{report['base_commit'][:8]}"
    if report.get("branch") != expected:
        raise ValueError("invalid candidate branch")
    # Only one pending automatic candidate. Never rewrite a human-reviewed branch.
    pending = pages(github, "pulls?state=open")
    own = next((p for p in pending if p["head"]["ref"] == expected and marker in (p.get("body") or "")), None)
    if own is None and any(p["head"]["ref"].startswith(PREFIX) and MARKER in (p.get("body") or "") for p in pending):
        print("Another automatic candidate awaits review; no new PR or branch")
        return "pending"
    closed = pages(github, "pulls?state=closed&sort=created&direction=desc")
    if any(marker in (p.get("body") or "") for p in closed):
        print("This candidate was already closed; it will not be reopened")
        return "closed"
    remote_main = github.api("git/ref/heads/main")["object"]["sha"]
    if remote_main != report["base_commit"]:
        print("main advanced during preparation; next run will recompute the candidate")
        return "stale"
    apply(root, folder)
    tree = git(root, "write-tree").stdout.decode().strip()
    existing_ref = github.api("git/ref/heads/" + expected, missing=True)
    if own and existing_ref is None:
        raise ValueError("reviewed candidate branch was removed; refusing to recreate it")
    if existing_ref:
        commit = existing_ref["object"]["sha"]
        if github.api("git/commits/" + commit)["tree"]["sha"] != tree:
            raise ValueError("existing branch has different contents; refusing to overwrite")
        if own:
            if own["head"]["sha"] != commit:
                raise ValueError("candidate head changed")
            ci_dispatch(github, expected, commit)
            print("Existing candidate preserved", own["html_url"])
            return "existing"
    else:
        git(root, "config", "user.name", "github-actions[bot]")
        git(root, "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com")
        git(root, "switch", "-c", expected)
        git(root, "-c", "core.hooksPath=/dev/null", "commit", "-m", f"Prepare Peri upstream {head[:12]} for manual review")
        commit = git(root, "rev-parse", "HEAD").stdout.decode().strip()
        # Auth is supplied only to this trusted publisher. No credentials are stored in checkout.
        result = subprocess.run(["git", "-C", str(root), "-c", "credential.helper=",
                                 "-c", "credential.helper=!gh auth git-credential", "push", "origin",
                                 f"HEAD:refs/heads/{expected}"], capture_output=True)
        if result.returncode:
            raise RuntimeError("candidate branch push failed; no force push attempted")
    body = (marker + "\n\n自动生成的确定性三方同步候选，**仅供人工审查和合并**。\n\n"
            f"上游：`{base}` → `{head}`；本仓库基线：`{report['base_commit']}`。\n\n"
            f"[上游差异](https://github.com/{report['repository']}/compare/{base}...{head})\n\n"
            "保留原补丁白名单；不自动更新依赖锁、解决冲突、批准 PR、合并 main 或部署。"
            "请复核权限、持久恢复、工作区、上下文、依赖及提示词变化。无文本冲突不代表行为兼容。\n\n"
            "CI 将通过 workflow_dispatch 显式触发，检查失败保持草稿等待维护者处理。\n\n"
            f"范围内变化：{report.get('scoped_changes', 0)} 项；patch SHA-256：`{report['patch_sha256']}`。\n\n"
            "候选路径（最多 80 项）：\n\n```json\n" + display(report["files"][:80]) + "\n```\n")
    pr = github.api("pulls", {"title": f"[自动同步 / 人工审查] Peri {head[:12]}", "head": expected,
                              "base": "main", "body": body, "draft": True, "maintainer_can_modify": True})
    print("Created", pr["html_url"])
    ci_dispatch(github, expected, commit)
    return "created"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--artifact", type=Path, required=True)
    args = parser.parse_args()
    if os.environ.get("GITHUB_REF") != "refs/heads/main" or os.environ.get("GITHUB_EVENT_NAME") not in ("schedule", "workflow_dispatch"):
        raise SystemExit("publisher runs only from main schedule/manual events")
    github = GitHub(os.environ["GITHUB_REPOSITORY"], os.environ["GH_TOKEN"])
    result = publish(args.root.resolve(), args.artifact.resolve(), github)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as output:
            output.write(f"\nPublication: `{result}`. Human merge only; no deployment.\n")


if __name__ == "__main__":
    main()
