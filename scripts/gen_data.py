#!/usr/bin/env python3
"""Generate a deterministic pair of CSV files for benchmarking and CI.

Both files have 20 columns and share the composite key (account_id, txn_id).
File B is file A with a controlled amount of drift applied:

  status        changes on ~3.0% of rows
  amount        changes on ~1.5% of rows
  balance       changes on ~1.5% of rows
  value_date    blanked  on ~0.3% of rows
  updated_at    changes on every row (so `--ignore updated_at` has something to do)
  removed       ~0.10% of rows are absent from B
  added         ~0.10% extra rows only in B
  duplicate key ~0.01% of rows are emitted twice in B, and a few in A

Usage:
  python scripts/gen_data.py --rows 1000000 --out-dir data

The same splitmix hash and the same drift recipe as the TypeScript, Java, Go and
Rust generators, so the files come out byte for byte identical and a benchmark
number from any of them is directly comparable. Money is carried in integer
cents and the drift is applied to those integers, never to a float, so no
language's rounding rule can enter into it.
"""
from __future__ import annotations

import argparse
import os
import time

COLUMNS = ["account_id", "txn_id", "posting_date", "value_date", "currency", "amount", "fee",
           "balance", "status", "channel", "region", "branch_code", "product_code", "counterparty",
           "quantity", "rate", "category", "risk_flag", "note", "updated_at"]

STATUS = ["posted", "pending", "settled", "reversed"]
CHANNEL = ["branch", "online", "mobile", "atm", "wire"]
REGION = ["EMEA", "NA", "APAC", "LATAM"]
CURRENCY = ["USD", "EUR", "GBP", "JPY"]
CATEGORY = ["retail", "corporate", "treasury", "cards", "loans"]

# Drift buckets, expressed against a 0..9999 hash bucket per row.
CHG_STATUS, CHG_AMOUNT, CHG_BALANCE, CHG_VALUE_DATE = 300, 150, 150, 30   # 3%, 1.5%, 1.5%, 0.3%
REMOVED_MOD, ADDED_RATIO, DUP_MOD = 1000, 1000, 10000


def generate(rows: int, a_path: str, b_path: str, seed: int) -> None:
    """Write both files in a single pass."""
    def h(i: int, salt: int) -> int:                       # splitmix-style, deterministic
        x = (i * 31 + salt + seed) & 0xFFFFFFFFFFFFFFFF
        x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & 0xFFFFFFFFFFFFFFFF
        x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & 0xFFFFFFFFFFFFFFFF
        return x ^ (x >> 31)

    from datetime import date, timedelta
    d0 = date(2026, 1, 1)
    day = [(d0 + timedelta(days=n)).isoformat() for n in range(240)]

    def money(cents: int) -> str:
        """An amount held in cents, as a two-decimal number."""
        return f"{'-' if cents < 0 else ''}{abs(cents) // 100}.{abs(cents) % 100:02d}"

    def row(i: int, b: bool) -> list[str]:
        # Money is carried in integer cents and the drift is applied to those
        # integers, never to a float. Every implementation of this generator then
        # produces the same digits without depending on its language's rounding
        # rule -- which is what makes the five sets of files byte-identical.
        bucket = h(i, 0) % 10000
        amount_cents = (h(i, 21) % 900000000) - 100000000
        balance_cents = h(i, 31) % 2000000000
        status = STATUS[h(i, 11) % len(STATUS)]
        value_date = day[h(i, 41) % 240]
        if b:
            if bucket < CHG_STATUS:
                status = STATUS[(h(i, 11) + 1) % len(STATUS)]
            elif bucket < CHG_STATUS + CHG_AMOUNT:
                amount_cents += 1234
            elif bucket < CHG_STATUS + CHG_AMOUNT + CHG_BALANCE:
                balance_cents = (balance_cents * 101 + 50) // 100   # +1%, half up
            if bucket < CHG_VALUE_DATE:
                value_date = ""
        return [f"ACC-{(i * 7919) % 250000:08d}", f"TXN-{i:011d}", day[h(i, 1) % 240], value_date,
                CURRENCY[h(i, 51) % 4], money(amount_cents), money(h(i, 61) % 5000),
                money(balance_cents), status, CHANNEL[h(i, 71) % 5], REGION[h(i, 81) % 4],
                f"BR{(h(i, 91) % 900) + 100:04d}", f"P{h(i, 101) % 5000:05d}",
                f"CP-{h(i, 111) % 90000:06d}", str((h(i, 121) % 500) + 1),
                f"0.{h(i, 131) % 1200:04d}", CATEGORY[h(i, 141) % 5],
                "Y" if h(i, 151) % 20 == 0 else "N", f"batch {i % 997 + 1} line {i % 53 + 1}",
                "2026-09-01 02:15:00" if b else "2026-08-01 02:15:00"]

    header = ",".join(COLUMNS) + "\n"
    dup_extra, added = max(1, rows // DUP_MOD), max(1, rows // ADDED_RATIO)
    with open(a_path, "w", newline="", buffering=1 << 20) as fa, \
         open(b_path, "w", newline="", buffering=1 << 20) as fb:
        fa.write(header), fb.write(header)
        ba, bb = [], []
        for i in range(rows):
            ba.append(",".join(row(i, False)))
            if i % REMOVED_MOD != 7:
                bb.append(",".join(row(i, True)))
            if i % DUP_MOD == 3 and i < rows // 2:
                bb.append(",".join(row(i, True)))
            if len(ba) >= 20000:
                fa.write("\n".join(ba) + "\n"), fb.write("\n".join(bb) + "\n")
                ba, bb = [], []
        for i in range(dup_extra):
            ba.append(",".join(row(i, False)))
        for i in range(rows, rows + added):
            bb.append(",".join(row(i, True)))
        fa.write("\n".join(ba) + "\n"), fb.write("\n".join(bb) + "\n")


def parse_rows(s: str) -> int:
    s = s.strip().lower().replace("_", "").replace(",", "")
    mult = {"k": 1_000, "m": 1_000_000, "g": 1_000_000_000}
    return int(float(s[:-1]) * mult[s[-1]]) if s and s[-1] in mult else int(s)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rows", "-n", default="10k", help="Rows in file A: 10000, 10k, 1m, 10m")
    ap.add_argument("--out-dir", "-o", default="data")
    ap.add_argument("--prefix", default=None, help="File name prefix (default: rows label)")
    ap.add_argument("--seed", type=int, default=7)
    ns = ap.parse_args()

    rows = parse_rows(ns.rows)
    os.makedirs(ns.out_dir, exist_ok=True)
    prefix = ns.prefix or ns.rows.lower()
    a = os.path.join(ns.out_dir, f"{prefix}_a.csv")
    b = os.path.join(ns.out_dir, f"{prefix}_b.csv")

    t0 = time.perf_counter()
    generate(rows, a, b, ns.seed)
    dt = time.perf_counter() - t0
    print(f"python: {rows:,} rows x {len(COLUMNS)} columns in {dt:.1f}s")
    for p in (a, b):
        print(f"  {p}  {os.path.getsize(p) / 1e6:.1f} MB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
