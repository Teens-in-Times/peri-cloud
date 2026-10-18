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
    "/*__CANVAS__*/": (root / "canvas.js").read_text(encoding="utf-8"),
    "/*__APP__*/": (root / "app.js").read_text(encoding="utf-8"),
}
for marker, content in replacements.items():
    if template.count(marker) != 1:
        raise ValueError(f"Expected one marker: {marker}")
    template = template.replace(marker, content)
if len(template.splitlines()) >= 1000:
    raise ValueError("Generated page must stay below 1000 lines")
(root / "index.html").write_text(template, encoding="utf-8", newline="\n")
print(json.dumps({"output": str(root / "index.html"), "bytes": len(template.encode("utf-8")),
                  "lines": len(template.splitlines())}, ensure_ascii=False))
