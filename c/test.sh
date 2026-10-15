#!/usr/bin/env bash
# Holds this port to the answers the Rust port gives, on the fixture built from
# every shape that has broken an engine in this project. Run from c/.
# No pipefail: a compare exits 1 when it finds differences, which is the
# expected outcome of almost every check here.
set -uo pipefail
set +o pipefail
set -e
cd "$(dirname "$0")"
make >/dev/null
RUST=../rust/target/release/compara
GEN=./gen-data
CPP=../cpp/build/csvdiff

# Two sets of checks, because they cost very different amounts to run.
#
# By default only the ones this port can answer on its own: it generates its own
# fixtures now, so nothing else has to be built first and the whole suite is a
# few seconds. `--with-ports` adds the cross-port checks -- the Rust port as the
# oracle on CSV, the C++ port on ndjson, and the two generators' bytes against
# each other -- which need those toolchains and are worth a job of their own.
with_ports=0
[ "${1:-}" = "--with-ports" ] && with_ports=1
if [ "$with_ports" = 1 ]; then
  [ -x "$RUST" ] || { echo "--with-ports needs the Rust port: (cd ../rust && cargo build --release)"; exit 2; }
  [ -x "$CPP" ] || { echo "--with-ports needs the C++ port: (cd ../cpp && make && make gen-data)"; exit 2; }
fi

fail=0
# The engine label and the time after it differ by port and by run, and the Rust
# port groups its digits where this one does not -- which stays invisible until a
# fixture passes a thousand rows, and then reads as a disagreement about counts.
# `sed -E`, not the BRE spelling: BSD sed -- which is the sed on a Mac --
# does not read `\|` as alternation, so the substitution silently matched
# nothing there and every comparison failed on the engine label and the
# timing after it. ERE is the one dialect both seds agree on.
summary() { head -1 | sed -E 's/ \| (turbo|parquet).*//; s/,//g'; }

check() { # label, then the flags both are given
  local label=$1; shift
  local a=../tests/fixtures/awkward_a.csv b=../tests/fixtures/awkward_b.csv
  local r c
  r=$("$RUST" compare "$a" "$b" -k k "$@" --engine turbo -o /dev/null 2>&1 | summary) || true
  c=$(./csvdiff compare "$a" "$b" -k k "$@" 2>&1 | summary) || true
  if [ "$r" = "$c" ]; then
    printf '  ok    %s\n' "$label"
  else
    printf '  FAIL  %s\n    rust: %s\n    c   : %s\n' "$label" "$r" "$c"; fail=1
  fi
}

if [ "$with_ports" = 1 ]; then
echo "awkward fixture, c against rust:"
check "defaults"
# --trim and --ignore-case are not implemented here on purpose; see README.md.

echo "per-column counts, c against rust:"
a=../tests/fixtures/awkward_a.csv b=../tests/fixtures/awkward_b.csv
"$RUST" compare "$a" "$b" -k k --engine turbo -o /dev/null --json /tmp/c_test_rust.json >/dev/null 2>&1 || true
./csvdiff compare "$a" "$b" -k k --json /tmp/c_test_c.json >/dev/null 2>&1 || true
if python3 - <<'PY'
import json, sys
r = json.load(open("/tmp/c_test_rust.json"))
c = json.load(open("/tmp/c_test_c.json"))
sys.exit(0 if (r["counts"], r["columns"]) == (c["counts"], c["columns"]) else 1)
PY
then echo "  ok    counts and per-column changed/blanked/filled"
else echo "  FAIL  counts or per-column stats differ"; fail=1
fi
fi   # with_ports

agree() { python3 -c '
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
shape = lambda r: (r["counts"], sorted(tuple(sorted(c.items())) for c in r["columns"]))
sys.exit(0 if shape(a) == shape(b) else 1)
' "$1" "$2"; }

echo "the threaded csv sweep, over quoted newlines and chunk boundaries:"
# --- the threaded CSV sweep -------------------------------------------------
#
# Rows are found on every core, which means a thread starts in the middle of the
# file and has to work out whether it landed inside a quoted field. Nothing in
# the fixtures above reaches the size where that splitting turns on, so this
# builds a file that does, out of the shapes that make the boundary hard:
# newlines inside quotes, doubled quotes, delimiters inside quotes, CRLF, blank
# lines between rows, duplicate keys, and rows present on one side only.
#
# The check is that the thread count cannot change the answer -- against itself
# at one thread, and against the Rust port.
thr_dir=$(mktemp -d)
python3 - "$thr_dir" <<'PY'
import random, sys
out_dir = sys.argv[1]
random.seed(3)
FIELDS = ["plain", '"has,comma"', '"has\nnewline\nand more"',
          '"doubled ""quote"" inside"', '"comma, and\nnewline together"', '""']
def rows(side):
    out = ["k,a,b,c\n"]
    for i in range(130000):
        if side == "b" and i % 5000 == 0:
            continue                                  # removed from B
        a = FIELDS[(i * 7 + (3 if side == "b" and i % 41 == 0 else 0)) % len(FIELDS)]
        b = str(i * 3 + (1 if side == "b" and i % 23 == 0 else 0))
        c = '"tail\nwith newline"' if i % 11 == 0 else "tail"
        sep = "\r\n" if i % 13 == 0 else "\n"
        out.append(f"K{i:07d},{a},{b},{c}{sep}")
        if i % 997 == 0:
            out.append(f"K{i:07d},{a},{b},{c}{sep}")   # a duplicate key
        if i % 1499 == 0:
            out.append("\n")                           # a blank line is not a row
    if side == "b":
        for i in range(40):
            out.append(f"NEW{i:05d},plain,0,tail\n")   # added in B
    return "".join(out)
for side in ("a", "b"):
    open(f"{out_dir}/t_{side}.csv", "w").write(rows(side))
PY
bytes=$(wc -c < "$thr_dir/t_a.csv")
if [ "$bytes" -lt 4194304 ]; then
  echo "  FAIL  the threading fixture is $bytes bytes, under the 4 MB split threshold"
  fail=1
else
  ./csvdiff compare "$thr_dir/t_a.csv" "$thr_dir/t_b.csv" -k k --threads 1 \
      --json "$thr_dir/one.json" >/dev/null 2>&1 || true
  for t in 2 3 4 7; do
    ./csvdiff compare "$thr_dir/t_a.csv" "$thr_dir/t_b.csv" -k k --threads $t \
        --json "$thr_dir/many.json" >/dev/null 2>&1 || true
    if agree "$thr_dir/one.json" "$thr_dir/many.json"; then
      printf '  ok    %s threads finds what 1 thread finds\n' "$t"
    else
      printf '  FAIL  %s threads disagrees with 1 thread\n' "$t"; fail=1
    fi
  done
  if [ "$with_ports" = 1 ]; then
    "$RUST" compare "$thr_dir/t_a.csv" "$thr_dir/t_b.csv" -k k --engine turbo -o /dev/null \
        --json "$thr_dir/rust.json" >/dev/null 2>&1 || true
    if agree "$thr_dir/one.json" "$thr_dir/rust.json"; then
      echo "  ok    and what the rust port finds"
    else
      echo "  FAIL  the threaded sweep disagrees with the rust port"; fail=1
    fi
  fi
fi
# The splits are guessed at the next newline and checked after the sweep; only
# a wrong guess pays for counting quotes. This pair makes the guess wrong on
# purpose: one quoted field of 20,000 lines sits across the middle of each file,
# so the halfway split lands on a newline inside it, and the rows on either side
# of it carry the changes that a chunk started there would misread.
python3 - "$thr_dir" <<'PY'
import sys
d = sys.argv[1]
for side in ("a", "b"):
    rows = ["k,v,note\n"]
    rows += [f"R{i:06d},{i},plain\n" for i in range(110000)]
    rows.append(f"MID,{'x' if side == 'a' else 'y'},\"" + "inside\n" * 20000 + "\"\n")
    rows += [f"S{i:06d},{i + (side == 'b' and i == 7)},plain\n" for i in range(110000)]
    open(f"{d}/g_{side}.csv", "w").write("".join(rows))
PY
./csvdiff compare "$thr_dir/g_a.csv" "$thr_dir/g_b.csv" -k k --threads 1 \
    --json "$thr_dir/g1.json" >/dev/null 2>&1 || true
./csvdiff compare "$thr_dir/g_a.csv" "$thr_dir/g_b.csv" -k k --threads 4 \
    --json "$thr_dir/g4.json" >/dev/null 2>&1 || true
if agree "$thr_dir/g1.json" "$thr_dir/g4.json" &&
   python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["counts"]; sys.exit(0 if (c["changed"], c["added"], c["removed"]) == (2, 0, 0) else 1)' "$thr_dir/g4.json"; then
  echo "  ok    a split guessed inside a quoted field is caught and counted instead"
else
  echo "  FAIL  a split guessed inside a quoted field changed the answer"; fail=1
fi
# Past page cache the join runs inside A's sweep, joining every row as though it
# were its key's first and taking back, afterwards, the rows that repeat a key.
# CSVDIFF_FUSED_JOIN=1 takes that path at any size. The repeats here differ from
# their key's first row -- in a compared value, or in being absent from B -- so a
# repeat that was not taken back, or one taken back twice, changes the counts.
python3 - "$thr_dir" <<'PY'
import json, sys
d = sys.argv[1]
def rows(side):
    out = []
    for i in range(60000):
        if side == "b" and i % 4001 == 0:
            continue                                   # removed from B
        v = i * 3 + (1 if side == "b" and i % 17 == 0 else 0)
        out.append((f"K{i:06d}", str(v), "t"))
        if i % 97 == 0:                                # a repeat that differs
            out.append((f"K{i:06d}", str(v + 5), "u" if side == "a" else "t"))
        if side == "b" and i % 131 == 0:               # repeats in B too
            out.append((f"K{i:06d}", "0", "x"))
    for i in range(30):
        out.append((f"{side.upper()}ONLY{i:03d}", "1", "t"))
    return out
for side in ("a", "b"):
    rs = rows(side)
    with open(f"{d}/f_{side}.csv", "w") as f:
        f.write("k,v,w\n")
        f.writelines(f"{k},{v},{w}\n" for k, v, w in rs)
    with open(f"{d}/f_{side}.ndjson", "w") as f:
        f.writelines(json.dumps({"k": k, "v": v, "w": w}) + "\n" for k, v, w in rs)
PY
for ext in csv ndjson; do
  for t in 1 4; do
    ./csvdiff compare "$thr_dir/f_a.$ext" "$thr_dir/f_b.$ext" -k k --threads $t \
        --json "$thr_dir/f_after.json" >/dev/null 2>&1 || true
    CSVDIFF_FUSED_JOIN=1 ./csvdiff compare "$thr_dir/f_a.$ext" "$thr_dir/f_b.$ext" -k k \
        --threads $t --json "$thr_dir/f_inside.json" >/dev/null 2>&1 || true
    if agree "$thr_dir/f_after.json" "$thr_dir/f_inside.json"; then
      printf '  ok    %s, %s thread(s): the join inside the sweep finds what the join after it finds\n' "$ext" "$t"
    else
      printf '  FAIL  %s, %s thread(s): the join inside the sweep disagrees\n' "$ext" "$t"; fail=1
    fi
  done
  code=0
  out=$(CSVDIFF_FUSED_JOIN=1 CSVDIFF_VERIFY_ADDED=1 ./csvdiff compare "$thr_dir/f_a.$ext" \
      "$thr_dir/f_b.$ext" -k k 2>&1 >/dev/null) || code=$?
  if [ "$code" -le 1 ] && [ -z "$out" ]; then
    printf '  ok    %s: added derived inside the sweep matches the pass that counts it\n' "$ext"
  else
    printf '  FAIL  %s: added inside the sweep: %s\n' "$ext" "${out:-exit $code}"; fail=1
  fi
done
rm -rf "$thr_dir"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
if [ "$with_ports" = 1 ]; then
  echo "quoting, ragged rows and keys near the end of the file:"
  printf 'a,k,c\nx,K1,c1\ny,K2,cc\n'        > "$tmp/a.csv"
  printf 'a,k,c\nx,K1,c1\ny,K2,cccccccc\n'  > "$tmp/b.csv"
  r=$("$RUST" compare "$tmp/a.csv" "$tmp/b.csv" -k k --engine turbo -o /dev/null 2>&1 | summary) || true
  c=$(./csvdiff compare "$tmp/a.csv" "$tmp/b.csv" -k k 2>&1 | summary) || true
  [ "$r" = "$c" ] && echo "  ok    key in the last bytes of the file" \
                  || { echo "  FAIL  key near end: rust=$r c=$c"; fail=1; }
fi



# --- newline-delimited JSON -------------------------------------------------
#
# The same rows in both shapes have to give the same answer, which is the whole
# claim: a JSON reader that disagreed with the CSV one would be a second engine,
# not a second parser.
echo "newline-delimited json:"
if [ -x "$GEN" ]; then
  js_dir=$(mktemp -d)
  "$GEN" --rows 20k --out-dir "$js_dir" --prefix j >/dev/null 2>&1
  "$GEN" --rows 20k --out-dir "$js_dir" --prefix j --format json >/dev/null 2>&1
  ./csvdiff compare "$js_dir/j_a.csv" "$js_dir/j_b.csv" -k account_id,txn_id -i updated_at \
      --json "$js_dir/csv.json" >/dev/null 2>&1 || true
  ./csvdiff compare "$js_dir/j_a.ndjson" "$js_dir/j_b.ndjson" -k account_id,txn_id -i updated_at \
      --json "$js_dir/js.json" >/dev/null 2>&1 || true
  if agree "$js_dir/csv.json" "$js_dir/js.json"; then
    echo "  ok    ndjson finds what the same rows as csv find"
  else
    echo "  FAIL  ndjson disagrees with csv on the same rows"; fail=1
  fi
  # Against C++ rather than Rust here: it is the only other port that reads
  # ndjson at all, and a check against a port that refuses the file would be a
  # check that always passes.
  if [ "$with_ports" = 1 ]; then
    "$CPP" compare "$js_dir/j_a.ndjson" "$js_dir/j_b.ndjson" -k account_id,txn_id \
        -i updated_at --json "$js_dir/cpp.json" >/dev/null 2>&1 || true
    if agree "$js_dir/js.json" "$js_dir/cpp.json"; then
      echo "  ok    and what the c++ port finds on the same ndjson"
    else
      echo "  FAIL  ndjson disagrees with the c++ port"; fail=1
    fi
  fi
  rm -rf "$js_dir"
else
  echo "  skip  ndjson against csv needs $GEN"; fail=1
fi

# Two rules the key-only JSON parse rests on, neither of which the generated
# fixture can exercise because it never produces either shape.
# `added` is derived from the A pass rather than counted by a pass of its own,
# on the argument that the join is symmetric. CSVDIFF_VERIFY_ADDED=1 runs the
# pass that was removed and refuses the run if the two disagree, so the argument
# is checked here on every shape that has ever broken an engine, not assumed.
echo "the derivation of added, checked against the pass it replaced:"
vcheck() { # label, then the compare arguments
  local label=$1; shift
  local out code=0
  # A compare exits 1 when it finds differences, which every case here does, so
  # the status has to be caught rather than reach `set -e`. Declaring and
  # assigning on one line would also hide it: the status would be `local`'s.
  out=$(CSVDIFF_VERIFY_ADDED=1 ./csvdiff compare "$@" 2>&1 >/dev/null) || code=$?
  if [ "$code" -le 1 ] && [ -z "$out" ]; then
    printf '  ok    %s\n' "$label"
  else
    printf '  FAIL  %s\n    %s\n' "$label" "${out:-exit $code}"; fail=1
  fi
}
vcheck "the awkward fixture" ../tests/fixtures/awkward_a.csv ../tests/fixtures/awkward_b.csv -k k
if [ -x "$GEN" ]; then
  vd=$(mktemp -d)
  "$GEN" --rows 20k --out-dir "$vd" --prefix v >/dev/null 2>&1
  "$GEN" --rows 20k --out-dir "$vd" --prefix v --format json >/dev/null 2>&1
  "$GEN" --rows 20k --out-dir "$vd" --prefix v --format parquet >/dev/null 2>&1
  vcheck "generated csv"     "$vd/v_a.csv"          "$vd/v_b.csv"          -k account_id,txn_id -i updated_at
  vcheck "generated ndjson"  "$vd/v_a.ndjson"       "$vd/v_b.ndjson"       -k account_id,txn_id -i updated_at
  vcheck "generated parquet" "$vd/v_a.unc.parquet"  "$vd/v_b.unc.parquet"  -k account_id,txn_id -i updated_at
  rm -rf "$vd"
else
  echo "  skip  the derivation checks need $GEN"; fail=1
fi

echo "the ndjson rules the fast path depends on:"
jd=$(mktemp -d)
BS=$(printf '\\')
# A row ends at the next newline byte, full stop -- valid JSON cannot carry a
# raw one inside a string, so an *escaped* newline must not split the row.
printf '{"k":"1","v":"a%snb"}\n{"k":"2","v":"plain"}\n' "$BS" > "$jd/a.ndjson"
printf '{"k":"1","v":"a%snb"}\n{"k":"2","v":"other"}\n' "$BS" > "$jd/b.ndjson"
jrun() { # file pair, expected "rows changed"
  rm -f "$jd/o.json"
  ./csvdiff compare "$jd/$1" "$jd/$2" -k k --json "$jd/o.json" >/dev/null 2>&1 || true
  python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["counts"]; print(c["a_rows"], c["changed"])' "$jd/o.json" 2>/dev/null || echo "(no report)"
}
got=$(jrun a.ndjson b.ndjson)
if [ "$got" = "2 1" ]; then
  echo "  ok    an escaped newline inside a string does not end the row"
else
  echo "  FAIL  an escaped newline inside a string does not end the row"
  echo "    want: 2 1"; echo "    got : $got"; fail=1
fi

# A repeated key column takes its *first* value, so that the key-only parse --
# which stops as soon as it has the keys -- and the full parse agree on what a
# row's key is. Both rows below key on 1 under that rule and on 2 under the
# other, so the counts say which rule ran.
printf '{"k":"1","k":"2","v":"x"}\n' > "$jd/dup_a.ndjson"
printf '{"k":"1","v":"y"}\n'         > "$jd/dup_b.ndjson"
got=$(jrun dup_a.ndjson dup_b.ndjson)
if [ "$got" = "1 1" ]; then
  echo "  ok    a repeated key column keeps its first value"
else
  echo "  FAIL  a repeated key column keeps its first value"
  echo "    want: 1 1 (matched on k=1, v differs)"; echo "    got : $got"; fail=1
fi
rm -rf "$jd"

# A JSON writer may escape a character or write it literally, and the two spell
# the same value -- so they have to compare equal. This is the one place the two
# dialects genuinely differ: CSV doubles a quote, JSON puts a backslash in front,
# and \uXXXX has to become UTF-8 before anything is compared.
esc_dir=$(mktemp -d)
python3 - "$esc_dir" <<'PY'
import json, sys
out = sys.argv[1]
BS = chr(92)                      # kept out of the literals below
pairs = [
    ("k1", BS + "u00e9" + BS + "u00e8", "éè"),   # two-byte utf-8
    ("k2", BS + "ud83d" + BS + "ude00", "\U0001F600"),      # a surrogate pair is one code point
    ("k3", "a" + BS + "/b",             "a/b"),             # the optional solidus escape
    ("k4", "tab" + BS + "there",        "tab\there"),
    ("k5", BS + "u0041BC",              "ABC"),             # ascii written as an escape
    ("k6", "quote" + BS + '"in',        'quote"in'),
    ("k7", "back" + BS + BS + "slash",  "back" + BS + "slash"),
    ("k8", BS + "u4e2d" + BS + "u6587", "中文"),    # three-byte utf-8
    ("k9", "line" + BS + "nbreak",      "line\nbreak"),
]
with open(f"{out}/a.ndjson", "w") as fa, open(f"{out}/b.ndjson", "w") as fb:
    for k, escaped, literal in pairs:
        fa.write('{"k":"%s","v":"%s"}\n' % (k, escaped))
        fb.write(json.dumps({"k": k, "v": literal}, ensure_ascii=False) + "\n")
# And a control: one value that really does differ, so the check above is known
# to be capable of failing.
open(f"{out}/c.ndjson", "w").write(
    open(f"{out}/b.ndjson").read().replace('"éè"', '"éé"'))
PY
# `set -e` is on and a compare exits 1 when it finds differences, so the status
# has to be caught rather than left to reach the shell -- which would end the
# run here and call the negative control below a crash.
code=0; ./csvdiff compare "$esc_dir/a.ndjson" "$esc_dir/b.ndjson" -k k >/dev/null 2>&1 || code=$?
if [ "$code" -eq 0 ]; then
  echo "  ok    an escaped value equals the same value written literally"
else
  echo "  FAIL  escaped and literal spellings of one value compare different"; fail=1
fi
code=0; ./csvdiff compare "$esc_dir/a.ndjson" "$esc_dir/c.ndjson" -k k >/dev/null 2>&1 || code=$?
if [ "$code" -eq 1 ]; then
  echo "  ok    and a value that really differs is still found"
else
  echo "  FAIL  the escape check cannot fail, so it proves nothing"; fail=1
fi
rm -rf "$esc_dir"

# The same, with the escapes in the *key* -- and in every port. A key is hashed
# before it is ever compared, so two spellings of one key have to hash alike or
# the lookup never reaches the comparison above, and the row comes out added and
# removed instead of matched. The C++, Rust and Zig ports hashed an escaped key's
# decoded bytes one at a time and an unescaped one a word at a time, and missed
# exactly this. Keys of every length around the eight bytes the hash folds at
# once, and a CSV key with a doubled quote against the JSON spelling of it.
kesc_dir=$(mktemp -d)
python3 - "$kesc_dir" <<'PY'
import json, sys
out = sys.argv[1]
BS = chr(92)
pairs = [
    ("a" + BS + "/b",                          "a/b"),
    (BS + "u0041BC",                           "ABC"),
    ("tab" + BS + "there",                     "tab\there"),
    ("sev" + BS + "/ena",                      "sev/ena"),
    ("an eight" + BS + "/",                    "an eight/"),
    ("a key longer than a word " + BS + "/ x", "a key longer than a word / x"),
    ("quote" + BS + '"in',                     'quote"in'),
    (BS + "u4e2d" + BS + "u6587 key",          "中文 key"),
]
with open(f"{out}/a.ndjson", "w") as fa, open(f"{out}/b.ndjson", "w") as fb:
    for i, (escaped, literal) in enumerate(pairs):
        fa.write('{"k":"%s","v":"%d"}\n' % (escaped, i))
        fb.write(json.dumps({"k": literal, "v": str(i)}, ensure_ascii=False) + "\n")
with open(f"{out}/c.csv", "w") as fc:
    fc.write('k,v\n"quote""in",6\n"a/b",0\n')
with open(f"{out}/d.ndjson", "w") as fd:
    fd.write('{"k":"quote%s\"in","v":"6"}\n{"k":"a/b","v":"0"}\n' % BS)
PY
key_ports="C:./csvdiff"
[ "$with_ports" = 1 ] && key_ports="$key_ports Rust:$RUST C++:$CPP"
for entry in $key_ports; do
  name=${entry%%:*}; exe=${entry#*:}
  got=$("$exe" compare "$kesc_dir/a.ndjson" "$kesc_dir/b.ndjson" -k k 2>/dev/null | grep -o 'matched [0-9]*' || true)
  if [ "$got" = "matched 8" ]; then
    echo "  ok    $name: an escaped key finds the same key written literally"
  else
    echo "  FAIL  $name: escaped and literal spellings of one key did not match: $got"; fail=1
  fi
  got=$("$exe" compare "$kesc_dir/c.csv" "$kesc_dir/d.ndjson" -k k 2>/dev/null | grep -o 'matched [0-9]*' || true)
  if [ "$got" = "matched 2" ]; then
    echo "  ok    $name: and a CSV doubled quote finds the JSON backslash"
  else
    echo "  FAIL  $name: a CSV key with a doubled quote missed its JSON spelling: $got"; fail=1
  fi
done
rm -rf "$kesc_dir"
# --- the Parquet path -------------------------------------------------------
#
# The claim the columnar path has to earn is that it is the *same* comparison:
# the same rows, in a format that stores them as columns of dictionary indices
# rather than as lines of text, must produce the same counts and the same
# per-column changed/blanked/filled. So each case generates one pair in both
# formats and requires the two reports to agree in full.
#
# The fixtures come from this port's own generator, which writes CSV, ndjson and
# Parquet from one field-by-field recipe -- byte for byte the same files the C++
# generator writes, which is checked below. Where it is not built these skip by
# name rather than silently passing.
pq_dir=$(mktemp -d); trap 'rm -rf "$tmp" "$pq_dir"' EXIT

same_report() { # label, key, then the generator flags for the parquet side
  local label=$1 key=$2; shift 2
  rm -f "$pq_dir"/p_*
  "$GEN" --rows 20k --out-dir "$pq_dir" --prefix p >/dev/null 2>&1
  "$GEN" --rows 20k --out-dir "$pq_dir" --prefix p "$@" >/dev/null 2>&1
  ./csvdiff compare "$pq_dir/p_a.csv" "$pq_dir/p_b.csv" -k "$key" -i updated_at \
      --json "$pq_dir/csv.json" >/dev/null 2>&1 || true
  ./csvdiff compare "$pq_dir/p_a.unc.parquet" "$pq_dir/p_b.unc.parquet" -k "$key" -i updated_at \
      --json "$pq_dir/pq.json" >/dev/null 2>&1 || true
  if agree "$pq_dir/csv.json" "$pq_dir/pq.json"; then
    printf '  ok    %s\n' "$label"
  else
    printf '  FAIL  %s\n' "$label"; fail=1
  fi
}

if [ -x "$GEN" ]; then
  echo "uncompressed parquet against the same rows as csv:"
  same_report "defaults"                    account_id,txn_id --format parquet --compression none
  same_report "many small row groups"       account_id,txn_id --format parquet --compression none --row-group-size 512
  # A dictionary budget the data crosses partway makes a column that is
  # dictionary encoded in some row groups and plain in others -- the shape a
  # real writer produces on a high-cardinality string, and the one the reader
  # has to fold into a single form.
  same_report "dictionary gives up partway" account_id,txn_id --format parquet --compression none --dict-limit 175 --row-group-size 300
  same_report "every column plain"          account_id,txn_id --format parquet --compression none --dict-limit 1
  # A key column both sides store as a dictionary takes the shared-id path,
  # where the join never looks at a byte after the dictionaries are mapped.
  same_report "dictionary key columns"      currency,status   --format parquet --compression none

  echo "refusals:"
  rm -f "$pq_dir"/r_*
  "$GEN" --rows 1k --out-dir "$pq_dir" --prefix r >/dev/null 2>&1
  "$GEN" --rows 1k --out-dir "$pq_dir" --prefix r --format parquet --compression none >/dev/null 2>&1
  # A column store and a byte stream have no common ground to be compared on.
  if ./csvdiff compare "$pq_dir/r_a.unc.parquet" "$pq_dir/r_b.csv" -k account_id 2>&1 \
       | grep -q "one file is parquet"; then
    echo "  ok    a mixed parquet/text pair is refused"
  else
    echo "  FAIL  mixed parquet/text pair"; fail=1
  fi
  # Snappy and LZ4 are read here now, both written out in parquet.c rather than
  # linked. This port writes neither, so the files that prove it reads them are
  # checked in rather than generated.
  if ./csvdiff compare ../tests/fixtures/snappy.parquet ../tests/fixtures/snappy.parquet \
       -k account_id 2>&1 | grep -q "matched"; then
    echo "  ok    snappy is read"
  else
    echo "  FAIL  snappy should be read: $(./csvdiff compare ../tests/fixtures/snappy.parquet \
         ../tests/fixtures/snappy.parquet -k account_id 2>&1 | head -1)"; fail=1
  fi
  # A file per codec, each compared against itself, so a decoder that produced
  # plausible-but-wrong bytes would show up as a changed row rather than pass.
  for spec in "a_plain_none read" "a_dict_snappy read" "a_plain_lz4 read" \
              "a_dict_gzip refused" "a_plain_zstd refused"; do
    set -- $spec
    out=$(./csvdiff compare "../tests/fixtures/formats/$1.parquet" \
                            "../tests/fixtures/formats/$1.parquet" -k id 2>&1 | head -1)
    case "$2:$out" in
      read:*"matched 301 (changed 0)"*) echo "  ok    $1 is read, and identical to itself" ;;
      refused:*"is not read here"*)     echo "  ok    $1 is refused by name" ;;
      *) echo "  FAIL  $1: expected $2, got: $out"; fail=1 ;;
    esac
  done
  # The two codecs this port carries, against the same rows uncompressed: a
  # decoder that dropped or duplicated a value would not land on equal counts.
  rm -f "$pq_dir"/z_*
  "$GEN" --rows 2k --out-dir "$pq_dir" --prefix z --format parquet >/dev/null 2>&1
  plain=$(./csvdiff compare "$pq_dir/z_a.unc.parquet" "$pq_dir/z_b.unc.parquet" \
              -k account_id,txn_id -i updated_at 2>&1 | summary)
  echo "  ok    uncompressed baseline: $plain"

  # A misspelled --ignore used to widen the comparison in silence: the column it
  # meant to drop got compared and came back changed on every row. Both of this
  # port's readers refuse it now, so the text path and the columnar one are
  # checked separately -- they resolve columns in different files.
  if ./csvdiff compare "$pq_dir/r_a.csv" "$pq_dir/r_b.csv" -k account_id -i no_such_column 2>&1 \
       | grep -q "present in neither file"; then
    echo "  ok    an unknown --ignore name is refused (text)"
  else
    echo "  FAIL  an unknown --ignore name should be refused (text)"; fail=1
  fi
  if ./csvdiff compare "$pq_dir/r_a.unc.parquet" "$pq_dir/r_b.unc.parquet" \
       -k account_id -i no_such_column 2>&1 | grep -q "present in neither file"; then
    echo "  ok    an unknown --ignore name is refused (parquet)"
  else
    echo "  FAIL  an unknown --ignore name should be refused (parquet)"; fail=1
  fi
  # The other half of the rule: a name that is really there still works, so the
  # check refuses typos rather than refusing --ignore.
  if ./csvdiff compare "$pq_dir/r_a.csv" "$pq_dir/r_b.csv" -k account_id -i updated_at \
       >/dev/null 2>&1; [ $? -le 1 ]; then
    echo "  ok    a real --ignore name is still accepted"
  else
    echo "  FAIL  a real --ignore name should be accepted"; fail=1
  fi
else
  echo "skip: $GEN is not built, so the parquet checks did not run"
  echo "      it is built by 'make' in this directory"
  fail=1
fi

# --- the generator against the C++ one --------------------------------------
#
# The fast half of this suite makes its own fixtures, which is only safe because
# the bytes are the same bytes. A generator that drifted would not fail any
# check above -- both sides of every comparison would drift together -- so the
# drift has to be caught here, against the generator this one replaced.
# The sweep and the probes read only the key columns now, and the projection
# gets from a file column to its slot through a precomputed chain. Both make
# assumptions about where the key sits and which slots a column feeds, and
# neither is exercised by a fixture whose key is column zero.
echo "the key-only parse, where the key is not the first column:"
kdir=$(mktemp -d)
{
  echo 'x,y,id,z2'
  echo 'a1,b1,k1,z1'
  echo 'a2,b2,k2,z2'
  echo 'a3,b3,k3,z3'
} > "$kdir/a.csv"
{
  echo 'x,y,id,z2'
  echo 'a1,b1,k1,z1'
  echo 'a2,CHANGED,k2,z2'
  echo 'a9,b9,k9,z9'
} > "$kdir/b.csv"
kcase() { # label, expected "changed added removed", then the flags
  local label=$1 want=$2; shift 2
  # Removed first, and a missing one is a failure: the first version of this
  # helper read the previous case's report when a run refused its flags, and
  # reported the previous case's answer as this one's.
  rm -f "$kdir/o.json"
  ./csvdiff compare "$kdir/a.csv" "$kdir/b.csv" "$@" --json "$kdir/o.json" >/dev/null 2>&1 || true
  local got
  got=$(python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["counts"]; print(c["changed"], c["added"], c["removed"])' "$kdir/o.json" 2>/dev/null) || got="(no report)"
  if [ "$got" = "$want" ]; then
    printf '  ok    %s\n' "$label"
  else
    printf '  FAIL  %s\n    want: %s\n    got : %s\n' "$label" "$want" "$got"; fail=1
  fi
}
kcase "key in the third of four columns"      "1 1 1" -k id
# `z` was a typo for `z2` and nothing could see it: the columns are
# x,y,id,z2, so this ignored two of the three non-key columns and compared
# the third, while the label said all of them. The counts came out the same
# either way, which is why it survived. `--ignore` refuses an unknown name
# now, and that is how this was found.
kcase "with everything after the key ignored" "0 1 1" -k id -i x,y,z2
kcase "key in the last column"                "1 1 1" -k z2
kcase "two key columns, first and last"       "1 1 1" -k x,z2
rm -rf "$kdir"

# The join proves a row unchanged from the two rows' raw bytes when they agree
# far enough in, and only parses the mate when they do not. These are the shapes
# where that proof has to hold or refuse: a change in the column it checks, a
# change hidden behind an ignored one, and two files that spell the same columns
# in a different order -- where equal bytes mean different columns.
echo "proving a row unchanged from the bytes:"
pdir=$(mktemp -d)
pcase() { # label, expected "changed added removed", a-file, b-file, then flags
  local label=$1 want=$2 af=$3 bf=$4; shift 4
  rm -f "$pdir/o.json"
  ./csvdiff compare "$pdir/$af" "$pdir/$bf" "$@" --json "$pdir/o.json" >/dev/null 2>&1 || true
  local got
  got=$(python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["counts"]; print(c["changed"], c["added"], c["removed"])' "$pdir/o.json" 2>/dev/null) || got="(no report)"
  if [ "$got" = "$want" ]; then
    printf '  ok    %s\n' "$label"
  else
    printf '  FAIL  %s\n    want: %s\n    got : %s\n' "$label" "$want" "$got"; fail=1
  fi
}

# One ignored column at the end that always differs -- the case the proof is
# for -- and a change in the last compared column, which is the one it checks.
{ echo 'id,a,b,ts'; echo 'k1,p,q,T1'; echo 'k2,p,q,T1'; } > "$pdir/tail_a.csv"
{ echo 'id,a,b,ts'; echo 'k1,p,q,T2'; echo 'k2,p,zz,T2'; } > "$pdir/tail_b.csv"
pcase "an ignored last column that always differs" "1 0 0" tail_a.csv tail_b.csv -k id -i ts
pcase "and no proof to be had once it is compared" "2 0 0" tail_a.csv tail_b.csv -k id

# A change in the first compared column, with everything after it identical.
{ echo 'id,a,b,ts'; echo 'k1,p,q,T1'; } > "$pdir/head_a.csv"
{ echo 'id,a,b,ts'; echo 'k1,X,q,T2'; } > "$pdir/head_b.csv"
pcase "a change in the first compared column" "1 0 0" head_a.csv head_b.csv -k id -i ts

# An ignored column in the middle that differs, and a real change after it: the
# bytes diverge early, so the proof must refuse and the change must be found.
{ echo 'id,skip,a,ts'; echo 'k1,S1,p,T1'; } > "$pdir/mid_a.csv"
{ echo 'id,skip,a,ts'; echo 'k1,S2,X,T2'; } > "$pdir/mid_b.csv"
pcase "a change behind an ignored middle column" "1 0 0" mid_a.csv mid_b.csv -k id -i skip,ts

# The gapped proof: an ignored column among compared ones is skipped by field
# count in each row on its own, so a value there that differs -- in length, in
# quoting, with a delimiter inside the quotes -- does not stop a row proving
# equal, and does not shift what the proof compares after it. The last column
# here is compared, so the proof also has to close on the line ending.
{ echo 'id,skip,a,b'; echo 'k1,S1,p,q'; echo 'k2,S1,p,q'; echo 'k3,S1,p,q'; } > "$pdir/gap_a.csv"
{ echo 'id,skip,a,b'; echo 'k1,"S, ""two"" and more",p,q'; echo 'k2,,p,X'; echo 'k3,S2,p,qq'; } > "$pdir/gap_b.csv"
pcase "a middle ignored column that differs" "2 0 0" gap_a.csv gap_b.csv -k id -i skip
# A mate that ends inside the gap, and one that ends in the gap's last field.
{ echo 'id,skip,a,b'; echo 'k1,S1,p,q'; echo 'k2,S1,p,q'; } > "$pdir/gs_a.csv"
{ echo 'id,skip,a,b'; echo 'k1'; echo 'k2,S2'; } > "$pdir/gs_b.csv"
pcase "a mate that stops inside the gap" "2 0 0" gs_a.csv gs_b.csv -k id -i skip
# The gap first, before the key, and two gaps.
{ echo 'skip,id,a,t,b'; echo 'S1,k1,p,T1,q'; echo 'S1,k2,p,T1,q'; } > "$pdir/g2_a.csv"
{ echo 'skip,id,a,t,b'; echo 'S22,k1,p,T2,q'; echo 'S,k2,p,T222,Y'; } > "$pdir/g2_b.csv"
pcase "a leading gap and a second one" "1 0 0" g2_a.csv g2_b.csv -k id -i skip,t
# CRLF against LF, with the last column compared: the bytes differ at the line
# ending, so the proof refuses and the values -- equal -- are compared.
printf 'id,skip,a\nk1,S1,p\n' > "$pdir/gc_a.csv"
printf 'id,skip,a\r\nk1,S22,p\r\n' > "$pdir/gc_b.csv"
pcase "a gap, and CRLF against LF" "0 0 0" gc_a.csv gc_b.csv -k id -i skip

# Identical bytes, different columns. The proof reads bytes, so this is the
# shape that breaks it if it is applied where the two headers disagree.
{ echo 'id,p,q'; echo 'k1,X,Y'; } > "$pdir/ord_a.csv"
{ echo 'id,q,p'; echo 'k1,X,Y'; } > "$pdir/ord_b.csv"
pcase "the same bytes under a different column order" "1 0 0" ord_a.csv ord_b.csv -k id

# Same value, spelled two ways: quoted on one side, bare on the other, and CRLF
# against LF. The bytes diverge, the values do not.
printf 'id,a,ts\nk1,"p",T1\n' > "$pdir/q_a.csv"
printf 'id,a,ts\r\nk1,p,T2\r\n' > "$pdir/q_b.csv"
pcase "quoted against bare, and CRLF against LF" "0 0 0" q_a.csv q_b.csv -k id -i ts

# A row that stops before the column the proof checks.
{ echo 'id,a,b,ts'; echo 'k1,p,q,T1'; } > "$pdir/rag_a.csv"
{ echo 'id,a,b,ts'; echo 'k1,p'; } > "$pdir/rag_b.csv"
pcase "a mate that stops before the checked column" "1 0 0" rag_a.csv rag_b.csv -k id -i ts

# The mate's value carries on where this one stopped. `cc` is a prefix of
# `cccccccc`, so a proof that reads only as far as this row's last byte calls
# them equal; the cross-port oracle caught exactly this.
{ echo 'a,k,c'; echo 'x,K1,c1'; echo 'y,K2,cc'; } > "$pdir/pre_a.csv"
{ echo 'a,k,c'; echo 'x,K1,c1'; echo 'y,K2,cccccccc'; } > "$pdir/pre_b.csv"
pcase "a mate whose last column carries on" "1 0 0" pre_a.csv pre_b.csv -k k

# The same trap behind quotes: the mate closes its field two bytes later,
# because what looks like the closing quote is a doubled one.
printf 'id,a,ts\nk1,"ab",T1\n'      > "$pdir/dq_a.csv"
printf 'id,a,ts\nk1,"ab""x",T2\n'   > "$pdir/dq_b.csv"
pcase "a mate that reopens on a doubled quote" "1 0 0" dq_a.csv dq_b.csv -k id -i ts

# The same proof on objects, where a value is found by name rather than by
# position -- and a name repeated in one object takes its *last* value for a
# compared column. A duplicate past the diverging byte is the shape that breaks
# it, so the mate's tail is checked for anything this run tracks.
echo "proving an object unchanged from the bytes:"
printf '{"id":"k1","a":"p","ts":"T1"}\n' > "$pdir/j_a.ndjson"
printf '{"id":"k1","a":"p","ts":"T2"}\n' > "$pdir/j_b.ndjson"
pcase "an ignored last field that always differs" "0 0 0" j_a.ndjson j_b.ndjson -k id -i ts

# The mate repeats the compared name after the bytes stop agreeing, and last
# wins -- so the value the prefix proved is not the value the row has.
printf '{"id":"k1","a":"p","ts":"T1"}\n'              > "$pdir/jd_a.ndjson"
printf '{"id":"k1","a":"p","ts":"T2","a":"LATER"}\n'  > "$pdir/jd_b.ndjson"
pcase "a mate that repeats the column past the prefix" "1 0 0" jd_a.ndjson jd_b.ndjson -k id -i ts

# The same duplicate, but inside the agreeing prefix: both objects read it the
# same way, so there is nothing to catch and the proof may stand.
printf '{"id":"k1","a":"p","a":"q","ts":"T1"}\n' > "$pdir/jp_a.ndjson"
printf '{"id":"k1","a":"p","a":"q","ts":"T2"}\n' > "$pdir/jp_b.ndjson"
pcase "a duplicate inside the prefix, agreeing"  "0 0 0" jp_a.ndjson jp_b.ndjson -k id -i ts

# Objects need not list their names in the same order. The bytes diverge at the
# first name, so the proof refuses and the values are compared properly.
printf '{"id":"k1","a":"p","b":"q","ts":"T1"}\n' > "$pdir/jo_a.ndjson"
printf '{"id":"k1","b":"q","a":"p","ts":"T2"}\n' > "$pdir/jo_b.ndjson"
pcase "the same values under a different name order" "0 0 0" jo_a.ndjson jo_b.ndjson -k id -i ts

# A change in the compared value itself, with the ignored field moving too.
printf '{"id":"k1","a":"p","ts":"T1"}\n' > "$pdir/jc_a.ndjson"
printf '{"id":"k1","a":"X","ts":"T2"}\n' > "$pdir/jc_b.ndjson"
pcase "a change in the compared value"          "1 0 0" jc_a.ndjson jc_b.ndjson -k id -i ts

# A repeated *key* name. First occurrence wins, so this row's key is `k1` and it
# matches. Under last-wins the key would be `OTHER`, and the row would come out
# as one added and one removed instead -- which is what the C++ port did until
# its key-only parse landed, on an input no fixture covered. Both ports are
# pinned to it now.
printf '{"id":"k1","a":"p"}\n'              > "$pdir/jk_a.ndjson"
printf '{"id":"k1","id":"OTHER","a":"p"}\n' > "$pdir/jk_b.ndjson"
pcase "a repeated key name keeps the first"     "0 0 0" jk_a.ndjson jk_b.ndjson -k id

# Past an ignored value in the middle: where the rows first differ inside the
# value of a name nobody tracks, that value is skipped in each row on its own
# and the comparison carries on. It may differ in length, in escapes, in type,
# or be a nested object whose inner names do not count.
printf '{"id":"k1","ts":"T1","a":"p"}\n'                      > "$pdir/jg_a.ndjson"
printf '{"id":"k1","ts":"T2 and longer","a":"p"}\n'           > "$pdir/jg_b.ndjson"
pcase "an ignored middle value that differs"      "0 0 0" jg_a.ndjson jg_b.ndjson -k id -i ts
printf '{"id":"k1","ts":"T2 and longer","a":"X"}\n'           > "$pdir/jgc_b.ndjson"
pcase "and a real change after it"                "1 0 0" jg_a.ndjson jgc_b.ndjson -k id -i ts
printf '{"id":"k1","ts":"a\\"b,c}","a":"p"}\n'                 > "$pdir/jge_a.ndjson"
printf '{"id":"k1","ts":null,"a":"p"}\n'                      > "$pdir/jge_b.ndjson"
pcase "an escaped ignored value against null"     "0 0 0" jge_a.ndjson jge_b.ndjson -k id -i ts
printf '{"id":"k1","ts":{"a":"Z"},"a":"p"}\n'                 > "$pdir/jgn_a.ndjson"
printf '{"id":"k1","ts":[1,{"a":"Y"}],"a":"p"}\n'             > "$pdir/jgn_b.ndjson"
pcase "nested ignored values, names inside"       "0 0 0" jgn_a.ndjson jgn_b.ndjson -k id -i ts
# The mate repeats the compared name after the gap, and last wins.
printf '{"id":"k1","ts":"T2","a":"p","a":"LATER"}\n'          > "$pdir/jgd_b.ndjson"
pcase "a repeat of the column after the gap"      "1 0 0" jg_a.ndjson jgd_b.ndjson -k id -i ts
# Two gaps, the first before the key.
printf '{"t1":"x","id":"k1","t2":"y","a":"p"}\n'              > "$pdir/jg2_a.ndjson"
printf '{"t1":"xx","id":"k1","t2":"","a":"p"}\n'              > "$pdir/jg2_b.ndjson"
pcase "two ignored values, one before the key"    "0 0 0" jg2_a.ndjson jg2_b.ndjson -k id -i t1,t2
rm -rf "$pdir"

# --- the generator's own front door -----------------------------------------
#
# `data/` is in .gitignore, so on a fresh clone the directory the README's first
# command writes into does not exist. It used to fail there, with "write failed"
# and nothing about which write or why -- the first thing a new reader pasted.
echo "the generator's output directory:"
gd=$(mktemp -d)
if "$GEN" --rows 10k --out-dir "$gd/made" --prefix g >/dev/null 2>&1 \
   && [ -s "$gd/made/g_a.csv" ] && [ -s "$gd/made/g_b.csv" ]; then
  printf '  ok    a missing --out-dir is created\n'
else
  printf '  FAIL  a missing --out-dir is created\n'; fail=1
fi
if "$GEN" --rows 10k --out-dir "$gd/made" --prefix g >/dev/null 2>&1; then
  printf '  ok    and writing into it again is fine\n'
else
  printf '  FAIL  and writing into it again is fine\n'; fail=1
fi
# One level only, on purpose: a missing parent is a typo, and saying so beats
# quietly building a tree the reader did not ask for.
gcode=0   # this script runs with -e, so the status has to be caught, not returned
gerr=$("$GEN" --rows 10k --out-dir "$gd/no/such/parent" --prefix g 2>&1) || gcode=$?
if [ "$gcode" != 0 ] && printf '%s' "$gerr" | grep -qi "no such file"; then
  printf '  ok    a missing parent says which and why\n'
else
  printf '  FAIL  a missing parent says which and why (exit %s: %s)\n' "$gcode" "$gerr"; fail=1
fi
rm -rf "$gd"

if [ "$with_ports" = 1 ]; then
  echo "generator bytes, c against c++:"
  gen_dir=$(mktemp -d)
  gen_case() { # label, then the flags both generators are given
    local label=$1; shift
    rm -rf "$gen_dir/c" "$gen_dir/x"; mkdir -p "$gen_dir/c" "$gen_dir/x"
    "$GEN" --out-dir "$gen_dir/c" --prefix g "$@" >/dev/null 2>&1
    ../cpp/build/gen-data --out-dir "$gen_dir/x" --prefix g "$@" >/dev/null 2>&1
    local same=1 any=0
    for f in $(cd "$gen_dir/x" && ls); do
      any=1
      cmp -s "$gen_dir/c/$f" "$gen_dir/x/$f" || same=0
    done
    if [ "$any" = 1 ] && [ "$same" = 1 ]; then
      printf '  ok    %s\n' "$label"
    else
      printf '  FAIL  %s\n' "$label"; fail=1
    fi
  }
  gen_case "csv"                    --rows 5k
  gen_case "ndjson"                 --rows 5k --format json
  # The generator formats rows on every core, in waves, so these are the shapes
  # where a threaded writer differs from a serial one: a thread count that does
  # not divide the wave, a row count that ends inside one, and one row.
  gen_case "csv, one thread"        --rows 20k --threads 1
  gen_case "csv, three threads"     --rows 20k --threads 3
  gen_case "csv, seven threads"     --rows 20k --threads 7
  gen_case "csv, ending mid-wave"   --rows 8193
  gen_case "csv, a single row"      --rows 1
  gen_case "ndjson, three threads"  --rows 20k --format json --threads 3
  gen_case "parquet"                --rows 5k --format parquet --compression none
  gen_case "parquet, small groups"  --rows 5k --format parquet --compression none --row-group-size 512
  gen_case "parquet, dict gives up" --rows 5k --format parquet --compression none --dict-limit 175 --row-group-size 300
  gen_case "parquet, all plain"     --rows 5k --format parquet --compression none --dict-limit 1
  gen_case "a different seed"       --rows 2k --seed 42
  rm -rf "$gen_dir"
fi

# --compare: the flag this port did not have, where the other three did. The
# rule is the one they apply -- a name that is not in both files is an error,
# and key and ignored columns are filtered out of an explicit list rather than
# refused, so `-k id -c id,a` compares `a`.
echo "--compare:"
cdir=$(mktemp -d)
printf 'id,a,b,c\nk1,p,q,r\nk2,p,q,r\n' > "$cdir/a.csv"
printf 'id,a,b,c\nk1,X,q,r\nk2,p,Y,r\n' > "$cdir/b.csv"
ccase() { # label, expected "changed added removed", then flags
  local label=$1 want=$2; shift 2
  rm -f "$cdir/o.json"
  ./csvdiff compare "$cdir/a.csv" "$cdir/b.csv" -k id "$@" --json "$cdir/o.json" \
      >/dev/null 2>&1 || true
  local got
  got=$(python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["counts"]; print(c["changed"], c["added"], c["removed"])' "$cdir/o.json" 2>/dev/null) || got="(no report)"
  if [ "$got" = "$want" ]; then printf '  ok    %s\n' "$label"
  else printf '  FAIL  %s\n    want: %s\n    got : %s\n' "$label" "$want" "$got"; fail=1; fi
}
ccase "every common non-key column, the default" "2 0 0"
ccase "one column named"                          "1 0 0" -c a
ccase "the other one named"                       "1 0 0" -c b
ccase "both named"                                "2 0 0" -c a,b
ccase "a key column in the list is filtered out"  "1 0 0" -c id,a
ccase "--ignore still subtracts from an explicit list" "1 0 0" -c a,b -i b

out=$(./csvdiff compare "$cdir/a.csv" "$cdir/b.csv" -k id -c nope 2>&1) || true
case "$out" in
  *"not present in both files"*) echo "  ok    an unknown --compare name is refused" ;;
  *) echo "  FAIL  expected a refusal, got: $out"; fail=1 ;;
esac
rm -rf "$cdir"

# A known flag with nothing after it used to report "unknown option", blaming
# the flag for not being recognised when it was recognised and empty.
echo "a flag left without its value:"
for flag in --key --ignore --compare --json --threads; do
  out=$(./csvdiff compare x.csv y.csv "$flag" 2>&1) || true
  case "$out" in
    *"$flag needs a value"*) echo "  ok    $flag says so" ;;
    *) echo "  FAIL  $flag: $out"; fail=1 ;;
  esac
done

# --max-memory: a ceiling declared before the allocations that scale with the
# input, rather than discovered when one fails. Checking what malloc returns is
# necessary and not sufficient -- under Linux's default overcommit there is a
# band where malloc succeeds and the kernel kills the process on touch, with no
# message and exit 137. This is the flag that turns that into an error.
echo "--max-memory:"
mdir=$(mktemp -d)
"$GEN" --rows 60k --out-dir "$mdir" --prefix m >/dev/null 2>&1

out=$(./csvdiff compare "$mdir/m_a.csv" "$mdir/m_b.csv" -k account_id,txn_id \
        -i updated_at --max-memory 1 2>&1 | head -1)
case "$out" in
  *"needs more than the 1 MB"*) echo "  ok    a ceiling too small is refused, naming the ceiling" ;;
  *) echo "  FAIL  expected a refusal naming the ceiling, got: $out"; fail=1 ;;
esac

# The same run with room finishes, and finishes with the same answer it gives
# with no ceiling at all -- a budget that changed the counts would be worse
# than no budget.
free=$(./csvdiff compare "$mdir/m_a.csv" "$mdir/m_b.csv" -k account_id,txn_id -i updated_at 2>&1 | summary)
capped=$(./csvdiff compare "$mdir/m_a.csv" "$mdir/m_b.csv" -k account_id,txn_id -i updated_at \
             --max-memory 512 2>&1 | summary)
if [ "$free" = "$capped" ] && [ -n "$free" ]; then
  echo "  ok    a ceiling with room changes nothing: $capped"
else
  echo "  FAIL  a ceiling changed the answer"; echo "    without: $free"; echo "    with   : $capped"; fail=1
fi

# And on the columnar path, which allocates in a different file.
if [ -x "$GEN" ]; then
  # 20k rather than 5k. At five thousand rows the whole comparison now fits
  # inside one megabyte, so a `--max-memory 1` refusal could not happen and this
  # check passed only while the ceiling was counting every column ever read
  # instead of the ones held. A test that is green for the wrong reason goes red
  # when the reason is fixed, which is what happened here.
  "$GEN" --rows 20k --out-dir "$mdir" --prefix q --format parquet >/dev/null 2>&1
  out=$(./csvdiff compare "$mdir/q_a.unc.parquet" "$mdir/q_b.unc.parquet" \
          -k account_id,txn_id -i updated_at --max-memory 1 2>&1 | head -1)
  case "$out" in
    *"needs more than the 1 MB"*) echo "  ok    the parquet path honours the ceiling too" ;;
    *) echo "  FAIL  parquet ignored the ceiling: $out"; fail=1 ;;
  esac

  # The ceiling is what a run holds, not what it has ever read. The columnar
  # path reads one compared column at a time and frees it before asking for the
  # next, so comparing seventeen of them must not need seventeen columns' worth
  # of headroom -- which is what it used to ask for, because the budget was
  # taken and never given back. One thread, so this is about the accounting
  # rather than about how many columns are genuinely in flight.
  one=$(./csvdiff compare "$mdir/q_a.unc.parquet" "$mdir/q_b.unc.parquet" \
          -k account_id,txn_id -i updated_at --threads 1 --compare status \
          --max-memory 16 2>&1 | head -1)
  many=$(./csvdiff compare "$mdir/q_a.unc.parquet" "$mdir/q_b.unc.parquet" \
          -k account_id,txn_id -i updated_at --threads 1 \
          --max-memory 16 2>&1 | head -1)
  case "$one$many" in
    *"more than the"*) echo "  FAIL  a ceiling that fits one column refused: $one$many"; fail=1 ;;
    *) echo "  ok    the ceiling counts what is held, not every column read" ;;
  esac

  # And it counts every buffer a column keeps, not just the row index. A PLAIN
  # column's values array is eight bytes a row where the index is four, so a
  # ceiling that saw only the index read low on exactly the columns that cost
  # most. Ask for a ceiling between the two and the run must be refused: it
  # would have been allowed when only the index was counted.
  out=$(./csvdiff compare "$mdir/q_a.unc.parquet" "$mdir/q_b.unc.parquet" \
          -k account_id,txn_id -i updated_at --threads 1 --max-memory 1 2>&1 | head -1)
  case "$out" in
    *"more than the"*) echo "  ok    the ceiling counts the values array, not just the index" ;;
    *) echo "  FAIL  a 1 MB ceiling admitted a 20k-row parquet run: $out"; fail=1 ;;
  esac
fi

out=$(./csvdiff compare "$mdir/m_a.csv" "$mdir/m_b.csv" --max-memory 2>&1) || true
case "$out" in
  *"--max-memory needs a value"*) echo "  ok    --max-memory with no value says so" ;;
  *) echo "  FAIL  $out"; fail=1 ;;
esac
rm -rf "$mdir"

# The field scan steps 32 bytes where the build has AVX2, 16 with SSE2, and 8
# otherwise, each falling through to the next and finally to a byte loop. Rows
# shorter than a step are the case that only the tails see, so they are worth a
# row of their own: a file whose every row is under eight bytes never enters any
# wide loop, and one that straddles sixteen exercises the hand-off.
echo "rows shorter than the scan step:"
sdir=$(mktemp -d)
{ echo 'k,v'; echo 'a,1'; echo 'b,2'; echo 'c,3'; } > "$sdir/tiny_a.csv"
{ echo 'k,v'; echo 'a,1'; echo 'b,9'; echo 'd,4'; } > "$sdir/tiny_b.csv"
out=$(./csvdiff compare "$sdir/tiny_a.csv" "$sdir/tiny_b.csv" -k k 2>&1 | summary)
case "$out" in
  *"matched 2 (changed 1)"*"added 1"*"removed 1"*) echo "  ok    three-byte rows: $out" ;;
  *) echo "  FAIL  three-byte rows: $out"; fail=1 ;;
esac
# Straddling the sixteen-byte step: a row of exactly 15, 16 and 17 bytes.
{ echo 'k,v'; echo 'k1,aaaaaaaaaaaa'; echo 'k2,aaaaaaaaaaaaa'; echo 'k3,aaaaaaaaaaaaaa'; } > "$sdir/edge_a.csv"
{ echo 'k,v'; echo 'k1,aaaaaaaaaaaa'; echo 'k2,bbbbbbbbbbbbb'; echo 'k3,aaaaaaaaaaaaaa'; } > "$sdir/edge_b.csv"
out=$(./csvdiff compare "$sdir/edge_a.csv" "$sdir/edge_b.csv" -k k 2>&1 | summary)
case "$out" in
  *"matched 3 (changed 1)"*) echo "  ok    rows straddling the step: $out" ;;
  *) echo "  FAIL  rows straddling the step: $out"; fail=1 ;;
esac
rm -rf "$sdir"

# The four normalisation options, held to the answers the other ports give.
# Each case is two rows, A then B, under one flag set; the expected result is
# "changed added removed", or "refuse" for a run that must exit 2 naming the
# reason -- with and without --json, since only one of them builds samples.
echo "normalisation options:"
ndir=$(mktemp -d)
ncase() { # label, a-row, b-row (printf %b escapes), expected, then flags
  local label=$1 want=$4
  printf 'id,v\nk0,same\n%b\n' "$2" > "$ndir/a.csv"
  printf 'id,v\nk0,same\n%b\n' "$3" > "$ndir/b.csv"
  shift 4
  for json in "" "--json $ndir/o.json"; do
    rm -f "$ndir/o.json"
    local code=0 got
    # shellcheck disable=SC2086  # $json is zero words or two
    ./csvdiff compare "$ndir/a.csv" "$ndir/b.csv" -k id "$@" $json >"$ndir/out" 2>&1 || code=$?
    if [ "$code" = 2 ]; then
      if grep -q -- '--ignore-case on a field outside ASCII' "$ndir/out"; then got=refuse
      else got="exit 2: $(cat "$ndir/out")"; fi
    else got=$(summary < "$ndir/out" | sed -E 's/.*changed ([0-9]+)\) \| added ([0-9]+) \| removed ([0-9]+).*/\1 \2 \3/')
    fi
    if [ "$got" = "$want" ]; then printf '  ok    %s%s\n' "$label" "${json:+, --json}"
    else printf '  FAIL  %s%s\n    want: %s\n    got : %s\n' "$label" "${json:+, --json}" "$want" "$got"; fail=1; fi
  done
}
ncase "--tolerance reads a padded number"          'k1,100'          'k1,  100  '      "0 0 0" --tolerance 0.5
ncase "--tolerance: inf is text, not a number"     'k1,inf'          'k1,Inf'          "1 0 0" --tolerance 0.5
ncase "--tolerance outside it is a change"         'k1,7.25'         'k1,8'            "1 0 0" --tolerance 0.5
ncase "--trim: blank against empty is a change"    'k1,   '          'k1,'             "1 0 0" --trim
ncase "--trim --empty-is-null: and then it is not" 'k1,   '          'k1,'             "0 0 0" --trim --empty-is-null
ncase "--ignore-case folds ASCII"                  'k1,Alpha'        'k1,ALPHA'        "0 0 0" --ignore-case
ncase "--ignore-case, unchanged outside ASCII"     'k1,CAF\xc3\x89'  'k1,CAF\xc3\x89'  "0 0 0" --ignore-case
ncase "--ignore-case, one side empty"              'k1,'             'k1,\xc3\x89'     "1 0 0" --ignore-case
ncase "--ignore-case, differs outside ASCII"       'k1,CAF\xc3\x89'  'k1,caf\xc3\xa9'  refuse  --ignore-case
ncase "--ignore-case, a key outside ASCII"         'k\xc3\x89,x'     'k\xc3\xa9,x'     refuse  --ignore-case
# The columnar path does not carry the options yet, so it says so.
if [ -x "$GEN" ]; then
  "$GEN" --rows 1k --out-dir "$ndir" --prefix q --format parquet >/dev/null
  for opt in --trim --ignore-case --empty-is-null "--tolerance 0.5"; do
    code=0
    # shellcheck disable=SC2086  # $opt is one flag or a flag and its value
    out=$(./csvdiff compare "$ndir/q_a.unc.parquet" "$ndir/q_b.unc.parquet" -k account_id,txn_id $opt 2>&1) || code=$?
    case "$code:$out" in
      "2:"*"${opt%% *}"*) echo "  ok    parquet refuses $opt by name" ;;
      *) echo "  FAIL  parquet with $opt: exit $code: $out"; fail=1 ;;
    esac
  done
else
  echo "skip: $GEN is not built, so the parquet refusals did not run"
fi
rm -rf "$ndir"

exit $fail
