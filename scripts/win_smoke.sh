#!/usr/bin/env bash
# Does a Windows build give the same answer as the Rust port?
#
# The three POSIX ports each have a suite (c/test.sh, cpp/test.sh, zig/test.sh)
# and those are the real coverage. They are written for a POSIX shell and reach
# for binaries by their extensionless names, so they do not run on Windows yet.
# This is the part that can: the same oracle those suites use -- the Rust port,
# which has built and been tested on Windows for some time -- asked the same
# questions about the same files.
#
# It checks the two things a Windows build gets wrong that a Linux one cannot:
#
#   the generator's bytes, because the Microsoft runtime turns a lone \n into
#   \r\n on a text-mode handle, and a generator that does that has quietly
#   stopped writing the same files as the other five;
#
#   a comparison over CSV, ndjson and Parquet, because reading is done through a
#   file mapping, and on Windows that is a section object and a view of it
#   rather than mmap -- a different call with a different failure mode.
#
# Run from anywhere:  scripts/win_smoke.sh c|cpp|zig
# EXE is the binary suffix, `.exe` here and empty if you want to try it on Linux.
set -uo pipefail

port=${1:?usage: win_smoke.sh c|cpp|zig}
cd "$(dirname "$0")/.." || exit 2
EXE=${EXE-.exe}

RUST=rust/target/release/compara$EXE
RGEN=rust/target/release/compara-gen-data$EXE
case "$port" in
  c)   BIN=c/csvdiff$EXE;               GEN=c/gen-data$EXE ;;
  cpp) BIN=cpp/build/csvdiff$EXE;       GEN=cpp/build/gen-data$EXE ;;
  # The Zig port ships no generator, so it is checked against files the Rust one
  # wrote -- which also means there are no bytes of its own to compare.
  zig) BIN=zig/zig-out/bin/csvdiff$EXE; GEN=$RGEN ;;
  *)   echo "unknown port: $port"; exit 2 ;;
esac

for f in "$BIN" "$GEN" "$RUST" "$RGEN"; do
  [ -x "$f" ] || { echo "not built: $f"; exit 2; }
done

fail=0
# A relative directory in the working tree, deliberately, rather than `mktemp -d`.
# On Windows this script runs under MSYS2 while `path-type: inherit` also puts
# Git for Windows' tools on PATH, and the two are separate installations with
# separate roots: MSYS2's `/tmp` is its own `tmp` directory and Git's is the
# user's `%TEMP%`. So a `/tmp/...` path handed to one tool resolves somewhere the
# other cannot see -- which is exactly what happened, and it read as a byte
# difference because `cmp -s` hides the reason it failed. A relative path is
# resolved against the same working directory by all three kinds of binary here:
# MSYS2's, Git's, and the native ports themselves.
tmp=.win-smoke-tmp
rm -rf "$tmp"; mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

# The engine label and the time after it differ by port and by run, and the Rust
# port groups its digits where the others do not. Same normalisation the three
# suites use.
# `sed -E`, not the BRE spelling: BSD sed -- which is the sed on a Mac --
# does not read `\|` as alternation, so the substitution silently matched
# nothing there and every comparison failed on the engine label and the
# timing after it. ERE is the one dialect both seds agree on.
summary() { head -1 | sed -E 's/ \| (turbo|parquet).*//; s/,//g'; }

echo "generator bytes, $port against rust:"
if [ "$GEN" = "$RGEN" ]; then
  echo "  skip  this port has no generator of its own"
else
  # CSV only, and the two exclusions are worth writing down.
  #
  # Parquet, because the ports spell its compression defaults differently and so
  # write differently-named files: a byte comparison there is a question about
  # flags rather than about the platform. Nothing is lost -- a Parquet file with
  # \r\n injected into it cannot be read back, so the answers below catch it.
  #
  # ndjson, because the C and C++ generators already disagree with the Rust one
  # about it, on Linux, today: where a row has no `value_date` they write JSON
  # `null` and Rust writes `""`. The readers treat both as absent so every answer
  # matches, which is why nothing has caught it -- the only cross-generator byte
  # check in the repository is c/test.sh's, and that compares C against C++,
  # which agree. It is a real divergence from the byte-identical claim and it is
  # not this script's to settle; including it here would only report the same
  # known difference on every Windows run.
  #
  # CSV is where the hazard actually lives anyway: \r\n translation happens on a
  # text-mode handle, and that is the file written through one.
  # One format today; a loop so that adding one is a word, not a restructure.
  # shellcheck disable=SC2043
  for fmt in csv; do
    mine=$tmp/mine-$fmt theirs=$tmp/theirs-$fmt
    mkdir -p "$mine" "$theirs"
    "$GEN"  --rows 20k --out-dir "$mine"   --prefix g --format "$fmt" >/dev/null 2>&1
    "$RGEN" --rows 20k --out-dir "$theirs" --prefix g --format "$fmt" >/dev/null 2>&1
    same=1 any=0
    for name in $(cd "$theirs" && ls); do
      any=1
      # Existence first and separately: `cmp -s` returns non-zero both for
      # "these differ" and for "I could not open that", and reporting the second
      # as the first sends the reader hunting for a portability bug that is not
      # there. It cost a round of exactly that.
      [ -r "$mine/$name" ] || { echo "  FAIL  $fmt: $GEN wrote no $name"; same=0; break; }
      cmp -s "$mine/$name" "$theirs/$name" || same=0
    done
    if [ "$any" = 1 ] && [ "$same" = 1 ]; then
      echo "  ok    $fmt"
    else
      # Two different diagnoses, so say enough to tell them apart. A \r\n
      # translation shows up as a length difference and nothing else, which the
      # sizes give away. Equal lengths with different bytes is something else
      # entirely -- the generator computing a different value on this platform --
      # and for that the only useful thing to print is the first line that
      # differs, from both sides.
      echo "  FAIL  $fmt is not byte-identical"
      for name in $(cd "$theirs" && ls); do
        printf '        %-24s mine %s  rust %s\n' "$name" \
               "$(wc -c <"$mine/$name" 2>/dev/null)" "$(wc -c <"$theirs/$name")"
        cmp -s "$mine/$name" "$theirs/$name" && continue
        where=$(cmp "$mine/$name" "$theirs/$name" 2>&1 | head -1)
        printf '        %s\n' "${where#*: }"
        line=$(printf '%s' "$where" | sed -n 's/.*line \([0-9][0-9]*\).*/\1/p')
        if [ -n "$line" ]; then
          printf '          mine: %s\n' "$(sed -n "${line}p" "$mine/$name"   | cut -c1-160)"
          printf '          rust: %s\n' "$(sed -n "${line}p" "$theirs/$name" | cut -c1-160)"
        fi
      done
      fail=1
    fi
  done
fi

echo "answers, $port against rust:"
data=$tmp/data
mkdir -p "$data"
for fmt in csv json parquet; do
  "$RGEN" --rows 20k --out-dir "$data" --prefix "$fmt" --format "$fmt" >/dev/null 2>&1
done
compare() { # label, then the two files and the flags both ports are given
  local label=$1; shift
  local r m
  r=$("$RUST" compare "$@" --engine turbo -o /dev/null 2>&1 | summary) || true
  m=$("$BIN"  compare "$@"                                 2>&1 | summary) || true
  if [ -n "$r" ] && [ "$r" = "$m" ]; then
    printf '  ok    %s: %s\n' "$label" "$m"
  else
    printf '  FAIL  %s\n    rust: %s\n    %-4s: %s\n' "$label" "$r" "$port" "$m"; fail=1
  fi
}
compare csv    "$data/csv_a.csv"        "$data/csv_b.csv"        -k account_id,txn_id -i updated_at
compare ndjson "$data/json_a.ndjson"    "$data/json_b.ndjson"    -k account_id,txn_id -i updated_at
compare parquet "$data/parquet_a.parquet" "$data/parquet_b.parquet" -k account_id,txn_id -i updated_at

# Threads are the other thing that is a different call on Windows -- CreateThread
# in the C port, std::thread in the C++ one -- and a comparison that splits and
# rejoins wrongly gives a wrong count rather than a crash.
compare "csv on one thread"   "$data/csv_a.csv" "$data/csv_b.csv" -k account_id,txn_id -i updated_at --threads 1
compare "csv on four threads" "$data/csv_a.csv" "$data/csv_b.csv" -k account_id,txn_id -i updated_at --threads 4

[ "$fail" = 0 ] && echo "all good" || echo "there were failures"
exit "$fail"
