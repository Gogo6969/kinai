#!/usr/bin/env python3
"""Generate the KinAI project map: docs/kinai-map.json + docs/kinai-map.html.

The map is GENERATED, never hand-written. A hand-maintained architecture
doc is wrong within a fortnight and then actively misleads; this one is
rebuilt from the repository itself, so "what does this module do" comes
from that module's own `//!` doc comment and "how did we get here" comes
from CHANGELOG.md and the git tags.

    python3 scripts/build-map.py

Both outputs are written together and the HTML embeds the JSON inline, so
the page works from a file:// URL with no server and no fetch.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"

# Where each part of the Rust tree sits in the story of the app. Order is
# the order a newcomer should read them in, not alphabetical.
AREAS = [
    ("entry", "Entry & app shell", ["main.rs", "lib.rs", "commands.rs", "updater.rs", "config.rs"]),
    ("llm", "Model I/O", ["llm/"]),
    ("context", "Prompt & context", ["context/"]),
    ("tools", "Tools", ["tools/"]),
    ("routing", "Slot routing", ["slash.rs", "vision.rs", "factcheck.rs"]),
    ("network", "Host ↔ client", ["network/"]),
    ("db", "Storage", ["db/"]),
    ("auth", "Auth", ["auth/"]),
    ("telegram", "Telegram bridge", ["telegram/"]),
    ("media", "Voice & media", ["tts.rs", "stt.rs", "comfy.rs", "comfyui.rs", "discovery.rs"]),
    ("desktop", "Desktop shell", ["tray.rs", "hotkey.rs", "changelog.rs"]),
    ("updates", "Update distribution", ["update_sync.rs"]),
    ("input", "Attachments", ["attachments.rs"]),
]


def sh(*args: str) -> str:
    try:
        return subprocess.run(args, cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
    except Exception:
        return ""


def area_for(rel: str) -> str:
    for key, _label, prefixes in AREAS:
        for p in prefixes:
            if rel == p or rel.startswith(p):
                return key
    return "other"


def rust_doc(text: str) -> str:
    """The leading `//!` block, collapsed to a sentence or two."""
    lines = []
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("//!"):
            lines.append(s[3:].strip())
        elif not lines:
            if s.startswith("//") or s == "":
                continue
            break
        else:
            break
    # Rust convention: the summary is everything up to the first blank
    # doc line. Falls back to the whole block for single-paragraph docs.
    summary = []
    for l in lines:
        if not l and summary:
            break
        if l:
            summary.append(l)
    doc = re.sub(r"\s+", " ", " ".join(summary)).strip()
    return doc if len(doc) <= 240 else doc[:237].rsplit(" ", 1)[0] + "…"


def leading_comment(text: str) -> str:
    """First prose comment in a .svelte/.ts file.

    These files start with `<script lang="ts">` and a wall of imports, so
    there is rarely a comment at line 1. Scan the head of the file for the
    first comment that reads like a sentence — skipping lint directives,
    TODOs and one-word notes — and treat that as the description.
    """
    head = "\n".join(text.splitlines()[:80])
    cands = []
    for m in re.finditer(r"<!--(.*?)-->|/\*(.*?)\*/", head, re.S):
        cands.append((m.start(), m.group(1) or m.group(2)))
    for m in re.finditer(r"((?:^[ \t]*//[^\n]*\n)+)", head, re.M):
        cands.append((m.start(), " ".join(l.strip().lstrip("/ ") for l in m.group(1).splitlines())))
    cands.sort()
    for _pos, raw in cands:
        # Strip the leading "*" gutter of JSDoc-style block comments.
        raw = "\n".join(re.sub(r"^\s*\*+ ?", "", l) for l in raw.splitlines())
        body = re.sub(r"\s+", " ", raw).strip()
        if len(body) < 40:
            continue
        if re.match(r"(eslint|prettier|@ts-|todo|fixme|svelte-ignore)", body, re.I):
            continue
        return body[:400]
    return ""


def collect_rust() -> list[dict]:
    out = []
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = str(path.relative_to(ROOT / "src"))
        text = path.read_text(encoding="utf-8", errors="replace")
        out.append({
            "path": f"src/{rel}",
            "area": area_for(rel),
            "lines": text.count("\n") + 1,
            "doc": rust_doc(text),
            "tests": len(re.findall(r"#\[(?:tokio::)?test\]", text)),
        })
    return out


def collect_frontend() -> list[dict]:
    out = []
    base = ROOT / "frontend" / "src"
    for path in sorted(list(base.rglob("*.svelte")) + list(base.rglob("*.ts"))):
        rel = str(path.relative_to(ROOT / "frontend"))
        text = path.read_text(encoding="utf-8", errors="replace")
        kind = "route" if "/routes/" in rel else ("component" if "/components/" in rel else "lib")
        out.append({
            "path": f"frontend/{rel}",
            "kind": kind,
            "lines": text.count("\n") + 1,
            "doc": leading_comment(text),
        })
    return out


def collect_tests() -> list[dict]:
    out = []
    for path in sorted((ROOT / "tests").glob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="replace")
        fns = []
        for m in re.finditer(r"#\[(?:tokio::)?test\]\s*(#\[ignore[^\]]*\]\s*)?(?:async\s+)?fn\s+(\w+)", text):
            fns.append({"name": m.group(2), "ignored": bool(m.group(1))})
        out.append({
            "file": f"tests/{path.name}",
            "doc": rust_doc(text),
            "tests": fns,
        })
    # Unit tests live beside the code; count them per module.
    return out


def collect_changelog() -> list[dict]:
    text = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    versions = []
    # ## [0.2.89] — 2026-07-27
    blocks = re.split(r"^## \[", text, flags=re.M)[1:]
    for b in blocks:
        head, _, body = b.partition("\n")
        m = re.match(r"([0-9][^\]]*)\]\s*[—-]\s*(\S+)", head)
        if not m:
            continue
        version, date = m.group(1), m.group(2)
        sections = {}
        for sm in re.finditer(r"^### ([^\n]+)\n(.*?)(?=^### |\Z)", body, re.M | re.S):
            label, chunk = sm.group(1).strip(), sm.group(2)
            # "Fixed (image handling)" -> kind "Fixed", label kept intact.
            name = label.split()[0].rstrip(":")
            items = []
            for im in re.finditer(r"^- (.+?)(?=^- |\Z)", chunk, re.M | re.S):
                raw = re.sub(r"\s+", " ", im.group(1)).strip()
                tm = re.match(r"\*\*(.+?)\*\*\s*(.*)", raw)
                items.append({
                    "title": (tm.group(1) if tm else raw[:120]).strip(),
                    "detail": (tm.group(2) if tm else "").strip(),
                })
            if items:
                sections[name] = sections.get(name, []) + items
        versions.append({"version": version, "date": date, "sections": sections})
    return versions


def collect_tags() -> list[dict]:
    raw = sh("git", "for-each-ref", "--sort=-creatordate",
             "--format=%(refname:short)|%(creatordate:short)", "refs/tags")
    out = []
    for line in raw.splitlines():
        if "|" in line:
            name, date = line.split("|", 1)
            out.append({"tag": name.strip(), "date": date.strip()})
    return out


def grep_all(path: str, pattern: str, group: int = 1) -> list[str]:
    p = ROOT / path
    if not p.exists():
        return []
    text = p.read_text(encoding="utf-8", errors="replace")
    return sorted({m.group(group) for m in re.finditer(pattern, text, re.M)})


def slot_names() -> list[str]:
    """The routing slots, read from the one place that defines them."""
    p = ROOT / "src/slash.rs"
    if not p.exists():
        return []
    m = re.search(r"SLOTS: &\[&str\] = &\[(.*?)\]", p.read_text(encoding="utf-8", errors="replace"), re.S)
    return re.findall(r'"(\w+)"', m.group(1)) if m else []


def collect_surfaces() -> dict:
    proto = ROOT / "src/network/protocol.rs"
    envelope = []
    if proto.exists():
        t = proto.read_text(encoding="utf-8", errors="replace")
        em = re.search(r"pub enum Envelope\s*\{(.*?)\n\}", t, re.S)
        if em:
            envelope = re.findall(r"^\s{4}([A-Z]\w+)", em.group(1), re.M)
    return {
        "db_tables": grep_all("src/db/migrate.rs", r"CREATE TABLE IF NOT EXISTS (\w+)"),
        "protocol_envelope": envelope,
        "tools": grep_all("src/tools/registry.rs", r'"(web_search|x_search|calculator|datetime|image_search|remember|recall)"'),
        "slots": slot_names(),
        "config_structs": grep_all("src/config.rs", r"^pub struct (\w+)"),
        "tauri_commands": grep_all("src/commands.rs", r"#\[tauri::command\]\s*\npub(?:\(crate\))? async fn (\w+)"),
    }


def build() -> dict:
    version = ""
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    vm = re.search(r'^version = "([^"]+)"', cargo, re.M)
    if vm:
        version = vm.group(1)

    rust = collect_rust()
    frontend = collect_frontend()
    tests = collect_tests()
    versions = collect_changelog()
    tags = collect_tags()

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "version": version,
        "repo": {
            "name": "KinAI",
            "url": "https://github.com/Gogo6969/kinai",
            "first_commit": sh("git", "log", "--reverse", "--format=%ad", "--date=short").split("\n")[0] if sh("git", "log", "-1") else "",
            "commits": int(sh("git", "rev-list", "--count", "HEAD") or 0),
            "head": sh("git", "rev-parse", "--short", "HEAD"),
        },
        "totals": {
            "rust_files": len(rust),
            "rust_lines": sum(f["lines"] for f in rust),
            "frontend_files": len(frontend),
            "frontend_lines": sum(f["lines"] for f in frontend),
            "unit_tests": sum(f["tests"] for f in rust),
            "integration_test_files": len(tests),
            "releases": len(versions),
            "tags": len(tags),
        },
        "areas": [{"key": k, "label": l} for k, l, _ in AREAS] + [{"key": "other", "label": "Other"}],
        "rust": rust,
        "frontend": frontend,
        "tests": tests,
        "surfaces": collect_surfaces(),
        "history": versions,
        "tags": tags,
    }


def main() -> int:
    DOCS.mkdir(exist_ok=True)
    data = build()

    json_path = DOCS / "kinai-map.json"
    json_path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    tpl = (ROOT / "scripts" / "map-template.html").read_text(encoding="utf-8")
    html = tpl.replace("/*__KINAI_MAP_DATA__*/null", json.dumps(data, ensure_ascii=False))
    (DOCS / "kinai-map.html").write_text(html, encoding="utf-8")

    t = data["totals"]
    print(f"docs/kinai-map.json  {json_path.stat().st_size/1024:.0f} KB")
    print(f"docs/kinai-map.html  {(DOCS/'kinai-map.html').stat().st_size/1024:.0f} KB")
    print(f"  {t['rust_files']} rust files / {t['rust_lines']} lines, "
          f"{t['frontend_files']} frontend / {t['frontend_lines']} lines, "
          f"{t['releases']} releases, {t['unit_tests']} unit tests")
    return 0


if __name__ == "__main__":
    sys.exit(main())
