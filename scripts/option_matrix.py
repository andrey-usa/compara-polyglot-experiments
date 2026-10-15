#!/usr/bin/env python3
"""Every port, every format, the same answer to every option combination.

    scripts/option_matrix.py                 # c,cpp,rust,zig on csv, ndjson and parquet
    scripts/option_matrix.py --ports c,zig   # a subset; the first is the reference
    scripts/option_matrix.py --bin zig=zig/zig-out-debug/bin/csvdiff   # a port's other build

`scripts/fuzz_diff.py` varies the *input* and keeps the options simple. This
varies the *options* and keeps the input simple: one generated pair per format
(from c/gen-data, so every format holds the same rows), and a fixed table of
invocations covering what the fuzzer never passes -- --compare, --ignore and
--key together, overlapping, and misspelled.

That is where the ports had drifted apart. Before it existed, on the same pair:
  * `-c amount,updated_at -i updated_at` gave 317 changed rows in C and Rust and
    19,980 in C++ and Zig, which compared the column they were told to ignore;
  * a misspelled `-i` on Parquet was refused by C and Rust and accepted by C++
    and Zig, which then compared the column it meant to drop.

Per invocation and format, every port must give the same exit status, and where
that status is 0 or 1 the same `counts` and `columns`, and a document that
passes scripts/contract.py. A refusal (exit 2) must also say which column or
file it is about, the one thing a user needs from it.

The normalisation flags -- --trim, --ignore-case, --empty-is-null, --tolerance
-- run on the ports that carry them (C carries all four on CSV/ndjson and
refuses each by name on Parquet, by design; see c/README.md), with Rust as the
reference. They run on the generated pairs and on
tests/fixtures/fold, a pair written to sit on the one place the ports are
allowed to differ: C++ and Zig fold case in ASCII only, and refuse a value
outside it where the fold decides the answer -- a key, or a compared value that
differs byte for byte. There they must refuse and name --ignore-case; anywhere
else on that pair they must answer, with Rust's counts. Before this, C++ refused
a row it only displayed, and only under --json, and Zig refused a value that had
not changed; nothing ran the flags, so nothing noticed.

Exit status: 0 all agree, 1 at least one disagreement, 2 usage.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import contract  # noqa: E402
from fuzz_diff import PORTS  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
GEN = ROOT / "c" / ("gen-data.exe" if os.name == "nt" else "gen-data")
KEY = "account_id,txn_id"

# (label, flags, the name a refusal must mention -- or None where it succeeds)
CASES: list[tuple[str, list[str], str | None]] = [
    ("defaults", ["-k", KEY], None),
    ("ignore one", ["-k", KEY, "-i", "updated_at"], None),
    ("ignore several", ["-k", KEY, "-i", "updated_at,fee,note"], None),
    ("compare explicit", ["-k", KEY, "-c", "amount,status"], None),
    ("compare repeats a name", ["-k", KEY, "-c", "amount,amount"], None),
    ("compare names a key", ["-k", KEY, "-c", "account_id,amount"], None),
    ("compare names an ignored column", ["-k", KEY, "-c", "amount,updated_at", "-i", "updated_at"], None),
    ("compare only ignored", ["-k", KEY, "-c", "updated_at", "-i", "updated_at"], None),
    ("ignore a key column", ["-k", KEY, "-i", "account_id"], None),
    ("single-column key", ["-k", "txn_id"], None),
    ("key order reversed", ["-k", "txn_id,account_id"], None),
    ("unknown key", ["-k", "no_such_key"], "no_such_key"),
    ("unknown key in a list", ["-k", "account_id,no_such_key"], "no_such_key"),
    ("unknown ignore", ["-k", KEY, "-i", "no_such_ignore"], "no_such_ignore"),
    ("unknown compare", ["-k", KEY, "-c", "amount,no_such_compare"], "no_such_compare"),
]

# The ports that carry the normalisation flags, the reference first: Rust folds
# Unicode, so its answer is the one the ASCII-only ports are held to.
NORMALISING_PORTS = ["rust", "c", "cpp", "zig"]
ASCII_FOLDERS = {"c", "cpp", "zig"}
# The ports whose Parquet path does not carry the flags yet. They must refuse by
# name, naming the first flag given, rather than compare the raw bytes.
PARQUET_REFUSERS = {"c"}
NORM_FLAGS = ("--trim", "--ignore-case", "--empty-is-null", "--tolerance")
ALL_FOUR = ["--trim", "--ignore-case", "--empty-is-null", "--tolerance", "0.5"]
NORMALISING = [["--trim"], ["--ignore-case"], ["--empty-is-null"], ["--tolerance", "0.5"], ALL_FOUR]
FOLD = ROOT / "tests" / "fixtures" / "fold"
FOLD_PAIRS = {
    "csv": (FOLD / "a.csv", FOLD / "b.csv"),
    "ndjson": (FOLD / "a.ndjson", FOLD / "b.ndjson"),
    "parquet, dictionary": (FOLD / "a_dict.parquet", FOLD / "b_dict.parquet"),
    "parquet, plain": (FOLD / "a_plain.parquet", FOLD / "b_plain.parquet"),
}
# (label, flags, whether the ASCII-only folders must refuse). See
# scripts/make_fold_fixtures.py for what each row of the pair is there for.
FOLD_CASES: list[tuple[str, list[str], bool]] = [
    ("--ignore-case, the value differing outside ASCII ignored", ["-k", "id", "--ignore-case", "-i", "note"], False),
    ("--ignore-case, a value differing outside ASCII", ["-k", "id", "--ignore-case"], True),
    ("--ignore-case, a key outside ASCII", ["-k", "id,name", "--ignore-case", "-i", "note"], True),
    ("--trim", ["-k", "id", "--trim"], False),
    ("--empty-is-null", ["-k", "id", "--empty-is-null"], False),
    ("--tolerance", ["-k", "id", "--tolerance", "0.5"], False),
    ("all four, the value differing outside ASCII ignored", ["-k", "id", *ALL_FOUR, "-i", "note"], False),
]


# What a checked build prints when it finds a memory error, and may print while
# still exiting with the status the run deserved: Zig's debug allocator logs a
# double free or a leak and carries on. A release build prints none of these.
MEMORY_ERRORS = ("error(DebugAllocator)", "AddressSanitizer", "LeakSanitizer", "runtime error:")


def run(port: str, a: Path, b: Path, flags: list[str], work: Path):
    """(exit status, contract slice, stderr, contract problems)."""
    doc = work / f"{port}.json"
    doc.unlink(missing_ok=True)
    proc = subprocess.run([str(PORTS[port]), "compare", str(a), str(b), *flags, "--json", str(doc)],
                          capture_output=True, timeout=120)
    got, problems = None, []
    if proc.returncode <= 1:
        try:
            d = json.loads(doc.read_text())
            got = {k: d.get(k) for k in ("counts", "columns")}
            problems = contract.check(d)
        except (OSError, ValueError) as err:
            problems = [f"no readable --json document: {err}"]
    err = proc.stderr.decode(errors="replace").strip()
    for line in err.splitlines():
        if any(m in line for m in MEMORY_ERRORS):
            problems.append(f"reported a memory error: {line.strip()}")
            break
    return proc.returncode, got, err, problems


def judge(results: dict, flags: list[str], must_name: str | None = None,
          refusers: dict[str, str] | None = None) -> list[str]:
    """What is wrong with one invocation's results, against the first port's.

    A port in `refusers` must refuse, naming the option it maps to, instead of agreeing."""
    refusers = refusers or {}
    ref_port = next((p for p in results if p not in refusers), next(iter(results)))
    ref = results[ref_port]
    problems = []
    for p, (code, got, err, contract_problems) in results.items():
        problems += [f"{p}: {c}" for c in contract_problems]
        first = err.splitlines()[0] if err else "(nothing)"
        if p in refusers:
            if code != 2:
                problems.append(f"{p} exits {code} on {' '.join(flags)}; it must refuse")
            elif refusers[p] not in err:
                problems.append(f"{p} refuses without naming {refusers[p]}: {first}")
            continue
        if code != ref[0]:
            problems.append(f"{p} exits {code}, {ref_port} exits {ref[0]}" + (f" ({first})" if code > 1 else ""))
        elif code <= 1 and got != ref[1]:
            problems.append(f"{p} counts/columns differ from {ref_port}: {got} vs {ref[1]}")
        if must_name and code == 2 and must_name not in err:
            problems.append(f"{p} refuses without naming {must_name!r}: {first}")
        if must_name and code != 2:
            problems.append(f"{p} accepts {' '.join(flags)} (exit {code}); it must refuse")
    return problems


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--ports", default="c,cpp,rust,zig")
    ap.add_argument("--rows", default="2k")
    ap.add_argument("--bin", action="append", default=[], metavar="PORT=PATH",
                    help="run this build for PORT instead of its release binary; repeatable. "
                         "The sanitizer job runs the matrix this way: it is the one runner that "
                         "reaches the refusals and the normalising paths, which the fuzzer never does")
    args = ap.parse_args()
    for spec in args.bin:
        port, sep, path = spec.partition("=")
        if not sep or port not in PORTS:
            print(f"--bin wants PORT=PATH with PORT one of {', '.join(PORTS)}: {spec!r}", file=sys.stderr)
            return 2
        PORTS[port] = Path(path).resolve()
    ports = [p for p in args.ports.split(",") if p]
    missing = [p for p in ports if p not in PORTS or not PORTS[p].exists()]
    if missing or len(ports) < 2 or not GEN.exists():
        print(f"need two or more built ports and c/gen-data; missing: {missing or 'gen-data'}", file=sys.stderr)
        return 2

    bad = 0
    total = 0
    with tempfile.TemporaryDirectory(prefix="option-matrix-") as tmp:
        work = Path(tmp)
        pairs = {}
        for fmt, ext in (("csv", "csv"), ("ndjson", "ndjson"), ("parquet", "unc.parquet")):
            subprocess.run([str(GEN), "--rows", args.rows, "--out-dir", str(work), "--prefix", "m",
                            "--format", fmt], check=True, capture_output=True)
            pairs[fmt] = (work / f"m_a.{ext}", work / f"m_b.{ext}")

        def norm_refusers(fmt: str, flags: list[str], fold_decides: bool) -> dict[str, str]:
            """Who must refuse this normalising case, and the option each must name."""
            out = {p: "--ignore-case" for p in ASCII_FOLDERS} if fold_decides else {}
            if "parquet" in fmt:
                first = next(f for f in flags if f in NORM_FLAGS)
                out |= {p: first for p in PARQUET_REFUSERS}
            return out

        # (format, label, a, b, flags, the ports in reference-first order, must_name, refusers)
        plan = [(fmt, label, a, b, flags, ports, must_name, {})
                for fmt, (a, b) in pairs.items() for label, flags, must_name in CASES]
        norm_ports = [p for p in NORMALISING_PORTS if p in ports]
        if len(norm_ports) >= 2:
            plan += [(fmt, " ".join(f), a, b, ["-k", KEY, *f], norm_ports, None, norm_refusers(fmt, f, False))
                     for fmt, (a, b) in pairs.items() for f in NORMALISING]
            plan += [(f"fold, {fmt}", label, a, b, flags, norm_ports, None, norm_refusers(fmt, flags, refuse))
                     for fmt, (a, b) in FOLD_PAIRS.items() for label, flags, refuse in FOLD_CASES]

        for fmt, label, a, b, flags, order, must_name, refusers in plan:
            total += 1
            results = {p: run(p, a, b, flags, work) for p in order}
            problems = judge(results, flags, must_name, refusers)
            status = "ok  " if not problems else "FAIL"
            print(f"{status} {fmt:8} {label}")
            for line in problems:
                print(f"       {line}")
                if os.environ.get("GITHUB_ACTIONS"):
                    print(f"::error::{fmt} / {label}: {line}")
            bad += bool(problems)
    print(f"{total - bad}/{total} agree across {', '.join(ports)}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
