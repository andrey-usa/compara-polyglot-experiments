#!/usr/bin/env bash
# Holds this port to the answers the Rust port gives -- on every input format,
# at every thread count -- and checks that the memory budget is a bound rather
# than a suggestion. Run from zig/.
set -uo pipefail
cd "$(dirname "$0")" || exit 2
ZIG=${ZIG:-zig}
"$ZIG" build --release=fast || exit 2
RUST=../rust/target/release/compara
[ -x "$RUST" ] || { echo "build the Rust port first: (cd ../rust && cargo build --release)"; exit 2; }
BIN=zig-out/bin/csvdiff
FIX=../tests/fixtures
fail=0

# The two ports print the same facts in different shapes; this is the part both
# agree on, with the thousands separators, the engine label and the timing
# stripped -- the label is `parquet` on a Parquet pair and `turbo` otherwise,
# and only one of the two ports prints a time after it.
# `sed -E`, not the BRE spelling: BSD sed -- which is the sed on a Mac --
# does not read `\|` as alternation, so the substitution silently matched
# nothing there and every comparison failed on the engine label and the
# timing after it. ERE is the one dialect both seds agree on.
answer() { sed -E 's/ \| (turbo|parquet).*//; s/,//g' ; }

check() {
  local label=$1 a=$2 b=$3; shift 3
  local r z
  r=$("$RUST" compare "$a" "$b" "$@" --engine turbo -o /dev/null 2>&1 | head -1 | answer)
  z=$($BIN compare "$a" "$b" "$@" 2>&1 | head -1 | answer)
  if [ "$r" = "$z" ]; then printf '  ok    %s\n' "$label"
  else printf '  FAIL  %s\n    rust: %s\n    zig : %s\n' "$label" "$r" "$z"; fail=1; fi
}

echo "awkward fixture, zig against rust:"
check "defaults" "$FIX/awkward_a.csv" "$FIX/awkward_b.csv" -k k
check "--trim" "$FIX/awkward_a.csv" "$FIX/awkward_b.csv" -k k --trim
# --ignore-case is excluded on purpose: this port refuses non-ASCII folding,
# which the fixture contains. See README.md.

echo "every input format reads the same way:"
for a in a.csv a.ndjson a_dict_snappy.parquet a_plain_none.parquet a_dict_gzip.parquet \
         a_plain_zstd.parquet a_plain_lz4.parquet a_delta_v2.parquet a_dict_row_groups.parquet; do
  check "$a against b.csv" "$FIX/formats/$a" "$FIX/formats/b.csv" -k id
done
check "parquet against parquet" "$FIX/formats/a_dict_snappy.parquet" \
      "$FIX/formats/b_dict_snappy.parquet" -k id
check "json against parquet" "$FIX/formats/a.ndjson" "$FIX/formats/b_dict_snappy.parquet" -k id

echo "a typed parquet column renders as the csv of the same data:"
out=$($BIN compare "$FIX/formats/typed.parquet" "$FIX/formats/typed.csv" -k id 2>&1 | head -1)
case "$out" in
  *"matched 8 (changed 0)"*) echo "  ok    every column matches its stated text" ;;
  *) echo "  FAIL  expected no differences, got: $out"; fail=1 ;;
esac

echo "the thread count does not change the answer:"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
# Over the 4 MB threshold where the file is split into chunks, with quoted
# newlines and doubled quotes at the boundaries -- the case chunking can break.
python3 - "$tmp" <<'PY'
import sys
d = sys.argv[1]
with open(f"{d}/a.csv", "w") as a, open(f"{d}/b.csv", "w") as b:
    a.write("k,v,w\n"); b.write("k,v,w\n")
    for i in range(40000):
        quoted = f'"line {i}\nsecond, with ""quotes"" in it"'
        a.write(f"K{i},{quoted},{i}\n")
        b.write(f"K{i},{quoted},{i + 1 if i % 7 == 0 else i}\n")
PY
reference=""
for threads in 1 2 3 4 8; do
  got=$($BIN compare "$tmp/a.csv" "$tmp/b.csv" -k k --threads $threads 2>&1 | head -1 | answer)
  if [ -z "$reference" ]; then reference=$got
  elif [ "$got" != "$reference" ]; then
    printf '  FAIL  %s threads gave a different answer\n    %s\n' "$threads" "$got"; fail=1
  fi
done
[ $fail -eq 0 ] && echo "  ok    one answer at 1, 2, 3, 4 and 8 threads"
check "and it is the answer rust gives" "$tmp/a.csv" "$tmp/b.csv" -k k

# The CSV byte proof with gaps: an ignored column among compared ones is
# skipped by field count in each row on its own, so a value there that differs
# -- in length, in quoting, with a delimiter inside the quotes -- does not stop
# a row proving equal, and does not shift what the proof compares after it.
# The last column is compared, so the proof also closes on the line ending.
echo "the csv byte proof, past an ignored column:"
{ echo 'id,skip,a,b'; echo 'k1,S1,p,q'; echo 'k2,S1,p,q'; echo 'k3,S1,p,q'; } > "$tmp/gap_a.csv"
{ echo 'id,skip,a,b'; echo 'k1,"S, ""two"" and more",p,q'; echo 'k2,,p,X'; echo 'k3,S2,p,qq'; } > "$tmp/gap_b.csv"
check "a middle ignored column that differs" "$tmp/gap_a.csv" "$tmp/gap_b.csv" -k id -i skip
{ echo 'id,skip,a,b'; echo 'k1,S1,p,q'; echo 'k2,S1,p,q'; } > "$tmp/gs_a.csv"
{ echo 'id,skip,a,b'; echo 'k1'; echo 'k2,S2'; } > "$tmp/gs_b.csv"
check "a mate that stops inside the gap" "$tmp/gs_a.csv" "$tmp/gs_b.csv" -k id -i skip
{ echo 'skip,id,a,t,b'; echo 'S1,k1,p,T1,q'; echo 'S1,k2,p,T1,q'; } > "$tmp/g2_a.csv"
{ echo 'skip,id,a,t,b'; echo 'S22,k1,p,T2,q'; echo 'S,k2,p,T222,Y'; } > "$tmp/g2_b.csv"
check "a leading gap and a second one" "$tmp/g2_a.csv" "$tmp/g2_b.csv" -k id -i skip,t
printf 'id,skip,a\nk1,S1,p\n' > "$tmp/gc_a.csv"
printf 'id,skip,a\r\nk1,S22,p\r\n' > "$tmp/gc_b.csv"
check "a gap, and CRLF against LF" "$tmp/gc_a.csv" "$tmp/gc_b.csv" -k id -i skip
{ echo 'a,k,c'; echo 'x,K1,c1'; echo 'y,K2,cc'; } > "$tmp/pre_a.csv"
{ echo 'a,k,c'; echo 'x,K1,c1'; echo 'y,K2,cccccccc'; } > "$tmp/pre_b.csv"
check "a mate whose last column carries on" "$tmp/pre_a.csv" "$tmp/pre_b.csv" -k k

# The ndjson proof past an ignored value in the middle: where the rows first
# differ inside the value of `ts`, which nobody tracks, that value is skipped in
# each row on its own and the comparison carries on -- whatever its length,
# escapes, type or nested names. A repeat of the compared name after it wins.
echo "the ndjson byte proof, past an ignored value:"
printf '{"id":"k1","ts":"T1","a":"p"}\n'                  > "$tmp/jg_a.ndjson"
printf '{"id":"k1","ts":"T2 and longer","a":"p"}\n'       > "$tmp/jg_b.ndjson"
check "an ignored middle value that differs" "$tmp/jg_a.ndjson" "$tmp/jg_b.ndjson" -k id -i ts
printf '{"id":"k1","ts":"T2 and longer","a":"X"}\n'       > "$tmp/jgc_b.ndjson"
check "and a real change after it" "$tmp/jg_a.ndjson" "$tmp/jgc_b.ndjson" -k id -i ts
printf '{"id":"k1","ts":"a\\"b,c}","a":"p"}\n'            > "$tmp/jge_a.ndjson"
printf '{"id":"k1","ts":null,"a":"p"}\n'                  > "$tmp/jge_b.ndjson"
check "an escaped ignored value against null" "$tmp/jge_a.ndjson" "$tmp/jge_b.ndjson" -k id -i ts
printf '{"id":"k1","ts":{"a":"Z"},"a":"p"}\n'             > "$tmp/jgn_a.ndjson"
printf '{"id":"k1","ts":[1,{"a":"Y"}],"a":"p"}\n'         > "$tmp/jgn_b.ndjson"
check "nested ignored values, tracked names inside" "$tmp/jgn_a.ndjson" "$tmp/jgn_b.ndjson" -k id -i ts
printf '{"id":"k1","ts":"T2","a":"p","a":"LATER"}\n'      > "$tmp/jgd_b.ndjson"
check "a repeat of the compared name after the gap" "$tmp/jg_a.ndjson" "$tmp/jgd_b.ndjson" -k id -i ts

# --ignore-case refuses a value outside ASCII only where folding it decides the
# answer: a key, or a compared value that differs byte for byte. A value that is
# unchanged, or in a row only one side has, or whose mate is empty, is answered.
echo "--ignore-case outside ASCII:"
fold() { # name, a-rows, b-rows (printf %b escapes), expected exit status
  printf 'k,v\n1,alpha\n%b\n' "$2" > "$tmp/fa.csv"
  printf 'k,v\n1,ALPHA\n%b\n' "$3" > "$tmp/fb.csv"
  for json in "" "--json $tmp/fold.json"; do
    # shellcheck disable=SC2086  # $json is zero words or two
    $BIN compare "$tmp/fa.csv" "$tmp/fb.csv" -k k --ignore-case $json >/dev/null 2>&1
    code=$?
    if [ "$code" = "$4" ]; then echo "  ok    $1${json:+, --json}"
    else echo "  FAIL  $1${json:+, --json}: exit $code, wanted $4"; fail=1; fi
  done
}
fold "unchanged, outside ASCII" '2,CAF\xc3\x89' '2,CAF\xc3\x89' 0
fold "only in an added row" '2,x' '2,x\n3,na\xc3\xafve' 1
fold "absent on one side" '2,' '2,\xc3\x89' 1
fold "differs, outside ASCII: refused" '2,CAF\xc3\x89' '2,caf\xc3\xa9' 2
fold "a key outside ASCII: refused" '\xc3\x89,x' '\xc3\x89,x' 2

echo "the memory budget is a bound, not a target:"
# Enough rows that an index cannot fit in a very small budget.
{ echo "k,v"; for i in $(seq 1 20000); do echo "$i,value-$i"; done; } > "$tmp/small_a.csv"
cp "$tmp/small_a.csv" "$tmp/small_b.csv"

out=$($BIN compare "$tmp/small_a.csv" "$tmp/small_b.csv" -k k --max-memory 1 2>&1 | head -1)
case "$out" in
  *"more than the 1 MB"*) echo "  ok    a budget too small is refused, naming the budget" ;;
  *) echo "  FAIL  expected a refusal, got: $out"; fail=1 ;;
esac

# A misspelled --ignore used to widen the comparison in silence: the column it
# meant to drop got compared and came back changed on every row. `--key` and
# `--compare` have always refused an unknown name; this is the third.
out=$($BIN compare "$tmp/small_a.csv" "$tmp/small_b.csv" -k k -i no_such_column 2>&1 | head -1)
case "$out" in
  *"present in neither file"*) echo "  ok    an unknown --ignore name is refused" ;;
  *) echo "  FAIL  expected a refusal, got: $out"; fail=1 ;;
esac

# The other half: a name that is really there still works, so the check refuses
# typos rather than refusing --ignore.
out=$($BIN compare "$tmp/small_a.csv" "$tmp/small_b.csv" -k k -i v 2>&1 | head -1)
case "$out" in
  *"matched 20000"*) echo "  ok    a real --ignore name is still accepted" ;;
  *) echo "  FAIL  a real --ignore name should be accepted, got: $out"; fail=1 ;;
esac

out=$($BIN compare "$tmp/small_a.csv" "$tmp/small_b.csv" -k k --max-memory 64 2>&1 | head -1)
case "$out" in
  *"matched 20000"*) echo "  ok    a budget that is enough finishes inside it" ;;
  *) echo "  FAIL  expected a comparison, got: $out"; fail=1 ;;
esac

echo "parquet, read natively and compared columnwise:"
# The claim the columnar path has to earn is that it is the *same* comparison:
# the same rows, in a format that stores them as columns of dictionary indices
# rather than as lines of text, must produce the same counts and the same
# per-column statistics. The files come from cpp/build/gen-data, which writes
# CSV and Parquet from one field-by-field recipe.
GEN=../cpp/build/gen-data
if [ ! -x "$GEN" ]; then
  echo "  skip  ../cpp/build/gen-data is not built: (cd ../cpp && make gen-data)"
else
  pq=$(mktemp -d)
  same() { # label, generator flags, extension, then compare flags
    local label=$1 flags=$2 ext=$3; shift 3
    "$GEN" --rows 20k --out-dir "$pq" --prefix t >/dev/null
    # shellcheck disable=SC2086
    "$GEN" --rows 20k --out-dir "$pq" --prefix t $flags >/dev/null
    $BIN compare "$pq/t_a.csv" "$pq/t_b.csv" "$@" --json "$pq/csv.json" >/dev/null 2>&1
    $BIN compare "$pq/t_a$ext" "$pq/t_b$ext" "$@" --json "$pq/pq.json" >/dev/null 2>&1
    if python3 - "$pq/csv.json" "$pq/pq.json" <<'PYEOF'
import json, sys
def load(path):
    d = json.load(open(path))
    return (d["counts"], d["columns"])
sys.exit(0 if load(sys.argv[1]) == load(sys.argv[2]) else 1)
PYEOF
    then
      printf '  ok    %s\n' "$label"
    else
      printf '  FAIL  %s: the parquet answer differs from the csv one\n' "$label"; fail=1
    fi
    rm -f "$pq"/t_*
  }
  # A comma inside one word is the key list ("a,b"), not two array elements.
  # shellcheck disable=SC2054
  K=(-k account_id,txn_id -i updated_at)
  same "snappy"                  "--format parquet --compression snappy" .parquet "${K[@]}"
  same "uncompressed"            "--format parquet --compression none"   .unc.parquet "${K[@]}"
  same "many small row groups"   "--format parquet --compression snappy --row-group-size 512" .parquet "${K[@]}"
  # A dictionary budget the data crosses partway makes a column that is
  # dictionary encoded in some row groups and plain in others -- the shape a
  # real writer produces at scale, and the one the reader has to fold together.
  same "the dictionary gives up" "--format parquet --compression none --dict-limit 175 --row-group-size 300" .unc.parquet "${K[@]}"
  same "every column plain"      "--format parquet --compression snappy --dict-limit 1" .parquet "${K[@]}"
  same "--trim"                  "--format parquet --compression snappy" .parquet "${K[@]}" --trim
  same "--empty-is-null"         "--format parquet --compression snappy" .parquet "${K[@]}" --empty-is-null
  # A tolerance makes equality non-transitive, so it cannot be given an id: the
  # column falls back to comparing bytes, and must still agree.
  same "--tolerance"             "--format parquet --compression snappy" .parquet "${K[@]}" --tolerance 0.01
  # A key column both sides store as a dictionary takes the shared-id path,
  # where the join never looks at a byte after the dictionaries are mapped.
  same "a dictionary key column" "--format parquet --compression snappy" .parquet -k currency,status

  # Files of different sizes, both ways round, so many keys on one side have
  # no mate. `added` is derived rather than counted on the columnar path -- B's
  # distinct keys minus the pairs the A pass found -- and this is where a wrong
  # derivation would show: the CSV engine still counts it directly.
  "$GEN" --rows 20k --out-dir "$pq" --prefix big >/dev/null
  "$GEN" --rows 20k --out-dir "$pq" --prefix big --format parquet --compression snappy >/dev/null
  "$GEN" --rows 7k --out-dir "$pq" --prefix small >/dev/null
  "$GEN" --rows 7k --out-dir "$pq" --prefix small --format parquet --compression snappy >/dev/null
  for pair in "big_a small_b" "small_a big_b"; do
    set -- $pair
    c=$($BIN compare "$pq/$1.csv" "$pq/$2.csv" "${K[@]}" 2>&1 | head -1 | answer)
    p=$($BIN compare "$pq/$1.parquet" "$pq/$2.parquet" "${K[@]}" 2>&1 | head -1 | answer)
    case "$c" in *"added 0 | removed 0"*) printf '  FAIL  %s: the pair has nothing unmatched to test\n' "$pair"; fail=1; continue ;; esac
    if [ "$c" = "$p" ]; then
      printf '  ok    uneven sizes, %s: %s\n' "$pair" "$p"
    else
      printf '  FAIL  uneven sizes, %s: csv says %s, parquet says %s\n' "$pair" "$c" "$p"; fail=1
    fi
  done
  rm -f "$pq"/big_* "$pq"/small_*

  "$GEN" --rows 1k --out-dir "$pq" --prefix m >/dev/null
  "$GEN" --rows 1k --out-dir "$pq" --prefix m --format parquet --compression snappy >/dev/null

  # A mixed pair has no column to compare a byte stream against, so the
  # columnar path cannot take it -- but the text engine can, by materialising
  # the Parquet side into rows (see pqread.zig). Slower, and the same answer:
  # that is what this checks, against csv-vs-csv on the same rows.
  $BIN compare "$pq/m_a.csv" "$pq/m_b.csv" -k account_id,txn_id \
       --json "$pq/text.json" >/dev/null 2>&1
  $BIN compare "$pq/m_a.parquet" "$pq/m_b.csv" -k account_id,txn_id \
       --json "$pq/mixed.json" >/dev/null 2>&1
  if python3 - "$pq/text.json" "$pq/mixed.json" <<'PYEOF'
import json, sys
def load(path):
    d = json.load(open(path))
    return (d["counts"], d["columns"])
sys.exit(0 if load(sys.argv[1]) == load(sys.argv[2]) else 1)
PYEOF
  then echo "  ok    a mixed parquet/text pair is read, not refused"
  else echo "  FAIL  the mixed pair disagrees with csv against csv"; fail=1; fi

  # The budget is what this port is for, and it has to hold on this path too.
  out=$($BIN compare "$pq/m_a.parquet" "$pq/m_b.parquet" -k account_id,txn_id --max-memory 1 2>&1 | head -1)
  case "$out" in
    *"more than the 1 MB"*) echo "  ok    --max-memory bounds the parquet path too" ;;
    *) echo "  FAIL  the budget was not enforced: $out"; fail=1 ;;
  esac
  rm -rf "$pq"
fi

# Parquet decodes into an arena taken from the same allocator, so the budget has
# to bound that path too rather than only the mapped one. It needs a file large
# enough that the arena is the thing that does not fit.
../rust/target/release/compara-gen-data --rows 20k --out-dir "$tmp" --prefix pq --format parquet >/dev/null
out=$($BIN compare "$tmp/pq_a.parquet" "$tmp/pq_b.parquet" -k account_id,txn_id -i updated_at \
      --max-memory 4 2>&1 | head -1)
case "$out" in
  *"more than the 4 MB"*) echo "  ok    reading parquet stays inside the budget too" ;;
  *) echo "  FAIL  expected a refusal, got: $out"; fail=1 ;;
esac
out=$($BIN compare "$tmp/pq_a.parquet" "$tmp/pq_b.parquet" -k account_id,txn_id -i updated_at \
      --max-memory 128 2>&1 | head -1)
case "$out" in
  *"matched 19980"*) echo "  ok    and finishes when the budget is enough" ;;
  *) echo "  FAIL  expected a comparison, got: $out"; fail=1 ;;
esac

echo "unit tests:"
if "$ZIG" build test; then echo "  ok    scan, field, slab, text, thrift, codec, encoding, parquet"
else echo "  FAIL  zig build test"; fail=1; fi

# The columnar reader takes uncompressed and snappy; the reader beside it takes
# zstd, gzip and lz4. A capability refusal falls through to that one, so a file
# this binary carries a decoder for is read rather than turned away. Before the
# fall-through a zstd pair was refused -- and, on a large enough file, the
# refusal path freed an uninitialized column array and took the process down
# with SIGSEGV.
echo "parquet codecs, and which reader takes them:"
fx=../tests/fixtures/formats
for spec in "a_plain_none parquet" "a_dict_snappy parquet" \
            "a_plain_zstd turbo" "a_dict_gzip turbo" "a_plain_lz4 turbo"; do
  set -- $spec
  out=$($BIN compare "$fx/$1.parquet" "$fx/$1.parquet" -k id 2>&1 | head -1)
  case "$out" in
    *"matched 301"*"| $2")
      echo "  ok    $1 is read, by the $2 reader" ;;
    *"matched 301"*)
      echo "  FAIL  $1 was read but by the wrong reader: $out"; fail=1 ;;
    *)
      echo "  FAIL  $1 was not read: $out"; fail=1 ;;
  esac
done

# A file that is actually broken must still fail, rather than being handed to
# the second reader and failing there with a message about the wrong thing.
trunc=$(mktemp -d)
head -c 400 "$fx/a_plain_zstd.parquet" > "$trunc/cut.parquet"
out=$($BIN compare "$trunc/cut.parquet" "$trunc/cut.parquet" -k id 2>&1 | head -1)
case "$out" in
  *"matched"*) echo "  FAIL  a truncated parquet file was read: $out"; fail=1 ;;
  *) echo "  ok    a truncated parquet file is still refused" ;;
esac
rm -rf "$trunc"

# A key a JSON writer escaped has to find the same key written literally: the
# hash sees the decoded bytes on both sides, or the row comes out added and
# removed. This port once folded an escaped key's bytes one at a time and a plain
# one a word at a time, and missed it. Lengths around the eight-byte fold, and a
# CSV doubled quote against the JSON backslash.
kesc=$(mktemp -d)
python3 - "$kesc" <<'PY'
import json, sys
out = sys.argv[1]
BS = chr(92)
pairs = [("a" + BS + "/b", "a/b"), (BS + "u0041BC", "ABC"), ("an eight" + BS + "/", "an eight/"),
         ("a key longer than a word " + BS + "/ x", "a key longer than a word / x"),
         ("quote" + BS + '"in', 'quote"in'), (BS + "u4e2d" + BS + "u6587 key", "中文 key")]
with open(f"{out}/a.ndjson", "w") as fa, open(f"{out}/b.ndjson", "w") as fb:
    for i, (escaped, literal) in enumerate(pairs):
        fa.write('{"k":"%s","v":"%d"}\n' % (escaped, i))
        fb.write(json.dumps({"k": literal, "v": str(i)}, ensure_ascii=False) + "\n")
open(f"{out}/c.csv", "w").write('k,v\n"quote""in",4\n"a/b",0\n')
open(f"{out}/d.ndjson", "w").write('{"k":"quote%s\"in","v":"4"}\n{"k":"a/b","v":"0"}\n' % BS)
PY
got=$($BIN compare "$kesc/a.ndjson" "$kesc/b.ndjson" -k k 2>/dev/null | grep -o 'matched [0-9]*' || true)
if [ "$got" = "matched 6" ]; then echo "  ok    an escaped key finds the same key written literally"
else echo "  FAIL  escaped and literal spellings of one key did not match: $got"; fail=1; fi
got=$($BIN compare "$kesc/c.csv" "$kesc/d.ndjson" -k k 2>/dev/null | grep -o 'matched [0-9]*' || true)
if [ "$got" = "matched 2" ]; then echo "  ok    and a CSV doubled quote finds the JSON backslash"
else echo "  FAIL  a CSV key with a doubled quote missed its JSON spelling: $got"; fail=1; fi
rm -rf "$kesc"

# Past page cache the join runs inside A's sweep, joining every row as though it
# were its key's first and taking back, afterwards, the rows that repeat a key.
# CSVDIFF_FUSED_JOIN=1 takes that path at any size. The repeats here differ from
# their key's first row -- in a compared value, or in being absent from B -- and
# the file is past the split threshold, so a repeat that was not taken back, or a
# chunk's last row cut short, changes the counts or the column counts.
echo "the join inside the sweep:"
fz=$(mktemp -d)
python3 - "$fz" <<'PY'
import json, sys
d = sys.argv[1]
def rows(side):
    out = []
    for i in range(250000):
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
    $BIN compare "$fz/f_a.$ext" "$fz/f_b.$ext" -k k --threads $t --json "$fz/after.json" >/dev/null 2>&1 || true
    CSVDIFF_FUSED_JOIN=1 $BIN compare "$fz/f_a.$ext" "$fz/f_b.$ext" -k k --threads $t \
        --json "$fz/inside.json" >/dev/null 2>&1 || true
    if python3 -c 'import json,sys; a,b=(json.load(open(p)) for p in sys.argv[1:3]); sys.exit(0 if (a["counts"],a["columns"])==(b["counts"],b["columns"]) and a["counts"]["matched"]>0 else 1)' \
        "$fz/after.json" "$fz/inside.json"; then
      printf '  ok    %s, %s thread(s): the join inside the sweep finds what the join after it finds\n' "$ext" "$t"
    else
      printf '  FAIL  %s, %s thread(s): the join inside the sweep disagrees\n' "$ext" "$t"; fail=1
    fi
  done
done
# A guessed split inside a quoted field: one field of 20,000 lines sits across
# the middle of each file, so the halfway guess lands on a newline inside it and
# the sweep has to start over on counted splits -- and throw away what it had
# already joined, not only what it had indexed.
python3 - "$fz" <<'PY'
import sys
d = sys.argv[1]
for side in ("a", "b"):
    rows = ["k,v,note\n"]
    rows += [f"R{i:06d},{i},plain\n" for i in range(110000)]
    rows.append(f"MID,{'x' if side == 'a' else 'y'},\"" + "inside\n" * 20000 + "\"\n")
    rows += [f"S{i:06d},{i + (side == 'b' and i == 7)},plain\n" for i in range(110000)]
    open(f"{d}/g_{side}.csv", "w").write("".join(rows))
PY
one=$($BIN compare "$fz/g_a.csv" "$fz/g_b.csv" -k k --threads 1 2>&1 | head -1 | answer) || true
inside=$(CSVDIFF_FUSED_JOIN=1 $BIN compare "$fz/g_a.csv" "$fz/g_b.csv" -k k --threads 4 2>&1 | head -1 | answer) || true
case "$one" in
  *"(changed 2) | added 0 | removed 0"*)
    if [ "$inside" = "$one" ]; then
      echo "  ok    a split guessed inside a quoted field is caught, and the join inside the sweep starts over"
    else
      printf '  FAIL  the join inside the sweep kept rows from a wrong guess\n    1     : %s\n    inside: %s\n' "$one" "$inside"; fail=1
    fi ;;
  *) echo "  FAIL  the guessed-split fixture: $one"; fail=1 ;;
esac
rm -rf "$fz"

exit $fail
