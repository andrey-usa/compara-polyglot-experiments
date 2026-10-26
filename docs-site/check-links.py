#!/usr/bin/env python3
"""Internal link checker for a built static site directory.

Usage: python3 check-links.py <site-dir> [--allow FILE]

Checks every href in every *.html file: same-site links must resolve to a
file in the site dir, and any #fragment must match an id/name in the target
page. External links are counted, not fetched (offline-safe); mailto: and
javascript: links are skipped. Prints a summary and exits non-zero when an
internal link is broken. Links pointing outside the four built pages but
inside the source repo (e.g. README links to CONTRIBUTING.md) are reported
separately as out-of-scope, not broken.

--allow FILE names one regex per line; a broken link matching any pattern
is reported as allowed (known source-repo links, see link-baseline.txt)
instead of broken, so CI fails only on new breaks.
"""
import re
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlparse

HREF_RE = re.compile(r'href="([^"]+)"')


class IdParser(HTMLParser):
    def __init__(self):
        super().__init__()
        self.ids = set()

    def handle_starttag(self, tag, attrs):
        for k, v in attrs:
            if k in ("id", "name"):
                self.ids.add(v)


def page_ids(path: Path) -> set:
    p = IdParser()
    try:
        p.feed(path.read_text(encoding="utf-8", errors="replace"))
    except OSError:
        pass
    return p.ids


def main() -> int:
    args = sys.argv[1:]
    allow_pats = []
    if "--allow" in args:
        i = args.index("--allow")
        allow_pats = [re.compile(line.strip()) for line in Path(args[i + 1]).read_text().splitlines()
                      if line.strip() and not line.startswith("#")]
        args = args[:i] + args[i + 2:]
    if len(args) != 1:
        print("usage: check-links.py <site-dir> [--allow FILE]", file=sys.stderr)
        return 2
    root = Path(args[0]).resolve()
    pages = sorted(root.rglob("*.html"))
    ids_cache = {}
    broken, out_of_scope, checked, external = [], [], 0, set()
    for page in pages:
        text = page.read_text(encoding="utf-8", errors="replace")
        for href in HREF_RE.findall(text):
            parsed = urlparse(href)
            if parsed.scheme in ("http", "https"):
                external.add(href.split("#")[0])
                continue
            if parsed.scheme in ("mailto", "javascript", "tel", "") and not href:
                continue
            if parsed.scheme:
                continue
            path_part = unquote(parsed.path)
            if not path_part:
                target, frag = page, parsed.fragment
            else:
                cand = (page.parent / path_part).resolve() if not path_part.startswith("/") \
                    else (root / path_part.lstrip("/")).resolve()
                if cand.is_dir():
                    cand = cand / "index.html"
                target, frag = cand, parsed.fragment
            try:
                target.relative_to(root)
            except ValueError:
                out_of_scope.append(f"{page.relative_to(root)} -> {href}")
                continue
            checked += 1
            if not target.exists():
                broken.append(f"{page.relative_to(root)} -> {href}")
                continue
            if frag:
                if target not in ids_cache:
                    ids_cache[target] = page_ids(target)
                if frag not in ids_cache[target]:
                    broken.append(f"{page.relative_to(root)} -> {href} (missing anchor #{frag})")
    print(f"pages: {len(pages)}")
    print(f"internal links checked: {checked}")
    print(f"external links (counted, not fetched): {len(external)}")
    print(f"out-of-scope links (target outside site dir): {len(out_of_scope)}")
    for line in sorted(set(out_of_scope)):
        print(f"  out-of-scope: {line}")
    allowed = [b for b in broken if any(p.search(b) for p in allow_pats)]
    broken = [b for b in broken if not any(p.search(b) for p in allow_pats)]
    print(f"allowed broken links (known source-repo links): {len(allowed)}")
    for line in sorted(set(allowed)):
        print(f"  allowed: {line}")
    print(f"broken internal links: {len(broken)}")
    for line in sorted(set(broken)):
        print(f"  BROKEN: {line}")
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main())
