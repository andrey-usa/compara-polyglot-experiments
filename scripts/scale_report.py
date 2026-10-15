#!/usr/bin/env python3
"""Merges one JSON per port into the two tables the scale run exists to produce.

The ceiling run and the floor run are separate jobs on separate runners -- the
ladder is larger than one runner's disk, and a port that dies at 40m should not
take the others down with it -- so the merge happens here, from artifacts, once
they have all finished.

  python scripts/scale_report.py --ceiling results/ceiling-*/*.json \\
      --floor results/floor-*/*.json --out report.md

Two tables come out. The first is how far each port got and what stopped it;
the second is the smallest memory limit each one finishes a fixed size in, which
is a different question and often a different order.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from mdtable import table

ORDER = ["c", "cpp", "rust", "zig"]


def load(paths: list[str]) -> list[dict]:
    out: list[dict] = []
    for p in paths:
        try:
            data = json.loads(Path(p).read_text())
        except (OSError, json.JSONDecodeError):
            continue
        out.extend(data if isinstance(data, list) else [data])
    return out


def rank(port: str) -> int:
    return ORDER.index(port) if port in ORDER else len(ORDER)


def ceiling_tables(rows: list[dict]) -> str:
    """Every size every port reached, and then the ceiling on its own."""
    if not rows:
        return "_No ceiling results._\n"
    ports = sorted({r["port"] for r in rows}, key=rank)
    sizes: list[str] = []
    for r in rows:
        if r["size"] not in sizes:
            sizes.append(r["size"])
    sizes.sort(key=lambda s: next(x["rows"] for x in rows if x["size"] == s))

    by = {(r["port"], r["size"]): r for r in rows}
    out = ["### How far each port got\n",
           "**Read down a column for whether a size completed, not across for how",
           "long it took.** Every cell is its own job on its own hosted runner, and",
           "those runners are not the same machine -- so two cells differ by "
           "hardware",
           "as well as by size, and the seconds below are not a curve. Where a size",
           "matters more than the machine, `bench-ladder.yml` puts every port on one",
           "host per size, which is the table to compare times in.\n",
           "**failed** is that size failing and says nothing about the sizes above",
           "it -- those ran too, independently. The reason is under the table.\n"]
    grid = []
    for s in sizes:
        cells = [s]
        for p in ports:
            r = by.get((p, s))
            if r is None:
                cells.append("-")
            elif r.get("ok"):
                cells.append(f"{r['wall_s']:.2f}s")
            else:
                cells.append("**failed**")
        grid.append(cells)
    out += table(["Rows"] + ports, ["l"] + ["r"] * len(ports), grid)

    out += ["", "### The ceiling\n"]
    grid = []
    for p in ports:
        mine = [r for r in rows if r["port"] == p]
        ok = [r for r in mine if r.get("ok")]
        bad = [r for r in mine if not r.get("ok")]
        if not ok:
            grid.append([f"**{p}**", "none", "-", "-", "-",
                         bad[0]["why"] if bad else "no result"])
            continue
        best = max(ok, key=lambda r: r["rows"])
        # Which failure is the ceiling depends on where it sits. Rungs are run
        # independently, so a port can fail at 30m and still finish 60m -- and
        # then 30m is not what stopped it, it is a hole. Only a failure above
        # the largest completed size is a ceiling; one below it is an anomaly,
        # and worth saying out loud rather than quietly reporting as the limit.
        # The sequential ladder this replaced could not produce that state,
        # which is why this column used to be `bad[0]` and nothing more.
        above = sorted((r for r in bad if r["rows"] > best["rows"]), key=lambda r: r["rows"])
        below = sorted((r for r in bad if r["rows"] < best["rows"]), key=lambda r: r["rows"])
        stopped = f"{above[0]['why']} at {above[0]['size']}" if above else "ladder ran out, not the port"
        if below:
            stopped += " — but also failed at " + ", ".join(r["size"] for r in below) + \
                       ", under a size that passed"
        grid.append([f"**{p}**", f"**{best['size']}**", f"{best['input_mb']:,.0f} MB",
                     f"{best['wall_s']:.2f}s", f"{best['rss_mb']:,.0f} MB", stopped])
    out += table(["Port", "Largest completed", "Input at that size", "Wall", "Peak RSS",
                  "Stopped by"], ["l", "r", "r", "r", "r", "l"], grid)

    # The evidence for the warning above, and only when there is something to
    # show: if every cell drew the same CPU the table is one value repeated, and
    # if none of them recorded one there is nothing to say.
    seen = {r.get("cpu") for r in rows if r.get("cpu")}
    if len(seen) > 1:
        out += ["", "### Which machine each cell drew\n",
                "The reason the seconds above are not comparable, in full. These are",
                "hosted runners, and the fleet spans CPU generations.\n"]
        cpus = sorted(seen)
        short = {c: f"m{i + 1}" for i, c in enumerate(cpus)}
        by_cpu = {(r["port"], r["size"]): r.get("cpu") for r in rows}
        cells = []
        for s in sizes:
            cells.append([s] + [short.get(by_cpu.get((p, s)), "-") for p in ports])
        out += table(["Rows"] + ports, ["l"] + ["l"] * len(ports), cells)
        out += [""] + [f"- `{short[c]}` — {c}" for c in cpus]
    return "\n".join(out) + "\n"


def floor_table(rows: list[dict]) -> str:
    if not rows:
        return "\n_No memory-floor results._\n"
    rows = sorted(rows, key=lambda r: (r.get("floor_mb") or 1 << 30, rank(r.get("port", ""))))
    out = ["", "### The memory floor\n",
           "The smallest cgroup limit the same comparison finishes inside. Lower is",
           "better, and it is not the same ordering as speed. Peak RSS would not",
           "answer this: mapped pages are reclaimable, so that figure is whatever the",
           "kernel allowed, not what the engine needed.\n"]
    grid = []
    for r in rows:
        mb = r.get("floor_mb")
        grid.append([f"**{r.get('port','?')}**", str(r.get("size", "?")),
                     f"**{mb:,} MB**" if mb else "did not finish"])
    out += table(["Port", "Size compared", "Smallest limit that finishes"],
                 ["l", "l", "r"], grid)
    return "\n".join(out) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ceiling", nargs="*", default=[])
    ap.add_argument("--floor", nargs="*", default=[])
    ap.add_argument("--out", type=Path, default=Path("scale-report.md"))
    ap.add_argument("--title", default="Scale ceiling")
    args = ap.parse_args()

    body = (f"## {args.title}\n\n"
            + ceiling_tables(load(args.ceiling))
            + floor_table(load(args.floor)))
    args.out.write_text(body)
    print(body)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
