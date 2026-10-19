"""Build the dependency-free, offline-capable design page. No production files."""
from pathlib import Path
import json
import re

root = Path(__file__).resolve().parent
template = (root / "page.html").read_text(encoding="utf-8")
css = (root / "styles.css").read_text(encoding="utf-8")
prompts = {key: (root / "prompts" / f"{key}.md").read_text(encoding="utf-8")
           for key in ("main", "worker", "frames")}
replacements = {
    "/*__STYLES__*/": re.sub(r"\s+", " ", css),
    "/*__DATA__*/": (root / "data.js").read_text(encoding="utf-8"),
    "/*__INTEGRATION__*/": (root / "integration.js").read_text(encoding="utf-8"),
    "/*__PROMPTS__*/": "window.PERI_PROMPTS=" + json.dumps(prompts, ensure_ascii=False).replace("<", "\\u003c") + ";",
    "/*__LAYOUT__*/": (root / "layout.js").read_text(encoding="utf-8"),
    "/*__CACHE__*/": (root / "cache.js").read_text(encoding="utf-8"),
    "/*__NAVIGATION__*/": (root / "navigation.js").read_text(encoding="utf-8"),
    "/*__RESET__*/": "",
    "/*__CANVAS__*/": (root / "canvas.js").read_text(encoding="utf-8"),
    "/*__APP__*/": (root / "app.js").read_text(encoding="utf-8"),
}
atlas = root / "atlas-workspace.json"
if atlas.exists():
    workspace = json.loads(atlas.read_text(encoding="utf-8"))
    template = template.replace('<script id="workbench-snapshot" type="application/json">null</script>',
                                '<script id="workbench-snapshot" type="application/json">' +
                                json.dumps(workspace, ensure_ascii=False, separators=(",", ":")).replace("<", "\\u003c") + '</script>')
    # A new snapshot uses its own storage key; updating it never clears old drafts.
for marker, content in replacements.items():
    if template.count(marker) != 1:
        raise ValueError(f"Expected one marker: {marker}")
    template = template.replace(marker, content)
if len(template.splitlines()) >= 1000:
    raise ValueError("Generated page must stay below 1000 lines")
(root / "index.html").write_text(template, encoding="utf-8", newline="\n")
if atlas.exists():
    (root / "atlas-20261019.html").write_text(template, encoding="utf-8", newline="\n")
    fresh = dict(workspace, workspaceId="peri-execution-20261019-schema23")
    old_snapshot = json.dumps(workspace, ensure_ascii=False, separators=(",", ":")).replace("<", "\\u003c")
    fresh_snapshot = json.dumps(fresh, ensure_ascii=False, separators=(",", ":")).replace("<", "\\u003c")
    if template.count(old_snapshot) != 1:
        raise ValueError("Expected one immutable source snapshot")
    (root / "execution-architecture-20261019.html").write_text(
        template.replace(old_snapshot, fresh_snapshot), encoding="utf-8", newline="\n")
print(json.dumps({"output": str(root / "index.html"), "bytes": len(template.encode("utf-8")),
                  "lines": len(template.splitlines())}, ensure_ascii=False))
