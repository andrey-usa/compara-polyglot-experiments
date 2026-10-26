#!/usr/bin/env python3
"""Copy the shared navigera docs into each generator's source layout.

Source of truth: docs-site/content/*.md, copied verbatim from the navigera
repo (README.md, docs/navigera.md, docs/multi-model.md,
skills/navigera/SKILL.md). Each generator gets the same four pages, in the
same order, with only the frontmatter/layout adaptation that generator
requires. Run from docs-site/:  python3 sync-content.py

Page order (sidebar/nav everywhere):
  1. Overview        <- README.md
  2. Reference       <- navigera.md
  3. Multi-model     <- multi-model.md
  4. Agent skill     <- SKILL.md
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CONTENT = ROOT / "content"

# (source file, page title, slug/stem per generator)
PAGES = [
    ("README.md", "Overview"),
    ("navigera.md", "Reference"),
    ("multi-model.md", "Multi-model"),
    ("skill.md", "Agent skill"),
]

FRONTMATTER_RE = re.compile(r"\A---\n.*?\n---\n", re.DOTALL)


def first_h1(text: str) -> str:
    body = FRONTMATTER_RE.sub("", text, count=1)
    for line in body.splitlines():
        if line.startswith("# "):
            return line[2:].strip().strip("`")
    return ""


def strip_frontmatter(text: str) -> str:
    return FRONTMATTER_RE.sub("", text, count=1)


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def main() -> int:
    # --- Starlight (Astro): src/content/docs/*.md with frontmatter ---
    star = ROOT / "starlight" / "src" / "content" / "docs"
    star_files = ["index.md", "reference.md", "multi-model.md", "skill.md"]
    for (src, title), dest in zip(PAGES, star_files):
        text = (CONTENT / src).read_text(encoding="utf-8")
        body = strip_frontmatter(text)
        h1 = first_h1(text) or title
        write(star / dest, f"---\ntitle: {title}\ndescription: {h1}\n---\n{body}")

    # --- MkDocs Material: docs/*.md, verbatim (frontmatter tolerated) ---
    mk = ROOT / "mkdocs" / "docs"
    mk_files = ["index.md", "reference.md", "multi-model.md", "skill.md"]
    for (src, _), dest in zip(PAGES, mk_files):
        write(mk / dest, (CONTENT / src).read_text(encoding="utf-8"))

    # --- Zensical: docs/*.md, verbatim (same layout as MkDocs) ---
    zen = ROOT / "zensical" / "docs"
    zen_files = ["index.md", "reference.md", "multi-model.md", "skill.md"]
    for (src, _), dest in zip(PAGES, zen_files):
        write(zen / dest, (CONTENT / src).read_text(encoding="utf-8"))

    # --- mdBook: src/*.md + SUMMARY.md, frontmatter stripped ---
    mb = ROOT / "mdbook" / "src"
    mb_files = ["overview.md", "reference.md", "multi-model.md", "skill.md"]
    summary = ["# Summary", ""]
    for (src, title), dest in zip(PAGES, mb_files):
        text = strip_frontmatter((CONTENT / src).read_text(encoding="utf-8"))
        write(mb / dest, text)
        summary.append(f"- [{title}](./{dest})")
    summary.append("")
    write(mb / "SUMMARY.md", "\n".join(summary))
    print("synced 4 pages x 4 generators")
    return 0


if __name__ == "__main__":
    sys.exit(main())
