#!/usr/bin/env python3
"""Fixture generator for the compara-vs-contenders size ladder.

Writes, per size, A and B in both CSV and Parquet: B is A with planted
added/removed/changed rows. Schema is id,k2,amount,name,status, key (id,k2).

usage: python3 scripts/bench_sizes_gen.py --sizes 500,5000 --out data
"""
import argparse
import os

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.csv as pcsv
import pyarrow.parquet as pq

STAT = pa.array(["new", "open", "closed", "hold"])


def table(ids, chg_mask):
    ids = pa.array(ids, type=pa.int64())
    k2 = pc.cast(pc.bit_wise_and(ids, pa.scalar(7, pa.int64())), pa.int64())
    amt = pc.divide(pc.cast(pc.bit_wise_and(pc.multiply(ids, pa.scalar(37, pa.int64())),
                                            pa.scalar(131071, pa.int64())), pa.float64()), 100.0)
    nm = pc.binary_join_element_wise(pa.scalar("name_"), pc.cast(ids, pa.string()), "")
    st = pc.take(STAT, pc.bit_wise_and(ids, pa.scalar(3, pa.int64())))
    m = pa.array(chg_mask)
    which = pc.bit_wise_and(ids, pa.scalar(3, pa.int64()))
    amt = pc.if_else(pc.and_(m, pc.less(which, 2)), pc.add(amt, 1.0), amt)
    nm = pc.if_else(pc.and_(m, pc.equal(which, 2)),
                    pc.binary_join_element_wise(nm, pa.scalar("_x"), ""), nm)
    st = pc.if_else(pc.and_(m, pc.equal(which, 3)), pa.scalar("zzz"), st)
    return pa.table({"id": ids, "k2": k2, "amount": amt, "name": nm, "status": st})


def write_size(n, d):
    ch = max(5, n // 1000)
    rm = max(2, n // 10000)
    ad = max(2, n // 20000)
    rng = np.random.default_rng(42)
    pick = rng.permutation(n)[:ch + rm]
    changed, removed = np.sort(pick[:ch]), np.sort(pick[ch:])
    allids = np.arange(n, dtype=np.int64)
    bids = np.concatenate([np.setdiff1d(allids, removed),
                           np.arange(n, n + ad, dtype=np.int64)])
    for ids, chgset, base in ((allids, None, "A"), (bids, changed, "B")):
        perm = np.random.default_rng(7 if base == "A" else 8).permutation(len(ids))
        ids = ids[perm]
        isch = np.isin(ids, chgset) if chgset is not None else np.zeros(len(ids), bool)
        cw = pcsv.CSVWriter(f"{d}/{base}.csv", table(ids[:1], isch[:1]).schema)
        pw = pq.ParquetWriter(f"{d}/{base}.parquet", table(ids[:1], isch[:1]).schema,
                              compression="snappy")
        for s in range(0, len(ids), 1_000_000):
            t = table(ids[s:s + 1_000_000], isch[s:s + 1_000_000])
            cw.write_table(t)
            pw.write_table(t)
        cw.close()
        pw.close()
    print(f"N={n}: planted changed={ch} removed={rm} added={ad}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", required=True, help="comma-separated row counts")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    for s in [x.strip() for x in args.sizes.split(",") if x.strip()]:
        n = int(s)
        d = os.path.join(args.out, s)
        os.makedirs(d, exist_ok=True)
        write_size(n, d)


if __name__ == "__main__":
    main()
