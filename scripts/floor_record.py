#!/usr/bin/env python3
"""Stamps a memory-floor result with which port and size produced it.

`memory_floor.sh` measures one number and knows nothing about what it was
measuring -- it takes a whole command, so the port is just argv to it. The
caller knows, so the caller says.

A missing or unreadable raw file is a result too: the floor search did not
finish, and the merged table should say that rather than drop the row.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--raw", type=Path, required=True)
    ap.add_argument("--port", required=True)
    ap.add_argument("--size", required=True)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()

    record: dict = {}
    try:
        record = json.loads(args.raw.read_text())
    except (OSError, json.JSONDecodeError):
        pass
    record.update({"port": args.port, "size": args.size})
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(record))
    print(record)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
