#!/usr/bin/env python3
"""The same comparison from CSV, TSV, JSON and Parquet, in every native port.

Two questions at once, which is why the table has two axes:

  * how the three ports compare on one input -- the question "can Rust and Zig
    match the C++ port" is asked here, on one host, in one sitting;
  * what the input format costs -- the same rows, the same key, the same
    per-cell diff, with only the container changing.

Every payload is written by this project's own generator (`compara-gen-data`),
so the formats hold the same values spelled the same way; a conversion step
would otherwise be measuring the converter. Each format is generated, measured
and deleted in turn, because all three at ten million rows is about 15 GB.

Peak RSS and CPU time both come from `wait4`'s rusage for that exact child --
the kernel's own high-water mark rather than a poll that can miss a spike. These
engines map their inputs, so resident pages include the file; the `above` column
subtracts it, and for Parquet that number is the whole point, since the pages are
decoded into an arena and the mapping is given back.

The `Budget` column is a different quantity and is the one `--memory-cap` acts
on: peak `VmData`, the virtual size of the private writable mappings, which is
what `RLIMIT_DATA` is checked against. A port that holds an old buffer while it
fills a new one is charged for both there and for neither in peak RSS, so the two
columns can disagree by a lot -- on a 6M CSV pair the port with the second
smallest `above` needs the largest budget. Without this column the table could
not explain its own refusals. It is polled rather than taken from rusage, which
has no equivalent, so it is a floor on the true peak.

CPU seconds over wall seconds is the `cores` column: how many of this machine's
cores were actually busy. It is the column that separates "slow" from "idle" --
two ports at the same wall time and 1.2x against 3.6x cores are not the same
result, and the first one has headroom the second has already spent.

Both inputs are read once before anything is timed: a cold page cache costs more
than every difference this table is trying to show.

  python3 scripts/bench_formats_ports.py --rows 1m --repeats 2
  python3 scripts/bench_formats_ports.py --rows 10m --formats csv,parquet
"""
from __future__ import annotations

import argparse
import json
import os
import platform
import resource
import subprocess
import sys
import time
from pathlib import Path

from mdtable import render

ROOT = Path(__file__).resolve().parent.parent
KEY = ["-k", "account_id,txn_id", "-i", "updated_at"]

C = ROOT / "c/csvdiff"
CPP = ROOT / "cpp/build/csvdiff"
RUST = ROOT / "rust/target/release/compara"
ZIG = ROOT / "zig/zig-out/bin/csvdiff"
GEN = ROOT / "rust/target/release/compara-gen-data"


TEXT = {"csv", "tsv", "ndjson"}
ALL = {"csv", "tsv", "ndjson", "parquet"}
ALIAS = {"json": "ndjson"}


def gate_flags(flags: list[str]) -> list[str]:
    """The timed flags, with `--summary` traded back for a discarded report.

    The counts gate needs the JSON document, and `--summary` refuses an output
    flag rather than guessing which of the two was meant -- so the gate asks for
    the report the timed rounds do not. It is untimed, so what it costs the port
    that renders one does not reach any table.
    """
    if "--summary" not in flags:
        return flags
    return [f for f in flags if f != "--summary"] + ["-o", "/dev/null"]


def ports(threads: int | None, matrix: bool) -> list[tuple[str, list[str], list[str], set[str]]]:
    """(label, argv prefix, extra flags, the formats it can read).

    All four ports read all four formats. A Parquet pair is the one input none
    of them scans: it goes to the columnar path instead, which is why the Parquet
    rows below are not measuring the scanner the CSV rows are.

    The C port is here rather than only in `bench_ports.py` so that one table can
    answer "did this change make a port slower" for every port at once. It was
    the one port this script did not build, which meant the per-pull-request
    benchmark and the leading port were in two different workflows.

    `matrix` adds the scanner builds -- SWAR against a vector register, one
    binary each so nothing is measuring a branch -- and the Rust engine without
    its report, which is the only row here that is not comparing like with like:
    the Rust port renders the HTML the other two do not produce at all.

    The Rust rows do **not** pass `--engine turbo`, and used to. The Rust port
    carries two Parquet readers, and `engine.rs` says in as many words that an
    explicit `--engine turbo` is the one thing that retires the columnar one:
    *"Only an explicit --engine turbo should do that."* So this table spent its
    life measuring the reader a user never gets. On a 500k pair the two are
    0.15s against 0.43s, and 91 MB against 263 MB above the input, for identical
    counts -- which is the whole of the Rust Parquet column in every table this
    project has published, and most of why the port could not do 50M. `auto`
    picks the columnar reader and falls through to `turbo` for a codec or a page
    version it cannot read, so dropping the flag loses no coverage.
    """
    thread_flag = ["--threads", str(threads)] if threads else []
    # Only the report suppression. See the note above about `--engine`.
    #
    # `--summary` rather than `-o /dev/null`, which is what this was and which
    # only moved the write. The render still ran, and so did everything the
    # engine does to feed it: up to `--max-rows` rows per section decoded into
    # strings, sorted and cell-diffed, plus a walk of every duplicated key. C,
    # C++ and Zig write nothing without an output flag and build none of that,
    # so those rounds were timing four ports on three tasks. On a 4M pair it is
    # 1.31x wall and 1.18x CPU of what this port actually has to do; on 2M rows
    # of Parquet, 1.27x and 1.11x.
    #
    # `gate_flags` puts the report back for the counts gate, which needs the
    # document and is not timed.
    report = ["--summary"]
    rows: list[tuple[str, list[str], list[str], set[str]]] = [
        ("C", [str(C)], thread_flag, ALL),
        ("C++", [str(CPP)], thread_flag, ALL),
        ("Rust", [str(RUST)], report + thread_flag, ALL),
        ("Zig", [str(ZIG)], thread_flag, ALL),
    ]
    # `BENCH_RUST_VARIANT_ENV="NAME=value"`: the Rust port again, beside
    # itself, with that variable set -- an A/B of an opt-in switch on the
    # same runner and the same data.
    variant = os.environ.get("BENCH_RUST_VARIANT_ENV", "").strip()
    if variant:
        rows.append(("Rust*", ["/usr/bin/env", variant, str(RUST)], report + thread_flag, ALL))
    if not matrix:
        return rows
    for label, path, flags, formats in [
        # The scanner builds differ only in how they find a delimiter, so they
        # are asked only about the formats that have delimiters to find. The
        # SWAR row is the plain `C++` build above: it takes the eight-byte step,
        # so a separate `csvdiff-swar` was the same binary under a second name.
        ("C++ avx2", CPP.with_name("csvdiff-avx2"), thread_flag, TEXT),
        ("C++ avx512", CPP.with_name("csvdiff-avx512"), thread_flag, TEXT),
        ("Rust avx2", ROOT / "rust/target-avx2/release/compara", report + thread_flag, ALL),
        ("Zig v32", ROOT / "zig/zig-out-v32/bin/csvdiff", thread_flag, ALL),
        ("Zig v64", ROOT / "zig/zig-out-v64/bin/csvdiff", thread_flag, ALL),
    ]:
        # A variant that was not built is left out by name rather than reported
        # as slow, and an AVX-512 binary on a runner without AVX-512 will not
        # start at all -- which the run records as a failure rather than a time.
        if path.exists():
            rows.append((label, [str(path)], flags, formats))
    return rows


def host() -> dict[str, object]:
    """What machine this is, in enough detail to refuse a bad comparison.

    Every number this script produces is only comparable with another number
    taken on the same CPU. That is not a general caution: GitHub's hosted fleet
    mixes processor generations behind one label, so two jobs both saying
    `ubuntu-latest` can be a Xeon Platinum 8370C and an EPYC 7763 -- different
    core counts per socket, different cache, different AVX-512 story. A ladder
    that fans one size out per job and merges the results is merging tables, and
    this project's own rule says never to do that.

    So the identity goes in the JSON beside the numbers rather than only into a
    log line, and `scripts/bench_group.py` groups on `key` before it prints
    anything. `key` is deliberately coarse -- model, core count and the widest
    vector the CPU admits to -- because those are what change a result. Stepping
    and microcode do not, and including them would split groups that belong
    together.
    """
    model, flags = "unknown", set()
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name") and model == "unknown":
                model = line.split(":", 1)[1].strip()
            elif line.startswith("flags") and not flags:
                flags = set(line.split(":", 1)[1].split())
    except OSError:
        pass  # not Linux; the fields below degrade to "unknown" rather than fail

    if "avx512bw" in flags:
        isa = "avx512"
    elif "avx2" in flags:
        isa = "avx2"
    elif "sse2" in flags:
        isa = "sse2"
    else:
        isa = "baseline"

    # How much RAM, because a rung whose input does not fit in it is not
    # measuring the engines. See `fits_in_ram` below.
    ram_mb = 0
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                ram_mb = int(line.split()[1]) // 1024
                break
    except (OSError, ValueError, IndexError):
        pass

    cores = os.cpu_count() or 1
    return {
        "cpu": model,
        "cores": cores,
        "ram_mb": ram_mb,
        "isa": isa,
        "arch": platform.machine(),
        # One string to group on. Two runs that disagree here are two tables.
        "key": f"{model} | {cores}c | {isa}",
        "runner": os.environ.get("RUNNER_NAME", ""),
        "os": platform.platform(),
    }


def run(argv: list[str], timeout: float,
        memory_cap_mb: int | None = None,
        errors: Path | None = None) -> tuple[float, float, float, int, str, float]:
    """Times one child: wall, peak RSS in MB, CPU seconds, exit code, why, peak data.

    CPU is user plus system for that exact child, from the same `wait4` rusage
    the RSS comes from. Wall time says how long you waited; CPU divided by wall
    says how many cores were busy while you waited, which is the difference
    between an engine that is slow and an engine that is idle. A port that
    finishes in the same wall time on half the CPU has the headroom the other
    one has already spent.

    `why` is the child's last line of stderr, which is empty on a run that
    worked and is the whole diagnosis on one that did not. It used to go to
    /dev/null with stdout, so a rung that ran out of memory reported `FAILED
    (exit 2)` and nothing else -- and the ports name what did not fit.

    The last number is peak `VmData`, and it is here because **it is the quantity
    the cap below actually bounds** and peak RSS is not. `RLIMIT_DATA` is checked
    against `mm->data_vm`, the *virtual* size of the private writable mappings --
    heap, anonymous mmap, thread stacks -- so a port that holds an old buffer
    while it fills a new one is charged for both even though only one is ever
    touched. Peak RSS cannot see that: it counts resident pages, and it counts
    the mapped input among them.

    Which made the table unable to explain its own refusals. Measured on a 6M CSV
    pair, the four ports want 448, 747, 748 and 955 MB of `VmData` while holding
    405, 511, 697 and 592 MB above the input -- so the port with the *second
    smallest* footprint is the one that needs the largest budget, and it is the
    one the 100M rung refused. Sampled from /proc rather than taken from `wait4`,
    because rusage has no equivalent: `ru_maxrss` is the only memory figure there.
    """
    started = time.monotonic()
    pid = os.fork()
    if pid == 0:
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, 1)
        if errors is None:
            os.dup2(devnull, 2)
        else:
            fd = os.open(errors, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            os.dup2(fd, 2)
        if memory_cap_mb:
            # RLIMIT_DATA, not RLIMIT_AS. Every port maps its input, and since
            # Linux 4.7 this limit covers the heap and private anonymous
            # mappings but *not* a file-backed one -- so it bounds what scales
            # with the row count and leaves the mapping alone, which is the
            # distinction the ports' own --max-memory draws. Measured: a 702 MB
            # pair compares fine under a 256 MB cap, and is refused under 96 MB.
            #
            # This is what stops a rung too large for its runner from taking the
            # whole runner with it. Without it the kernel reclaims the VM, the
            # step dies with exit 143 and "the runner has received a shutdown
            # signal", and the rung reports nothing at all -- not even that it
            # ran out of memory. All four ports fail legibly under the cap
            # instead, and all four exit 2: C and Zig say "out of memory", C++
            # "std::bad_alloc", and Rust names the structure that did not fit.
            limit = memory_cap_mb * 1024 * 1024
            resource.setrlimit(resource.RLIMIT_DATA, (limit, limit))
        try:
            os.execv(argv[0], argv)
        except OSError:
            pass
        os._exit(127)
    # The wall time comes from a blocking wait, not from the 50 ms poll that
    # samples memory: polled, every time was a multiple of 50 ms, which at two
    # million rows is a sixth of a run and turned 0.31 s against 0.34 s into
    # a tie or a 17% gap depending on where the polls fell.
    import threading
    status_path = f"/proc/{pid}/status"
    peak = [0.0]
    finished = threading.Event()
    killed = [False]

    def sample() -> None:
        deadline = started + timeout
        while not finished.wait(0.05):
            peak[0] = max(peak[0], vm_data_mb(status_path))
            if time.monotonic() > deadline:
                killed[0] = True
                try:
                    os.kill(pid, 9)
                except OSError:
                    pass
                return

    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    _, status, usage = os.wait4(pid, 0)
    wall = time.monotonic() - started
    finished.set()
    sampler.join()
    if killed[0]:
        return (wall, 0.0, usage.ru_utime + usage.ru_stime, -1, last_line(errors), peak[0])
    return (wall, usage.ru_maxrss / 1024, usage.ru_utime + usage.ru_stime,
            os.waitstatus_to_exitcode(status), last_line(errors), peak[0])


def vm_data_mb(status_path: str) -> float:
    """This process's current `VmData`, in MB, or 0 where it cannot be read.

    Linux only -- /proc/<pid>/status. On anything else this returns 0 and the
    column reads as a dash, which is honest: the cap is not applied there either,
    since `RLIMIT_DATA` bounding anonymous mappings is a Linux behaviour.

    Polled, so it is a floor on the true peak and not the peak itself: a spike
    shorter than the 50ms between samples is invisible here. Measured against a
    5ms poll of the same run, the Zig port's 955 MB peak reads 955, 888 and 888
    on three 50ms passes -- an undershoot of up to 7% on a run lasting 1.6s. The
    rungs this column exists for run for minutes, where a phase boundary lasts
    long enough to be sampled many times, so treat it as exact at scale and as
    approximate on a small pair.

    It rides the poll the timeout already runs on, so the column costs one open
    and one read per fifty milliseconds and no extra wakeups.
    """
    try:
        with open(status_path) as handle:
            for line in handle:
                if line.startswith("VmData:"):
                    return int(line.split()[1]) / 1024
    except (OSError, ValueError, IndexError):
        pass
    return 0.0


def last_line(path: Path | None) -> str:
    """The child's final line of stderr, or "" if it wrote none.

    The last one and not the first: a port that fails on several threads at once
    writes several lines, and the ones behind it are the same refusal again.
    """
    if path is None:
        return ""
    try:
        text = path.read_text(errors="replace").strip()
    except OSError:
        return ""
    if not text:
        return ""
    return text.splitlines()[-1].strip()[:200]


def warm(*paths: Path) -> None:
    for path in paths:
        with path.open("rb") as handle:
            while handle.read(1 << 22):
                pass


def counts(path: Path) -> dict:
    with path.open() as handle:
        return json.load(handle)["counts"]


def generate(rows: str, fmt: str, data: Path) -> tuple[Path, Path, float]:
    started = time.monotonic()
    argv = [str(GEN), "--rows", rows, "--out-dir", str(data), "--prefix", "bench",
            "--format", fmt]
    done = subprocess.run(argv, capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit(f"the generator failed: {done.stderr.strip()[:400]}")
    extension = {"csv": "csv", "tsv": "tsv", "ndjson": "ndjson", "parquet": "parquet"}[fmt]
    return (data / f"bench_a.{extension}", data / f"bench_b.{extension}",
            time.monotonic() - started)


# Where the input stops being a benchmark and starts being a page-cache test.
# Not 1.0: the ports need room above the mapped bytes for their indexes, which
# is the `above the input` column and runs to a gigabyte or two at these sizes.
RAM_HEADROOM = 0.8


def fits_in_ram(input_mb: float, h: dict) -> bool:
    """Whether a pair this size leaves the machine room to work.

    Unknown RAM answers True: this guards a reading of the numbers, and refusing
    to report on a machine whose memory could not be read would lose more than
    it protects.
    """
    ram = h.get("ram_mb") or 0
    return not ram or input_mb <= ram * RAM_HEADROOM


def table_of(results: list[dict]) -> str:
    """The results table as markdown, from whatever has been measured so far."""
    grid = []
    for row in results:
        if row.get("seconds") is None:
            grid.append([row["port"], row["format"], "-", "-", "-", "-", "-", "-", "-"])
            continue
        rate = row["rows"] / row["seconds"] if row["seconds"] else 0
        data = row.get("data") or 0
        grid.append([row["port"], row["format"], f"{row['seconds']:.3f}s", f"{rate:,.0f}",
                     f"{row['cpu']:.1f}s", f"{row['cores']:.2f}x",
                     f"{row['rss']:,.0f} MB", f"{row['above']:,.0f} MB",
                     f"{data:,.0f} MB" if data else "-"])
    # "Budget" and not a second memory column with a similar name: it is what
    # `--memory-cap` has to be at least, which is a different question from what
    # the run holds. See `run()` for why the two differ and by how much.
    return render(["Build", "Format", "Compare", "Rows/s", "CPU", "Cores", "Peak RSS",
                   "Above the input", "Budget"],
                  ["l", "l", "r", "r", "r", "r", "r", "r", "r"], grid)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--rows", default="1m")
    parser.add_argument("--formats", default="csv,ndjson,parquet")
    parser.add_argument("--json-out", type=Path, default=None,
                        help="also write the rows here, unrounded, for a harness "
                             "that has to compare two runs of this script")
    parser.add_argument("--md-out", type=Path, default=None,
                        help="write just the results table here, as markdown")
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--threads", type=int, default=None,
                        help="threads per run; the default is whatever each port picks")
    parser.add_argument("--timeout", type=float, default=1800)
    parser.add_argument("--memory-cap", type=int, default=None, metavar="MB",
                        help="cap each port's heap and anonymous memory (not its "
                             "mapped input) so a size too large for this host is "
                             "refused by the port rather than killing the host")
    parser.add_argument("--data-dir", default=str(ROOT / "data/bench"))
    parser.add_argument("--keep", action="store_true", help="do not delete the payloads")
    parser.add_argument("--matrix", action="store_true",
                        help="also run the scanner builds and the Rust engine without its report")
    parser.add_argument("--first", default=None, metavar="LABEL",
                        help="run the rows whose label starts with LABEL before all the "
                             "others; the table keeps its declared order either way")
    args = parser.parse_args(argv)

    for tool in (RUST, ZIG, GEN):
        if not tool.exists():
            raise SystemExit(f"build it first: {tool} is missing")

    data = Path(args.data_dir)
    data.mkdir(parents=True, exist_ok=True)
    summary = Path("/tmp/bench_ports_summary.json")
    errors = Path("/tmp/bench_ports_stderr.txt")
    results: list[dict] = []
    answers: dict[str, dict] = {}

    # Resolved and checked before anything is generated. `json` is what the
    # generators' own --format flag calls this format, so it is accepted here
    # too; a name that is neither is a mistake worth hearing about now rather
    # than after the first format has been measured.
    wanted = []
    for name in args.formats.split(","):
        fmt = ALIAS.get(name.strip(), name.strip())
        if fmt not in ALL:
            raise SystemExit(f"unknown format {name.strip()!r}: "
                             f"pick from {', '.join(sorted(ALL))}")
        wanted.append(fmt)

    # Resolved before the first rung rather than at the end: the paging warning
    # below needs to know how much memory this machine has, and it is the same
    # machine either way.
    h = host()

    for fmt in wanted:
        a, b, generated = generate(args.rows, fmt, data)
        size = (a.stat().st_size + b.stat().st_size) / (1 << 20)
        print(f"\n{fmt}: {size:,.0f} MB, generated in {generated:.1f}s", flush=True)

        # A pair that does not fit in RAM does not produce a benchmark. These
        # engines map their inputs, so once the pair approaches the machine's
        # memory the kernel starts evicting pages the port still wants and the
        # times become a ranking of page-fault behaviour. It is not subtle when
        # it happens: a 20m ndjson rung whose 16,975 MB pair ran on a 15,989 MB
        # host took 13m50s where the 20m csv rung took 1m37s, and every port
        # reported a peak RSS *below* its own input.
        #
        # This still runs the rung -- a number with a stated caveat beats no
        # number, and which port degrades worst under paging is its own kind of
        # answer -- but it will not let the table be read as anything else.
        if not fits_in_ram(size, h):
            fitting = f"{h['ram_mb']:,} MB" if h.get("ram_mb") else "unknown"
            print(f"  !! this pair is {size:,.0f} MB and the host has {fitting}. "
                  f"The times below rank paging, not parsing.", flush=True)
        warm(a, b)

        # Ports keep their declared order in the table no matter what
        # happens to them at run time, so two runs of this script can be read
        # side by side.
        plan = []
        for label, prefix, flags, can_read in ports(args.threads, args.matrix):
            if fmt not in can_read:
                print(f"  {label:5s} -- does not read {fmt}", flush=True)
                plan.append({"label": label, "state": "unreadable"})
            elif not Path(prefix[0]).exists():
                print(f"  {label:5s} -- not built", flush=True)
                plan.append({"label": label, "state": "unbuilt"})
            else:
                plan.append({"label": label, "prefix": prefix, "flags": flags,
                             "state": "ok"})
        runnable = [e for e in plan if e["state"] == "ok"]
        # `--first` reorders who runs, not what the table shows. A rung that dies
        # partway keeps whatever ran before it died, and declared order decided
        # who that was: a 200m measurement had the runner taken away after C and
        # C++ and before Rust and Zig, so the cells at the back of a truncated
        # ladder were blank for a reason that had nothing to do with them -- the
        # exact failure this rotation comment exists to name. Naming the port
        # that matters most puts its number on disk first. Rows that share the
        # label's prefix (the matrix Rust rows) come along with it.
        if args.first:
            first = [e for e in runnable if e["label"].startswith(args.first)]
            rest = [e for e in runnable if not e["label"].startswith(args.first)]
            runnable = first + rest
        best: dict[str, tuple[float, float, float]] = {}
        failed: set[str] = set()
        rows_seen: dict[str, int] = {}

        # The counts gate runs once per port, untimed, and the timed rounds
        # below do not pass `--json` at all.
        #
        # Passing it to everything was measuring four different tasks. Only the
        # C++ port emits row samples: C, Rust and Zig write `counts` and
        # `columns` and stop, and C has no flag to make it do otherwise because
        # it has no such feature. So `--json` costs C 39,250 instructions on a
        # 400k pair -- 0.0% -- and costs C++ 808 million, 44%, because it turns
        # on a second full random-probed pass over B and then materialises,
        # sorts and writes every sampled row.
        #
        # Measured on a 4M pair, four threads, page cache warmed and the two
        # modes interleaved: C++ is 1.81x C on the task both actually do, and
        # 2.25x once C++ alone is asked for a report. Roughly a third of the
        # published CSV gap was the report.
        #
        # This is the same fault as the `-march=native` one at the top of
        # BENCHMARKS.md, and `ports()` already fixes the matching case for Rust
        # by passing `-o /dev/null` so its HTML is not charged against ports
        # that render none. The gate itself still needs the document, so it gets
        # its own run outside the timing.
        for entry in runnable:
            argv_gate = entry["prefix"] + ["compare", str(a), str(b)] + KEY + \
                gate_flags(entry["flags"]) + ["--json", str(summary)]
            _, _, _, code, why, _ = run(argv_gate, args.timeout, args.memory_cap, errors)
            if code not in (0, 1):
                said = f": {why}" if why else ""
                print(f"  {entry['label']:5s} FAILED the counts gate (exit {code}){said}",
                      flush=True)
                failed.add(entry["label"])
                continue
            got = counts(summary)
            answers.setdefault(f"{entry['label']}/{fmt}", got)
            rows_seen[entry["label"]] = got["a_rows"]

        # One run of every port per round, and the rounds repeat -- which is
        # what BENCHMARKS.md has always said this does and what it did not do.
        # It used to run every repeat of one port before starting the next, so
        # a machine drifting under the run charged that drift to whichever port
        # happened to be in front of it.
        #
        # The starting port rotates. Fixed order makes position part of a
        # port's number -- someone is always first into a cold page cache and
        # someone always last -- and it decides who survives a truncated run.
        # A 40m Parquet cell has died three times on a 16 GB runner, always
        # after C and C++ and always before Rust and Zig, so the two ports at
        # the back of the list have never produced a number at that size. That
        # is not evidence they are slower. It is evidence they run last.
        for rnd in range(args.repeats):
            turn = rnd % len(runnable) if runnable else 0
            for entry in runnable[turn:] + runnable[:turn]:
                label = entry["label"]
                if label in failed:
                    continue
                # No `--json`: see the counts gate above for why the timed run
                # measures the task every port performs and not one port's
                # report.
                argv_run = entry["prefix"] + ["compare", str(a), str(b)] + KEY + \
                    entry["flags"]
                seconds, rss, cpu, code, why, vm_data = run(argv_run, args.timeout,
                                                            args.memory_cap, errors)
                if code not in (0, 1):
                    # A port that failed once has failed. A later round that
                    # happens to succeed does not withdraw the failure.
                    said = f": {why}" if why else ""
                    print(f"  {label:5s} FAILED (exit {code}){said}", flush=True)
                    failed.add(label)
                    continue
                # The best run is the fastest one, and its CPU travels with it:
                # pairing the fastest wall time with another run's CPU would
                # make the utilisation a ratio of two different runs.
                if label not in best or seconds < best[label][0]:
                    best[label] = (seconds, rss, cpu, vm_data)

        for entry in plan:
            label, state = entry["label"], entry["state"]
            if state == "unbuilt":
                continue
            if state == "unreadable" or label in failed or label not in best:
                results.append({"format": fmt, "port": label, "seconds": None})
                continue
            seconds, rss, cpu, vm_data = best[label]
            results.append({
                "format": fmt, "port": label, "seconds": seconds, "rss": rss,
                "cpu": cpu, "cores": cpu / seconds if seconds > 0 else 0.0,
                "input": size, "above": rss - size, "rows": rows_seen[label],
                "data": vm_data,
            })
            budget = f"{vm_data:8,.0f} MB budget" if vm_data else "   (no budget)"
            print(f"  {label:5s} {seconds:8.2f}s  {cpu:8.1f}s cpu  "
                  f"{cpu / seconds if seconds else 0:5.2f}x cores  "
                  f"{rss:9,.0f} MB peak  {rss - size:8,.0f} MB above the input  "
                  f"{budget}",
                  flush=True)

        if not args.keep:
            for path in (a, b):
                path.unlink(missing_ok=True)

        # Written now rather than at the end. A run that is killed partway --
        # which is how the 40m Parquet cell has ended three times -- used to
        # leave nothing behind at all, because the table was assembled after
        # every format had finished. What has been measured is now on disk as
        # soon as it has been measured.
        if args.md_out:
            args.md_out.parent.mkdir(parents=True, exist_ok=True)
            args.md_out.write_text(table_of(results))

    # Every port and every format has to return the same counts. A faster answer
    # that is not the same answer is not a result.
    distinct = {json.dumps(v, sort_keys=True) for v in answers.values()}
    # Three outcomes, not two. No port producing counts is an absence, not a
    # disagreement, and calling it "DISAGREE" sent a reader looking for a
    # discrepancy between ports that had each failed before answering -- which is
    # what every port does when the run is capped below what the size needs.
    if not distinct:
        print("\ncounts: none -- no port got far enough to answer")
        return 1
    print("\ncounts:", "identical everywhere" if len(distinct) == 1 else "DISAGREE")
    if len(distinct) != 1:
        for name, value in answers.items():
            print(f"  {name}: {value}")
        return 1
    print(f"  {json.dumps(json.loads(distinct.pop()))}")

    cores = h["cores"]
    print(f"\n{cores} cores; \"cores\" is CPU seconds over wall seconds -- how many "
          f"were busy, out of {cores}.")
    # Named here and carried in the JSON because it is what decides whether this
    # table may be read beside another one. See host().
    print(f"host: {h['key']}"
          + (f"  (runner {h['runner']})" if h["runner"] else ""))
    md = table_of(results)
    print("\n" + md, end="")

    # The table on its own, for a caller that wants to publish it as a table
    # rather than as part of this log. Everything above is narrative -- what was
    # generated, what each port did, whether the counts agreed -- and belongs in
    # a code block; the table does not, and inside one it can never render.
    if args.md_out:
        args.md_out.parent.mkdir(parents=True, exist_ok=True)
        args.md_out.write_text(md)

    # The table rounds seconds to two decimals, which is a couple of per cent at
    # the sizes this runs at -- fine to read, too coarse to compare two runs
    # with. The JSON keeps what was measured.
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(
            {"rows_arg": args.rows, "cores": cores, "host": h, "results": results},
            indent=2, sort_keys=True))
        print(f"\nwrote {args.json_out}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
