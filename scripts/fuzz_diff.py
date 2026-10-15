#!/usr/bin/env python3
"""Differential fuzzer: do the builds give the same answer on inputs nobody wrote by hand?

    # Port against port -- the conformance gate. The first port named is the reference.
    scripts/fuzz_diff.py --seeds 1-300 --ports c,cpp,rust,zig

    # Candidate against a reference build -- "is the output of this change identical to main's?"
    scripts/fuzz_diff.py --seeds 1-1000 --ref /tmp/main/csvdiff --cand ./build/csvdiff [--cand ...]

The benchmark data is uniform ASCII, and the two wrong answers this project has
shipped needed a key in the last eight bytes of a file and a key outside ASCII.
Nothing generated for speed can find those, so this generates the opposite:
small files (1-400 rows) full of the shapes that break byte-level parsers --
quoted fields holding the delimiter, doubled quotes, a newline inside quotes,
empty values, CRLF, ragged rows, non-ASCII, duplicate keys, composite keys, and
ndjson with shuffled and repeated-style separators.

Every seed is deterministic, and a failure prints the seed, the flags and the
build, so `--seeds N-N` replays it. Each case runs at 1 and 3 threads with the
last column ignored, a middle column ignored, and nothing ignored -- the guard
column moves with `-i`, and several optimisations key on where it sits.

Each seed also makes a second, normalising case: a pair whose differences are
the ones --trim, --ignore-case, --empty-is-null and --tolerance forgive (case,
padding, a blank against an empty cell, a small numeric nudge) and some they do
not, run with a random mix of those flags. The ports that carry the flags --
Rust, C++ and Zig; C refuses them as unknown options -- are held to Rust's
answer. C++ and Zig fold case in ASCII only, so under --ignore-case they must
refuse, naming the flag, exactly where the fold would decide the answer: a key
outside ASCII, or a compared value outside ASCII that differs byte for byte.
The pair is simple enough that the fuzzer works that out itself, and holds
them to it both ways. The flags were where the ports last disagreed, and the
parser-breaking cases above never passed one.

What is compared per run:
  * the exit status;
  * `counts` and `columns` from `--json` (the result contract);
  * in --cand mode, stderr as well, because a build against itself has no
    excuse for a different message.
Every document is also checked against docs/contract.schema.json plus the
arithmetic in scripts/contract.py, so a port that is wrong *with* the reference
still fails.

Exit status: 0 no differences, 1 at least one, 2 usage.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import contract  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
PORTS = {
    "c": ROOT / "c" / "csvdiff",
    "cpp": ROOT / "cpp" / "build" / "csvdiff",
    "rust": ROOT / "rust" / "target" / "release" / "compara",
    "zig": ROOT / "zig" / "zig-out" / "bin" / "csvdiff",
}
THREADS = (1, 3)
ALPHABET = "abcxyz0123456789 "
LENGTHS = (0, 1, 3, 7, 9, 15, 31, 33, 40)


# --- generators ---------------------------------------------------------------


def _csv_value(r: random.Random) -> str:
    k = r.random()
    s = "".join(r.choice(ALPHABET) for _ in range(r.choice(LENGTHS)))
    if k < 0.15:
        return '"' + s.replace('"', '""') + ',' + '"'  # the delimiter inside quotes
    if k < 0.22:
        return '"' + s + "\n" + s + '"'  # a newline inside quotes
    if k < 0.28:
        return '"' + s + '""' + s + '"'  # a doubled quote
    if k < 0.32:
        return ""
    if k < 0.36:
        return s + r.choice(["é", "☃", "ß", "İ"])  # multi-byte, some that change length when folded
    return s


def _tables(r: random.Random, ncol: int, n: int, value):
    """Rows for A and a B derived from it: some dropped, some edited, some new, some repeated."""
    a = [[f"k{i}"] + [value() for _ in range(ncol - 1)] for i in range(n)]
    b = [list(row) for row in a if r.random() > 0.05]
    for row in b:
        if r.random() < 0.3:
            row[r.randint(1, ncol - 1)] = value()
    for i in range(r.randint(0, 5)):
        b.append([f"n{i}"] + [value() for _ in range(ncol - 1)])
    if r.random() < 0.3 and a:  # duplicate keys are first-class: the first occurrence joins
        for _ in range(r.randint(1, 3)):
            a.insert(r.randrange(len(a) + 1), list(r.choice(a)))
    if r.random() < 0.3 and b:
        for _ in range(r.randint(1, 3)):
            b.insert(r.randrange(len(b) + 1), list(r.choice(b)))
    return a, b


def gen_csv(r: random.Random, d: Path, ragged: bool) -> tuple[Path, Path, list[str]]:
    ncol = r.randint(3, 9)
    a, b = _tables(r, ncol, r.randint(1, 400), lambda: _csv_value(r))
    eol = "\r\n" if r.random() < 0.3 else "\n"
    header = ["id"] + [f"c{i}" for i in range(1, ncol)]

    def write(path: Path, rows: list[list[str]]) -> None:
        with open(path, "w", newline="", encoding="utf-8") as f:
            f.write(",".join(header) + eol)
            for row in rows:
                if ragged:  # short rows, and rows with trailing extras
                    row = row[: r.randint(1, len(row))] if r.random() < 0.2 else row + ["x"] * r.randint(0, 3)
                f.write(",".join(row) + eol)

    write(d / "a.csv", a)
    write(d / "b.csv", b)
    return d / "a.csv", d / "b.csv", header


def gen_ndjson(r: random.Random, d: Path) -> tuple[Path, Path, list[str]]:
    ncol = r.randint(2, 8)

    def value():
        k = r.random()
        s = "".join(r.choice("abcxyz0123 ,:{}[]") for _ in range(r.choice((0, 1, 5, 9, 15, 31, 33, 47))))
        if k < 0.2:
            s += r.choice(['"', "\\", "\n", "\t", "é", "☃"])
        if k < 0.3:
            return None
        if k < 0.4:
            return r.randint(-1000, 1000)
        return s

    def row(i: int, prefix: str = "k") -> dict:
        obj = {"id": f"{prefix}{i}"}
        for c in range(1, ncol):
            v = value()
            if v is not None or r.random() < 0.5:
                obj[f"c{c}"] = v
        items = list(obj.items())
        if r.random() < 0.2:
            r.shuffle(items)
        return dict(items)

    a = [row(i) for i in range(r.randint(1, 300))]
    b = [dict(x) for x in a if r.random() > 0.05]
    for x in b:
        if r.random() < 0.3:
            x[f"c{r.randint(1, ncol - 1)}"] = value()
    for i in range(r.randint(0, 4)):
        b.append(row(i, "n"))
    if r.random() < 0.3 and a:
        a.insert(r.randrange(len(a) + 1), dict(r.choice(a)))
    escape = r.random() < 0.5
    with open(d / "a.ndjson", "w", encoding="utf-8") as f:
        for x in a:
            seps = (",", ":") if r.random() < 0.5 else (", ", ": ")
            f.write(json.dumps(x, ensure_ascii=escape, separators=seps) + "\n")
    with open(d / "b.ndjson", "w", encoding="utf-8") as f:
        for x in b:
            f.write(json.dumps(x, ensure_ascii=not escape) + "\n")
    return d / "a.ndjson", d / "b.ndjson", ["id"] + [f"c{i}" for i in range(1, ncol)]


# --- the normalising case --------------------------------------------------------
#
# Its own generator and its own random stream, so adding it changed no existing
# seed's data. Values are unquoted and hold no delimiter, quote or newline, which
# is what lets `must_refuse` below read the rows back without a CSV parser.

NORMALISING_PORTS = ("rust", "c", "cpp", "zig")  # the reference first: Rust folds Unicode
ASCII_FOLDERS = frozenset({"c", "cpp", "zig"})
WORDS = ("alpha", "Beta", "GAMMA", "delta", "x")
OUTSIDE_ASCII = ("é", "É", "ß", "İ", "\u212a")  # the last is the Kelvin sign, which folds to "k"
FLAG_CHOICES = (["--trim"], ["--ignore-case"], ["--empty-is-null"], ["--tolerance", "0.5"])


def _norm_value(r: random.Random, outside: float = 0.0) -> str:
    """A value, outside ASCII with probability `outside` when it is a word."""
    k = r.random()
    if k < 0.1:
        return ""
    if k < 0.3:
        return f"{r.choice((0, 1, 7, 42, 100)) + r.choice((0, 0.25, 0.5, 2.0)):g}"
    s = r.choice(WORDS)
    if r.random() < outside:
        s = s[: r.randint(0, len(s))] + r.choice(OUTSIDE_ASCII) + s[len(s) // 2 :]
    return s


def _nudge(r: random.Random, v: str, outside: float) -> str:
    """B's version of A's value: changed in a way one of the flags forgives, or not at all, or
    changed outright."""
    k = r.random()
    if k < 0.2:
        return r.choice((v.upper(), v.lower(), v.swapcase()))
    if k < 0.35:
        return " " * r.randint(1, 2) + v + " " * r.randint(0, 2)
    if k < 0.45 and v == "":
        return " " * r.randint(1, 3)  # only whitespace: "" under --trim, absent only with --empty-is-null
    if k < 0.6:
        try:
            return f"{float(v) + r.choice((0.3, -0.3, 3.0)):g}"
        except ValueError:
            return v
    if k < 0.75:
        return _norm_value(r, outside)
    return v


def make_norm_case(seed: int, d: Path) -> tuple[Path, Path, list[str], list[list[str]], list[list[str]], str]:
    """(a, b, header, rows of A, rows of B, kind): CSV or ndjson of the same rows."""
    r = random.Random(f"normalising-{seed}")
    ncol = r.randint(3, 7)
    header = ["id"] + [f"c{i}" for i in range(1, ncol)]
    # Outside ASCII in about a third of the pairs, and rarely there: where C++ and Zig must refuse
    # they cannot be held to Rust's counts, so most pairs have to be ones they answer.
    outside = r.choice((0.0, 0.0, 0.04))
    outside_key = r.random() < 0.05
    a = [[f"k{i}" + ("é" if outside_key and i == 0 else "")] + [_norm_value(r, outside) for _ in range(ncol - 1)]
         for i in range(r.randint(1, 120))]
    b = []
    for row in a:
        if r.random() < 0.05:
            continue
        b.append([row[0]] + [_nudge(r, v, outside) if r.random() < 0.4 else v for v in row[1:]])
    for i in range(r.randint(0, 3)):
        b.append([f"n{i}"] + [_norm_value(r, outside) for _ in range(ncol - 1)])
    if r.random() < 0.3:
        a.insert(r.randrange(len(a) + 1), list(r.choice(a)))
    kind = "norm-csv" if r.random() < 0.6 else "norm-ndjson"
    for name, rows in (("a", a), ("b", b)):
        with open(d / f"{name}.{kind[5:]}", "w", newline="", encoding="utf-8") as f:
            if kind == "norm-csv":
                f.write(",".join(header) + "\n")
                f.writelines(",".join(row) + "\n" for row in rows)
            else:
                f.writelines(json.dumps(dict(zip(header, row)), ensure_ascii=r.random() < 0.5) + "\n"
                             for row in rows)
    return d / f"a.{kind[5:]}", d / f"b.{kind[5:]}", header, a, b, kind


def norm_configs(seed: int, header: list[str]) -> list[list[str]]:
    """Two flag sets for the seed: a random non-empty mix of the four flags, on the plain key and
    with the last column ignored."""
    r = random.Random(f"normalising-flags-{seed}")
    out = []
    for base in (["-k", "id"], ["-k", "id", "-i", header[-1]]):
        picked = [f for f in FLAG_CHOICES if r.random() < 0.5] or [r.choice(FLAG_CHOICES)]
        out.append(base + [x for f in picked for x in f])
    return out


def must_refuse(a: list[list[str]], b: list[list[str]], header: list[str], flags: list[str]) -> bool:
    """Whether a port that folds case in ASCII only must refuse this run.

    The rule both such ports follow: under --ignore-case, a byte outside ASCII is refused where the
    fold decides the answer -- in any key, or in a compared value of a matched row that differs
    from its mate byte for byte (after --trim). Absent values, and numbers settled by --tolerance,
    are decided without the fold."""
    if "--ignore-case" not in flags:
        return False
    if not all(row[0].isascii() for row in a + b):
        return True
    trim, empty_is_null = "--trim" in flags, "--empty-is-null" in flags
    tolerance = float(flags[flags.index("--tolerance") + 1]) if "--tolerance" in flags else 0.0
    ignored = set(flags[flags.index("-i") + 1].split(",")) if "-i" in flags else set()
    compared = [i for i, name in enumerate(header) if i > 0 and name not in ignored]

    def absent(v: str) -> bool:
        return v == "" or (trim and empty_is_null and v.strip(" ") == "")

    def number(v: str) -> float | None:
        try:
            return float(v)
        except ValueError:
            return None

    first_a: dict[str, list[str]] = {}
    for row in a:
        first_a.setdefault(row[0], row)
    first_b: dict[str, list[str]] = {}
    for row in b:
        first_b.setdefault(row[0], row)
    for key in first_a.keys() & first_b.keys():
        for i in compared:
            x, y = first_a[key][i], first_b[key][i]
            if x == y or absent(x) or absent(y):
                continue
            if tolerance > 0 and number(x) is not None and number(y) is not None:
                continue
            tx, ty = (x.strip(" "), y.strip(" ")) if trim else (x, y)
            if not (tx.isascii() and ty.isascii()):
                return True
    return False


def make_case(seed: int, d: Path):
    """(a, b, header, kind) for `seed`. The seed picks the shape, so a range covers all of them."""
    r = random.Random(seed)
    kind = ("csv", "csv-ragged", "ndjson")[seed % 3]
    if kind == "ndjson":
        return (*gen_ndjson(r, d), kind)
    return (*gen_csv(r, d, ragged=kind == "csv-ragged"), kind)


def configs(header: list[str], kind: str) -> list[list[str]]:
    """Flag sets. `-i last` and `-i mid` move the guard column; a composite key is CSV only, where
    every row is guaranteed to have the column."""
    last, mid = header[-1], header[len(header) // 2]
    out: list[list[str]] = [["-k", "id"]]
    for col in dict.fromkeys((last, mid)):
        if col != "id":
            out.append(["-k", "id", "-i", col])
    if kind != "ndjson" and len(header) >= 3:
        out.append(["-k", "id," + header[1]])
    return out


# --- running --------------------------------------------------------------------


def run(binary: Path, a: Path, b: Path, flags: list[str], threads: int, work: Path, tag: str):
    """(exit status, contract slice, stderr, contract errors) for one invocation."""
    doc_path = work / f"{tag}.json"
    proc = subprocess.run(
        [str(binary), "compare", str(a), str(b), *flags, "--threads", str(threads), "--json", str(doc_path)],
        cwd=work, capture_output=True, timeout=120,
    )
    problems: list[str] = []
    slice_ = None
    if proc.returncode <= 1:
        try:
            doc = json.loads(doc_path.read_text())
            slice_ = {k: doc.get(k) for k in ("counts", "columns")}
            problems = contract.check(doc)
        except (OSError, ValueError) as err:
            problems = [f"no readable --json document: {err}"]
    return proc.returncode, slice_, proc.stderr.decode(errors="replace").strip(), problems


def one_norm_seed(seed: int, reference: tuple[str, Path], others: list[tuple[str, Path]],
                  strict_stderr: bool, folders: frozenset[str]) -> tuple[int, list[str]]:
    """The normalising case for one seed. A build named in `folders` folds case in ASCII only, and
    is held to the refusal rule instead of to the reference wherever the rule applies."""
    failures: list[str] = []
    n = 0
    with tempfile.TemporaryDirectory(prefix=f"fuzzn{seed}-") as tmp:
        work = Path(tmp)
        a, b, header, rows_a, rows_b, kind = make_norm_case(seed, work)
        for flags in norm_configs(seed, header):
            refuse = must_refuse(rows_a, rows_b, header, flags)
            for threads in THREADS:
                what = f"seed={seed} kind={kind} flags={' '.join(flags)} threads={threads}"
                want = run(reference[1], a, b, flags, threads, work, "ref")
                for problem in want[3]:
                    failures.append(f"CONTRACT {what} build={reference[0]}: {problem}")
                for name, binary in others:
                    got = run(binary, a, b, flags, threads, work, "got")
                    n += 1
                    for problem in got[3]:
                        failures.append(f"CONTRACT {what} build={name}: {problem}")
                    if name in folders and refuse:
                        if got[0] != 2 or "--ignore-case" not in got[2]:
                            failures.append(f"REFUSE {what} build={name}: exit {got[0]}; the fold decides "
                                            f"this answer, so it must refuse and name --ignore-case")
                        continue
                    if got[0] != want[0] or got[1] != want[1] or (strict_stderr and got[2] != want[2]):
                        failures.append(
                            f"DIFF {what} build={name} vs {reference[0]}: exit {got[0]} vs {want[0]}"
                            + ("" if got[0] <= 1 else f" ({got[2].splitlines()[0] if got[2] else 'no message'})")
                            + ("" if got[1] == want[1] else f"; counts/columns differ: {got[1]} vs {want[1]}")
                        )
    return n, failures


def one_seed(seed: int, reference: tuple[str, Path], others: list[tuple[str, Path]], strict_stderr: bool):
    """Every comparison for one seed. Returns (comparisons, list of failure lines)."""
    failures: list[str] = []
    n = 0
    with tempfile.TemporaryDirectory(prefix=f"fuzz{seed}-") as tmp:
        work = Path(tmp)
        a, b, header, kind = make_case(seed, work)
        for flags in configs(header, kind):
            for threads in THREADS:
                what = f"seed={seed} kind={kind} flags={' '.join(flags)} threads={threads}"
                want = run(reference[1], a, b, flags, threads, work, "ref")
                for problem in want[3]:
                    failures.append(f"CONTRACT {what} build={reference[0]}: {problem}")
                for name, binary in others:
                    got = run(binary, a, b, flags, threads, work, "got")
                    n += 1
                    for problem in got[3]:
                        failures.append(f"CONTRACT {what} build={name}: {problem}")
                    if got[0] != want[0] or got[1] != want[1] or (strict_stderr and got[2] != want[2]):
                        failures.append(
                            f"DIFF {what} build={name} vs {reference[0]}: "
                            f"exit {got[0]} vs {want[0]}"
                            + ("" if got[1] == want[1] else f"; counts/columns differ: {got[1]} vs {want[1]}")
                            + ("" if not strict_stderr or got[2] == want[2] else f"; stderr {got[2]!r} vs {want[2]!r}")
                        )
    return n, failures


def parse_seeds(text: str) -> range:
    lo, _, hi = text.partition("-")
    return range(int(lo), int(hi or lo) + 1)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--seeds", default="1-200", help="N or LO-HI (default 1-200)")
    ap.add_argument("--ports", help="comma list of c,cpp,rust,zig; the first is the reference")
    ap.add_argument("--ref", type=Path, help="reference binary (candidate mode)")
    ap.add_argument("--cand", type=Path, action="append", default=[], help="candidate binary; repeatable")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 2)
    args = ap.parse_args()

    if args.ports and (args.ref or args.cand):
        ap.error("use --ports, or --ref with --cand, not both")
    if args.ports:
        names = [p for p in args.ports.split(",") if p]
        unknown = [p for p in names if p not in PORTS]
        if unknown or len(names) < 2:
            ap.error(f"--ports needs two or more of {sorted(PORTS)}")
        missing = [p for p in names if not PORTS[p].exists()]
        if missing:
            print(f"not built: {', '.join(missing)} (scripts/build_ports.sh {' '.join(missing)})", file=sys.stderr)
            return 2
        reference, others, strict = (names[0], PORTS[names[0]]), [(p, PORTS[p]) for p in names[1:]], False
        carrying = [p for p in NORMALISING_PORTS if p in names]
        norm = ((carrying[0], PORTS[carrying[0]]), [(p, PORTS[p]) for p in carrying[1:]], ASCII_FOLDERS) \
            if len(carrying) >= 2 else None
    elif args.ref and args.cand:
        # Absolute, because every run happens in its own temporary directory.
        reference = ("ref", args.ref.resolve())
        others = [(f"cand{i}" if len(args.cand) > 1 else "cand", p.resolve()) for i, p in enumerate(args.cand)]
        strict = True
        # Two builds of one port: the same flags to both, and the same answer from both.
        norm = (reference, others, frozenset())
    else:
        ap.error("give --ports, or --ref and at least one --cand")

    def both(seed: int) -> tuple[int, list[str]]:
        n, failures = one_seed(seed, reference, others, strict)
        if norm:
            m, more = one_norm_seed(seed, norm[0], norm[1], strict, norm[2])
            n, failures = n + m, failures + more
        return n, failures

    total = bad = 0
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for n, failures in pool.map(both, parse_seeds(args.seeds)):
            total += n
            for line in failures:
                print(f"::error::{line}" if os.environ.get("GITHUB_ACTIONS") else line)
            bad += len(failures)
    print(f"comparisons={total} bad={bad}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
