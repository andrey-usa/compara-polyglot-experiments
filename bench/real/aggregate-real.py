#!/usr/bin/env python3
"""Aggregate bench/real/real-episodes.jsonl: per-tool success, steps, tokens, cost."""
import json, sys
from collections import defaultdict

rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
by = defaultdict(list)
for r in rows:
    by[r["tool"]].append(r)
for tool, rs in sorted(by.items()):
    ok = sum(1 for r in rs if r.get("judge_success") is True)
    print(f"{tool}: episodes={len(rs)} judge_success={ok} "
          f"steps={sum(r['steps'] for r in rs)} tokens={sum(r['tokens'] for r in rs)} "
          f"cost=${sum(r['cost'] for r in rs):.4f} wall_s={sum(r['wall_s'] for r in rs):.1f} "
          f"errors={sum(1 for r in rs if r.get('error'))}")
    failed = [r["task"] for r in rs if r.get("judge_success") is not True]
    print(f"  not-success: {','.join(failed) if failed else '(none)'}")
