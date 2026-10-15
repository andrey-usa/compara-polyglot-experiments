#!/usr/bin/env python3
"""Check that every relative link in the repository's Markdown resolves.

    scripts/check_links.py              # every *.md in the repository
    scripts/check_links.py README.md    # just these files

A link `[text](path)` or `[text](path#anchor)` must point at a file or directory
that exists, and an anchor must match a heading in the target file using
GitHub's rule (lowercase, punctuation dropped, spaces to hyphens, a repeated
heading getting -1, -2, ...). External links (http, https, mailto) are not
fetched. Fenced code blocks and inline code are ignored, since a command in
one is not a link.

The README's promise is that every command runs from a fresh clone; this holds
its references to the same standard. It was written when BENCHMARKS.md was split
into docs/benchmarks/, which moved the anchors three README links pointed at.

Exit status: 0 all links resolve, 1 at least one does not.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SKIP_DIRS = {".git", "target", "node_modules", "zig-out", ".zig-cache", "build", "data", "bench", "out"}
LINK = re.compile(r"(?<!\!)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
FENCE = re.compile(r"^\s*(```|~~~)")


def slug(heading: str) -> str:
    text = heading.strip().lower()
    return re.sub(r"[^\w\- ]", "", text).replace(" ", "-")


_anchor_cache: dict[Path, set[str]] = {}


def anchors(path: Path) -> set[str]:
    if path in _anchor_cache:
        return _anchor_cache[path]
    seen: dict[str, int] = {}
    out: set[str] = set()
    fenced = False
    for line in path.read_text(errors="replace").split("\n"):
        if FENCE.match(line):
            fenced = not fenced
            continue
        if fenced:
            continue
        m = re.match(r"^#{1,6}\s+(.*?)\s*#*\s*$", line)
        if not m:
            continue
        # Links and code spans inside a heading contribute their text only.
        text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", m.group(1)).replace("`", "")
        base = slug(text)
        n = seen.get(base, 0)
        seen[base] = n + 1
        out.add(base if n == 0 else f"{base}-{n}")
    _anchor_cache[path] = out
    return out


def links_in(path: Path):
    fenced = False
    for lineno, line in enumerate(path.read_text(errors="replace").split("\n"), 1):
        if FENCE.match(line):
            fenced = not fenced
            continue
        if fenced:
            continue
        stripped = re.sub(r"`[^`]*`", "", line)
        for m in LINK.finditer(stripped):
            yield lineno, m.group(1)


def check(path: Path) -> list[str]:
    problems = []
    for lineno, target in links_in(path):
        if re.match(r"^[a-z][a-z0-9+.\-]*:", target, re.I) or target.startswith("//"):
            continue  # http, https, mailto, ...
        rel, _, frag = target.partition("#")
        dest = path if not rel else (path.parent / rel).resolve()
        where = f"{path.relative_to(ROOT)}:{lineno}"
        if rel and not dest.exists():
            problems.append(f"{where}: {target}: no such file")
        elif frag and dest.is_file() and dest.suffix == ".md" and frag.lower() not in anchors(dest):
            problems.append(f"{where}: {target}: no heading '#{frag}' in {dest.relative_to(ROOT)}")
    return problems


def main(argv: list[str]) -> int:
    if argv:
        files = [Path(a).resolve() for a in argv]
    else:
        files = [p for p in ROOT.rglob("*.md") if not SKIP_DIRS & set(p.relative_to(ROOT).parts)]
    problems = []
    for f in sorted(files):
        problems += check(f)
    for p in problems:
        print(f"::error::{p}" if "GITHUB_ACTIONS" in __import__("os").environ else p)
    print(f"{len(files)} files, {len(problems)} broken links")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
