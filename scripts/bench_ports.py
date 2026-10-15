#!/usr/bin/env python3
"""Every native port that reads the format, on one pair of files, interleaved.

The question this answers is narrow on purpose: given the same columnar design
and the same input, how much is left for the language and the toolchain? The
four ports here read Parquet through code written for this project rather than
through a library, so nothing in the table is measuring somebody else's reader.

Two things it is careful about, both of which produced wrong numbers first.

**Interleaved, not one port at a time.** This machine's speed drifts under the
runs themselves -- page cache fills, and the kernel's supply of free 2 MB pages
is picked over and replenished -- so a number taken now and one taken twenty
minutes ago are comparing machine states, not builds. Every port runs once per
round, in the same order, and the rounds are what repeat.

**Best, median and worst, not just best.** A port that is quick once and slow
twice is not quick, and the spread is where memory pressure shows up.

Peak RSS is `wait4`'s rusage for that exact child -- the kernel's own high-water
mark rather than a poll that can miss a spike. These engines map their inputs,
so resident pages include the files; the column that carries information is
`above`, which subtracts them.

A port that cannot read the pair it is given is left out by name rather than
counted as slow or as agreeing with everyone: on `main` the Rust and Zig ports
refuse newline-delimited JSON, and a table that quietly omitted them would read
as though they had not been asked.

Ports built from another checkout can be added with `CSVDIFF_PORTS_EXTRA`, a
JSON array of `[label, path, extra_flags]`. That is how a branch's port is
measured against the same rows on the same host in the same sitting, which is
the only way two builds can be compared at all -- and the counts gate below
applies to them exactly as it does to the built-in four.

    python scripts/bench_ports.py A.parquet B.parquet --repeats 5
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import time
from pathlib import Path

from bench_formats_ports import host
from mdtable import render

ROOT = Path(__file__).resolve().parent.parent
KEY = "account_id,txn_id"
IGNORE = "updated_at"


def ports() -> list[tuple[str, list[str], list[str]]]:
    """(label, argv prefix, extra flags). A port that is not built is skipped by
    name rather than dropped: a missing build is not the same result as a slow
    one."""
    # Only the report suppression. `--engine turbo` used to be here too, and it
    # is the one flag that retires the columnar Parquet reader -- see the note in
    # `bench_formats_ports.py`. On a 500k pair the two readers are 0.15s against
    # 0.43s and 91 MB against 263 MB above the input, for identical counts, so
    # every Rust Parquet number this script has produced was of the reader `auto`
    # does not pick.
    #
    # `--summary` rather than `-o /dev/null`, which only moved the write: the
    # render still ran, and so did everything the engine does to feed it. See
    # `bench_formats_ports.py` for the measurement.
    report = ["--summary"]
    out = []
    for label, path, flags in [
        ("C", ROOT / "c/csvdiff", []),
        ("C++", ROOT / "cpp/build/csvdiff", []),
        ("Rust", ROOT / "rust/target/release/compara", report),
        ("Zig", ROOT / "zig/zig-out/bin/csvdiff", []),
    ]:
        if path.exists():
            out.append((label, [str(path)], flags))
        else:
            print(f"  {label:5s} -- not built ({path})", file=sys.stderr)
    out += extras()
    return out


def gate_flags(flags: list[str]) -> list[str]:
    """The timed flags, with `--summary` traded back for a discarded report.

    The counts gate needs the JSON document, and `--summary` refuses an output
    flag rather than guessing which of the two was meant. The gate is untimed,
    so what it costs the port that renders one reaches no table.
    """
    if "--summary" not in flags:
        return flags
    return [f for f in flags if f != "--summary"] + ["-o", "/dev/null"]


def extras() -> list[tuple[str, list[str], list[str]]]:
    """Ports from `CSVDIFF_PORTS_EXTRA`: JSON `[[label, path, [flags...]], ...]`.

    A malformed value is an error rather than a silent empty list -- the whole
    reason to pass this is that a column is expected in the table, and a
    benchmark that quietly measured one fewer port than asked for is worse than
    one that refuses to start."""
    raw = os.environ.get("CSVDIFF_PORTS_EXTRA", "").strip()
    if not raw:
        return []
    try:
        spec = json.loads(raw)
        items = [(str(label), [str(p)], [str(f) for f in fl]) for label, p, fl in spec]
    except (ValueError, TypeError) as exc:
        raise SystemExit(f"CSVDIFF_PORTS_EXTRA is not [[label, path, [flags]]]: {exc}")
    out = []
    for label, prefix, flags in items:
        # Resolved against the repository root like the built-in four, so the
        # variable says the same thing wherever it is set from.
        prefix = [str(ROOT / prefix[0])]
        if Path(prefix[0]).exists():
            out.append((label, prefix, flags))
        else:
            print(f"  {label:5s} -- not built ({prefix[0]})", file=sys.stderr)
    return out


def slug(label: str) -> str:
    """A label is a table heading, not a filename; this makes it one."""
    return "".join(c if c.isalnum() else "_" for c in label)


def warm(*paths: str) -> None:
    """Reads the inputs once so the first timed run is not also a disk test."""
    for p in paths:
        with open(p, "rb") as fh:
            while fh.read(1 << 22):
                pass


def run(argv: list[str], err: str) -> tuple[float, float, float, int]:
    """Wall, peak RSS in MB, CPU seconds and exit code, for exactly this child."""
    started = time.monotonic()
    pid = os.fork()
    if pid == 0:
        o = os.open(os.devnull, os.O_WRONLY)
        e = os.open(err, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
        os.dup2(o, 1)
        os.dup2(e, 2)
        os.execv(argv[0], argv)
        os._exit(127)
    _, status, usage = os.wait4(pid, 0)
    return (time.monotonic() - started, usage.ru_maxrss / 1024,
            usage.ru_utime + usage.ru_stime, os.waitstatus_to_exitcode(status))


def counts(path: str) -> dict | None:
    """The counts block, however a port spells the rest of its report."""
    try:
        with open(path) as fh:
            return json.load(fh).get("counts")
    except (OSError, ValueError):
        return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("a")
    ap.add_argument("b")
    ap.add_argument("--repeats", type=int, default=5)
    ap.add_argument("--key", default=KEY)
    ap.add_argument("--ignore", default=IGNORE)
    ap.add_argument("--tmp", default="/tmp")
    args = ap.parse_args()

    builds = ports()
    if not builds:
        raise SystemExit("no ports are built")
    warm(args.a, args.b)
    size = (os.path.getsize(args.a) + os.path.getsize(args.b)) / 2**20

    # One run each first, to find out who can read this pair at all -- and, now,
    # to collect the counts the gate below compares. The timed rounds do not
    # pass `--json`.
    #
    # Passing it to every port was timing four different tasks. Only the C++
    # port emits row samples; C, Rust and Zig write `counts` and `columns` and
    # stop, and C has no flag to do otherwise because it has no such feature.
    # On a 400k pair `--json` costs C 39,250 instructions (0.0%) and C++ 808
    # million (44%): it turns on a second full random-probed pass over B, then
    # materialises, sorts and writes every sampled row. On a 4M pair at four
    # threads, warmed and interleaved, C++ is 1.81x C on the task all four
    # perform and 2.25x once C++ alone is asked for a report.
    #
    # See bench_formats_ports.py for the same change and the same reasoning.
    reads: list[tuple[str, list[str], list[str]]] = []
    answers: dict[str, dict | None] = {}
    for label, prefix, flags in builds:
        out = f"{args.tmp}/ports_{slug(label)}.json"
        _, _, _, code = run(prefix + ["compare", args.a, args.b, "-k", args.key,
                                      "-i", args.ignore, "--json", out] + gate_flags(flags),
                            f"{args.tmp}/ports_err.txt")
        if code in (0, 1) and counts(out) is not None:
            reads.append((label, prefix, flags))
            answers[label] = counts(out)
        else:
            why = open(f"{args.tmp}/ports_err.txt").read().strip().splitlines()
            print(f"  {label:5s} -- does not read this pair"
                  f"{': ' + why[0][:90] if why else ''}", file=sys.stderr)
    if not reads:
        raise SystemExit("no port read the pair")
    builds = reads

    times: dict[str, list[tuple[float, float, float]]] = {label: [] for label, _, _ in builds}
    broken: dict[str, str] = {}
    # The starting port rotates between rounds. With a fixed order someone is
    # always first into a cold cache and someone always last, and position
    # quietly becomes part of every port's number.
    for rnd in range(args.repeats):
        turn = rnd % len(builds)
        for label, prefix, flags in builds[turn:] + builds[:turn]:
            if label in broken:
                continue
            secs, rss, cpu, code = run(
                prefix + ["compare", args.a, args.b, "-k", args.key, "-i", args.ignore]
                + flags, f"{args.tmp}/ports_err.txt")
            if code not in (0, 1):
                # One port failing used to end the whole benchmark, which meant
                # every port after it in the list produced no number at all --
                # and the ones at the back were never even attempted. A failure
                # is still a failure: it is reported by name and the run exits
                # nonzero below. It just no longer takes the other ports'
                # measurements down with it.
                why = open(f"{args.tmp}/ports_err.txt").read().strip()[:200]
                print(f"  {label:5s} FAILED ({code}): {why}", file=sys.stderr)
                broken[label] = f"exit {code}"
                times[label].clear()
                answers.pop(label, None)
                continue
            times[label].append((secs, rss, cpu))

    # The CPU, on the line above the table. This harness runs every format in
    # one job, so unlike the ladder its rows really are one machine -- but the
    # table is still only comparable with another table from the same CPU, and
    # on a hosted fleet that is not the same thing as the same runner label.
    h = host()
    print(f"\nhost: {h['key']}"
          + (f"  (runner {h['runner']})" if h["runner"] else ""))
    print(f"input {size:,.0f} MB total, {args.repeats} interleaved runs each\n")
    grid = []
    for label, _, _ in builds:
        if not times[label]:
            grid.append([label, "-", "-", "-", "-", "-", "-"])
            continue
        ts = sorted(t for t, _, _ in times[label])
        rss = max(r for _, r, _ in times[label])
        cpu = min(c for _, _, c in times[label])
        grid.append([label, f"{ts[0]:.2f}s", f"{statistics.median(ts):.2f}s",
                     f"{ts[-1]:.2f}s", f"{cpu:.1f}s", f"{rss:,.0f} MB",
                     f"{rss - size:,.0f} MB"])
    print(render(["Port", "Best", "Median", "Worst", "CPU", "Peak RSS", "Above the input"],
                 ["l", "r", "r", "r", "r", "r", "r"], grid), end="")

    # The correctness gate. Ports that disagree about how many rows changed mean
    # a bug in one of them, not an interesting benchmark, so say which.
    have = {label: a for label, a in answers.items() if a}
    if len(have) > 1:
        first = next(iter(have.values()))
        odd = [label for label, a in have.items() if a != first]
        print(f"\ncounts agree across {len(have)} ports: {not odd}")
        if odd:
            print(f"  disagreeing: {', '.join(odd)}")
            print(f"  {json.dumps(first, sort_keys=True)}")
            for label in odd:
                print(f"  {label}: {json.dumps(have[label], sort_keys=True)}")
            return 1
        print(f"  {json.dumps(first, sort_keys=True)}")
    if broken:
        print("\nfailed: " + ", ".join(f"{name} ({why})" for name, why in broken.items()),
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
