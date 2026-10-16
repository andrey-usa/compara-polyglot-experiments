#!/usr/bin/env bash
#
# Build named port targets at the same time instead of one after another.
#
# Every workflow that needs more than one port was building them serially in a
# single `run:` block, which on a four-core runner leaves three cores idle
# through most of it: a single-file `clang++` compile is one core, and three of
# those fit beside a `cargo build` that is already using what it can. Measured
# on a four-core box, `cpp`, `cpp-scanners` and `zig` together took 118.5s in a
# row and 54.0s at once -- 2.19x, for work that was always independent.
#
# Concurrency defaults to the core count rather than "all of them at once".
# Seven concurrent C++ and Rust compiles is several gigabytes of resident
# compiler, and a runner that swaps would give the time back with interest.
#
#   scripts/build_ports.sh rust zig cpp cpp-gen
#   BUILD_JOBS=2 scripts/build_ports.sh c cpp
#
# `CXX`, `CC` and `RUSTFLAGS` are read from the environment, so a caller that
# wants clang or a target feature sets it the way it always did.
#
# Each target's output is kept and printed under its own heading when it
# finishes, because interleaved compiler errors from four builds at once are
# not readable. The exit status is non-zero if any target failed, and every
# target is run even when an earlier one fails -- one run should name all the
# broken builds, not just the first.

set -uo pipefail

command_for() {
    case "$1" in
        c)            echo 'make -C c' ;;
        c-gen)        echo 'make -C c gen-data' ;;
        cpp)          echo 'make -C cpp' ;;
        cpp-gen)      echo 'make -C cpp gen-data' ;;
        # Not the `scanners` target: that also builds `build/csvdiff-avx512`,
        # which does not start on a runner without AVX-512 -- the caller asks
        # for `cpp-avx512` separately when the host has the instructions.
        cpp-scanners) echo 'make -C cpp build/csvdiff-avx2' ;;
        cpp-avx512)   echo 'make -C cpp build/csvdiff-avx512' ;;
        rust)         echo 'cd rust && cargo build --release --no-default-features' ;;
        # With DuckDB and polars, for the workflows that still ask for them.
        rust-full)    echo 'cd rust && cargo build --release' ;;
        # A target feature is not a source switch, so this needs its own target
        # directory; cargo locks per directory, so it runs beside the first.
        rust-avx2)    echo 'cd rust && RUSTFLAGS="-C target-feature=+avx2" cargo build --release --no-default-features --target-dir target-avx2' ;;
        zig)          echo 'cd zig && zig build --release=fast' ;;
        zig-native)   echo 'cd zig && zig build --release=fast -Dcpu=native' ;;
        # `-Dcpu=native` is not optional on these two: at baseline x86-64 a
        # @Vector(32, u8) is lowered to scalar code and the row would be
        # measuring a slower SWAR rather than the instruction set it names.
        zig-v32)      echo 'cd zig && zig build --release=fast -Dscan=32 -Dcpu=native --prefix zig-out-v32' ;;
        zig-v64)      echo 'cd zig && zig build --release=fast -Dscan=64 -Dcpu=native --prefix zig-out-v64' ;;
        *)            return 1 ;;
    esac
}

if [ "$#" -eq 0 ]; then
    echo "usage: $0 TARGET [TARGET...]" >&2
    echo "targets: c c-gen cpp cpp-gen cpp-scanners cpp-avx512 rust rust-full" >&2
    echo "         rust-avx2 zig zig-native zig-v32 zig-v64" >&2
    exit 2
fi

# Refuse the whole run on a name that does not exist rather than building most
# of it and reporting success. A workflow that asks for a target this script
# has since renamed should fail loudly, not quietly measure a stale binary.
for target in "$@"; do
    if ! command_for "$target" >/dev/null; then
        echo "build_ports: no such target: $target" >&2
        exit 2
    fi
done

# Every other port is compiled for the machine it runs on: both Makefiles probe
# and add `-march=native`, and Zig's default target is already the host (a build
# with `-Dcpu=native` is byte-identical to one without). Rust was the exception.
# With no RUSTFLAGS, `cargo build --release` targets baseline x86-64 -- sse, sse2
# and fxsr, nothing else -- so every table this script produced compared an SSE2
# Rust against an AVX-512 C and C++.
#
# That is not the documented intent. README.md says "every port is compiled for
# the machine it runs on: -march=native for C and C++, -C target-cpu for Rust,
# -Dcpu=native for Zig", BENCHMARKS.md says the same of its current tables, and
# `benchmark-native.yml` sets the flag by hand. The six workflows that build
# through this script never did, and those are the ones a pull request sees.
#
# On x86-64 that is no longer the gap it was. compara asks the CPU at run time
# for AVX2 and scans with it where it is there (E-001), so a baseline build --
# the one its releases ship and `cargo install` makes -- scans as fast as one
# built for the runner, and that baseline build is the one the tables measure:
# a number from a build nobody installs is the mismatch M-153 recorded. The
# static `+avx2` build stays beside it as `rust-avx2`, so the matrix still shows
# what compiling the compare in is worth. On x86-64 the script sets nothing.
#
# Elsewhere it still compiles for the host. The CPU is named rather than
# `native`, because `-C target-cpu` reaches *every* crate cargo compiles, not
# just the csvdiff binary the tables measure -- including the proc-macro build
# tools (`proc-macro2`, behind serde's derive) whose compiled binaries then
# *run* on the runner half-way through the build. Resolving the host CPU on
# GitHub's x86 VM fleet once picked `znver4`, and one of those build tools died
# with SIGILL because the guest advertises AVX-512 that it cannot execute.
#
# An explicit RUSTFLAGS from the caller still wins, as it always did.
if [ -z "${RUSTFLAGS:-}" ] && [ "$(uname -m 2>/dev/null)" != "x86_64" ]; then
    native_cpu=$(rustc --print target-cpus 2>/dev/null |
                 sed -n 's/.*currently \([A-Za-z0-9_-]*\).*/\1/p' | head -1)
    export RUSTFLAGS="-C target-cpu=${native_cpu:-native}"
fi
echo "build_ports: RUSTFLAGS=${RUSTFLAGS:-(none: baseline, as installed)}"

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
logs=$(mktemp -d)
trap 'rm -rf "$logs"' EXIT

jobs=${BUILD_JOBS:-$(nproc 2>/dev/null || echo 4)}
echo "building ${*} with up to $jobs at a time"

declare -a names pids
for target in "$@"; do
    # Wait for a slot. `wait -n` returns when any one child finishes; its exit
    # status is collected properly in the join loop below, so it is ignored here.
    while [ "$(jobs -rp | wc -l)" -ge "$jobs" ]; do
        wait -n 2>/dev/null || true
    done
    # Each target times itself. The duration used to be computed in the join
    # loop below as `SECONDS - started`, which is not how long the target took:
    # the loop joins in order and blocks on each `wait`, so a target that
    # finished early but sits behind a slow one was charged the wait as well.
    # In one CI run that reported `zig (108s)` and `cpp (108s)` beside
    # `cpp-gen (105s)` when the whole script took 123s -- cpp-gen had not even
    # started until a slot freed, so its own build could not have been 105s.
    # Everything looked as slow as the slowest thing in front of it, which is
    # the one reading that makes a parallel build script useless for deciding
    # what to speed up.
    (
        cd "$root" || exit 1
        started=$SECONDS
        eval "$(command_for "$target")"
        status=$?
        echo "$(( SECONDS - started ))" >"$logs/$target.time"
        exit "$status"
    ) >"$logs/$target.log" 2>&1 &
    names+=("$target")
    pids+=("$!")
done

broken=0
for i in "${!pids[@]}"; do
    name=${names[$i]}
    if wait "${pids[$i]}"; then
        status="ok"
    else
        status="FAILED"
        broken=$(( broken + 1 ))
    fi
    echo "::group::$status  $name  ($(cat "$logs/$name.time" 2>/dev/null || echo "?")s)"
    cat "$logs/$name.log"
    echo "::endgroup::"
    [ "$status" = "ok" ] || echo "build_ports: $name failed" >&2
done

echo "build_ports: $(( ${#names[@]} - broken )) of ${#names[@]} target sets built in ${SECONDS}s"
[ "$broken" -eq 0 ]
