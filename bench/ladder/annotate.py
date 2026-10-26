#!/usr/bin/env python3
"""Echo a markdown file as GitHub `::notice` annotations.

Step summaries and artifacts are only readable through the web UI / blob
storage; annotations come back through the plain checks API
(`gh api repos/<o>/<r>/check-runs/<job>/annotations`), so a remote agent or
script can read the ladder table without downloading anything.
"""
import sys
from pathlib import Path

# Tables carry ✓/✗/⚠: write UTF-8 even where the console code page isn't.
sys.stdout.reconfigure(encoding="utf-8")
path = Path(sys.argv[1] if len(sys.argv) > 1 else "bench/ladder/out/table.md")
title = sys.argv[2] if len(sys.argv) > 2 else "ladder table"
if not path.exists():
    print(f"::warning title={title}::{path} not found")
    sys.exit(0)
chunks, cur = [], ""
for line in path.read_text(encoding="utf-8").splitlines():
    if cur and len(cur) + len(line) > 3500:
        chunks.append(cur)
        cur = ""
    cur += line + "\n"
if cur:
    chunks.append(cur)
for i, chunk in enumerate(chunks[:10], 1):  # GitHub keeps 10 notices per step
    enc = chunk.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::notice title={title} {i}/{len(chunks)}::{enc}")
