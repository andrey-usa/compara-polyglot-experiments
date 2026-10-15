#!/usr/bin/env python3
"""Do the generators still write the same bytes?

Four programs write this project's benchmark data -- one per port that has a
generator, plus the Python one -- and the whole point of having four is that
they agree down to the byte. A table comparing engines is only a table about
engines if every engine read the same file.

Nothing enforced that. `parity.yml` generates *one* dataset with `c/gen-data`
and checks that every reader agrees on the counts, which is a different claim;
`c/test.sh --with-ports` compares C against C++ and stops there. So the Rust
generator and the Python one had never been byte-compared to anything in CI, and
a real divergence sat in that gap: on ndjson, where a row's `value_date` has
been blanked, C and C++ write `null` and Rust writes `""`. Every reader treats
both as absent, so no answer ever changed and nothing failed.

This is the check that would have caught it. Known divergences are named below
rather than hidden, and a named one that stops differing fails too -- otherwise
the list rots into a place where fixed bugs go to be forgotten.

  python3 scripts/generator_parity.py --rows 200k

Parquet is not compared. The generators disagree about compression defaults and
therefore about filenames -- C writes `p_a.unc.parquet` under
`--compression none` where Rust writes `p_a.parquet` -- so a byte comparison
there is a question about flags rather than about the data. Text is also where
the hazard is: a Parquet file that had gone wrong could not be read back, and
the reader checks already cover that.
"""
from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mdtable import table  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent

# name -> (argv prefix, formats it can write). The reference is first.
GENERATORS = {
    "c":    ([str(ROOT / "c/gen-data")], ("csv", "json")),
    "cpp":  ([str(ROOT / "cpp/build/gen-data")], ("csv", "json")),
    "rust": ([str(ROOT / "rust/target/release/compara-gen-data")], ("csv", "json")),
    "py":   ([sys.executable, str(ROOT / "scripts/gen_data.py")], ("csv",)),
}
REFERENCE = "c"

# Divergences this project knows about and has not settled. Keyed by
# (format, generator, filename); the value is why it is here.
#
# An entry is not permission to differ for ever -- the check fails when a listed
# pair turns out to be identical, so settling one is what removes it.
KNOWN = {
    ("json", "rust", "p_b.ndjson"):
        "a blanked value_date: C and C++ write `null`, Rust writes `\"\"`. Both "
        "readers treat null and empty as absent, so no answer changes. Settling "
        "it means choosing which spelling is right and regenerating. "
        "FIXME (issue #181): decide, then delete this entry.",
}


def rows_arg(gen: str, rows: str) -> list[str]:
    """The Python generator wants a plain integer where the others take `200k`."""
    if gen != "py":
        return ["--rows", rows]
    n = rows.strip().lower()
    mult = {"k": 1_000, "m": 1_000_000}.get(n[-1:])
    return ["--rows", str(int(n[:-1]) * mult) if mult else n]


def generate(gen: str, fmt: str, rows: str, out: Path) -> bool:
    argv, _ = GENERATORS[gen]
    out.mkdir(parents=True, exist_ok=True)
    cmd = argv + rows_arg(gen, rows) + ["--out-dir", str(out), "--prefix", "p"]
    if fmt != "csv":
        cmd += ["--format", fmt]
    done = subprocess.run(cmd, capture_output=True, text=True)
    if done.returncode != 0:
        print(f"  !! {gen} could not write {fmt}: {done.stderr.strip().splitlines()[-1:]}")
    return done.returncode == 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rows", default="200k")
    args = ap.parse_args()

    have = [g for g, (argv, _) in GENERATORS.items() if Path(argv[-1]).exists()]
    missing = [g for g in GENERATORS if g not in have]
    if REFERENCE not in have:
        print(f"the reference generator ({REFERENCE}) is not built", file=sys.stderr)
        return 2
    if missing:
        # Named, not silent: a generator that is not built is a gap in this
        # check, and a gap nobody is told about is the reason it exists.
        print(f"not built, so not compared: {', '.join(missing)}\n")

    tmp = Path(tempfile.mkdtemp(prefix="genparity-"))
    grid, unexpected, fixed = [], [], []
    try:
        for fmt in ("csv", "json"):
            writers = [g for g in have if fmt in GENERATORS[g][1]]
            if len(writers) < 2:
                continue
            for g in writers:
                generate(g, fmt, args.rows, tmp / f"{g}-{fmt}")
            ref_dir = tmp / f"{REFERENCE}-{fmt}"
            for name in sorted(p.name for p in ref_dir.iterdir()):
                for g in writers:
                    if g == REFERENCE:
                        continue
                    mine = tmp / f"{g}-{fmt}" / name
                    same = mine.exists() and mine.read_bytes() == (ref_dir / name).read_bytes()
                    key = (fmt, g, name)
                    if same and key in KNOWN:
                        fixed.append(key)
                        verdict = "**identical — known entry is stale**"
                    elif same:
                        verdict = "identical"
                    elif key in KNOWN:
                        verdict = "differs (known)"
                    else:
                        unexpected.append(key)
                        verdict = "**DIFFERS**"
                    grid.append([fmt, name, f"{REFERENCE} vs {g}", verdict])
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print(f"### Generator bytes, {args.rows} rows\n")
    print("\n".join(table(["Format", "File", "Pair", "Result"],
                          ["l", "l", "l", "l"], grid)))

    if KNOWN:
        print("\nKnown and not settled:\n")
        for (fmt, g, name), why in KNOWN.items():
            print(f"- `{fmt}` {name}, {REFERENCE} vs {g} — {why}")

    if fixed:
        print("\nThese no longer differ, so the entry above is out of date. "
              "Delete it from KNOWN:")
        for k in fixed:
            print(f"  {k}")
    if unexpected:
        print("\nGenerators that should agree and do not:")
        for k in unexpected:
            print(f"  {k}")
    return 1 if (fixed or unexpected) else 0


if __name__ == "__main__":
    raise SystemExit(main())
