#!/usr/bin/env bash
# Holds this port to the answers the Rust port gives, on the fixture built from
# every shape that has broken an engine in this project. Run from cpp/.
# No pipefail: a compare exits 1 when it finds differences, which is the
# expected outcome of almost every check here.
set -uo pipefail
set +o pipefail
set -e
cd "$(dirname "$0")"
make >/dev/null
RUST=../rust/target/release/compara
[ -x "$RUST" ] || { echo "build the Rust port first: (cd ../rust && cargo build --release)"; exit 2; }

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
  c=$(build/csvdiff compare "$a" "$b" -k k "$@" 2>&1 | summary) || true
  if [ "$r" = "$c" ]; then
    printf '  ok    %s\n' "$label"
  else
    printf '  FAIL  %s\n    rust: %s\n    c++ : %s\n' "$label" "$r" "$c"; fail=1
  fi
}

echo "awkward fixture, c++ against rust:"
check "defaults"
check "--trim" --trim
# --ignore-case is deliberately excluded: this port refuses non-ASCII folding,
# which the fixture contains on purpose. See README.md.

echo "newline-delimited JSON:"
tmpj=$(mktemp -d)
# The same rows twice: once with the keys in one order and the escapes spelled
# with \uXXXX, once in another order with the characters written literally. A
# JSON writer is free to do either, so these must compare equal.
printf '{"k":"1","v":"caf\\u00e9","w":"x"}\n{"k":"2","v":"has \\"quotes\\"","w":"y"}\n{"k":"3","v":"line\\nbreak","w":"z"}\n{"k":"4","v":null,"w":"w"}\n{"k":"5","v":"emoji \\ud83d\\ude00","w":"v"}\n' > "$tmpj/a.ndjson"
printf '{"w":"x","k":"1","v":"caf\xc3\xa9"}\n{"w":"y","k":"2","v":"has \\"quotes\\""}\n{"w":"z","k":"3","v":"line\\nbreak"}\n{"w":"w","k":"4","v":null}\n{"w":"v","k":"5","v":"emoji \xf0\x9f\x98\x80"}\n' > "$tmpj/b.ndjson"
out=$(./build/csvdiff compare "$tmpj/a.ndjson" "$tmpj/b.ndjson" -k k 2>&1 | head -1 | sed "s/ | turbo.*//") || true
case "$out" in
  *"matched 5 (changed 0)"*) echo "  ok    key order, \\uXXXX against literal UTF-8, null, surrogate pair" ;;
  *) echo "  FAIL  json escapes: $out"; fail=1 ;;
esac

# A CSV file against the JSON of the same rows: the formats meet at the join.
printf 'k,v,w\n1,alpha,x\n2,beta,y\n3,gamma,z\n' > "$tmpj/c.csv"
printf '{"k":"1","v":"alpha","w":"x"}\n{"k":"2","v":"beta","w":"y"}\n{"k":"3","v":"CHANGED","w":"z"}\n' > "$tmpj/c.ndjson"
out=$(./build/csvdiff compare "$tmpj/c.csv" "$tmpj/c.ndjson" -k k 2>&1 | head -1 | sed "s/ | turbo.*//") || true
case "$out" in
  *"matched 3 (changed 1)"*) echo "  ok    a CSV file compared against a JSON file" ;;
  *) echo "  FAIL  mixed csv/json: $out"; fail=1 ;;
esac
# The ndjson byte proof, and the ways a prefix cannot be taken on trust.
#
# A value is found by name, the two files need not list the names in the same
# order, and a name repeated in one object takes its *last* value -- so a second
# `v` past the diverging byte carries a value the proven prefix never saw. Each
# case is one row, cross-checked against the C, Rust and Zig ports on the same
# bytes.
echo "the ndjson byte proof:"
proof() { # name, a-row, b-row, expected "changed N"
  printf '%s\n' "$2" > "$tmpj/pa.ndjson"
  printf '%s\n' "$3" > "$tmpj/pb.ndjson"
  got=$(./build/csvdiff compare "$tmpj/pa.ndjson" "$tmpj/pb.ndjson" -k k -i w 2>&1 | head -1) || true
  case "$got" in
    *"$4"*) echo "  ok    $1" ;;
    *) echo "  FAIL  $1: wanted $4 -- $got"; fail=1 ;;
  esac
}
proof "a compared name repeated in the mate's tail" \
  '{"k":"1","v":"5","w":"x"}' '{"k":"1","v":"5","w":"x","v":"9"}' "changed 1"
proof "the same values written in a different order" \
  '{"k":"1","v":"7","w":"y"}' '{"k":"1","w":"y","v":"7"}' "changed 0"
proof "a value that is a prefix of the mate's longer one" \
  '{"k":"1","v":"12","w":"z"}' '{"k":"1","v":"123","w":"z"}' "changed 1"
proof "a difference confined to an ignored column" \
  '{"k":"1","v":"5","w":"one"}' '{"k":"1","v":"5","w":"two"}' "changed 0"
proof "the key written after the last compared value" \
  '{"v":"5","w":"x","k":"1"}' '{"v":"6","w":"x","k":"1"}' "changed 1"
# Past an ignored value in the middle: the rows first differ inside the value
# of `w`, which nobody tracks, so that value is skipped in each row on its own
# and the comparison carries on -- whatever its length, escapes, type or nested
# names. A repeat of the compared name after it still wins.
proof "an ignored middle value that differs" \
  '{"k":"1","w":"x","v":"5"}' '{"k":"1","w":"x and longer","v":"5"}' "changed 0"
proof "and a real change after it" \
  '{"k":"1","w":"x","v":"5"}' '{"k":"1","w":"x and longer","v":"6"}' "changed 1"
proof "an escaped ignored value against null" \
  '{"k":"1","w":"a\"b,c}","v":"5"}' '{"k":"1","w":null,"v":"5"}' "changed 0"
proof "nested ignored values, tracked names inside" \
  '{"k":"1","w":{"v":"Z"},"v":"5"}' '{"k":"1","w":[1,{"v":"Y"}],"v":"5"}' "changed 0"
proof "a repeat of the compared name after the gap" \
  '{"k":"1","w":"x","v":"5"}' '{"k":"1","w":"y","v":"5","v":"LATER"}' "changed 1"

# The CSV byte proof with gaps: an ignored column among compared ones is
# skipped by field count in each row on its own, so a value there that differs
# -- in length, in quoting, with a delimiter inside the quotes -- does not stop
# a row proving equal, and does not shift what the proof compares after it.
# The last column is compared, so the proof also closes on the line ending.
# Each pair is held to the Rust port's answer.
echo "the csv byte proof, past an ignored column:"
gapcheck() { # label, a-file, b-file, then flags
  local label=$1 a=$2 b=$3; shift 3
  local r c
  r=$("$RUST" compare "$a" "$b" "$@" --engine turbo -o /dev/null 2>&1 | summary) || true
  c=$(build/csvdiff compare "$a" "$b" "$@" 2>&1 | summary) || true
  if [ "$r" = "$c" ]; then printf '  ok    %s\n' "$label"
  else printf '  FAIL  %s\n    rust: %s\n    c++ : %s\n' "$label" "$r" "$c"; fail=1; fi
}
{ echo 'id,skip,a,b'; echo 'k1,S1,p,q'; echo 'k2,S1,p,q'; echo 'k3,S1,p,q'; } > "$tmpj/gap_a.csv"
{ echo 'id,skip,a,b'; echo 'k1,"S, ""two"" and more",p,q'; echo 'k2,,p,X'; echo 'k3,S2,p,qq'; } > "$tmpj/gap_b.csv"
gapcheck "a middle ignored column that differs" "$tmpj/gap_a.csv" "$tmpj/gap_b.csv" -k id -i skip
{ echo 'id,skip,a,b'; echo 'k1,S1,p,q'; echo 'k2,S1,p,q'; } > "$tmpj/gs_a.csv"
{ echo 'id,skip,a,b'; echo 'k1'; echo 'k2,S2'; } > "$tmpj/gs_b.csv"
gapcheck "a mate that stops inside the gap" "$tmpj/gs_a.csv" "$tmpj/gs_b.csv" -k id -i skip
{ echo 'skip,id,a,t,b'; echo 'S1,k1,p,T1,q'; echo 'S1,k2,p,T1,q'; } > "$tmpj/g2_a.csv"
{ echo 'skip,id,a,t,b'; echo 'S22,k1,p,T2,q'; echo 'S,k2,p,T222,Y'; } > "$tmpj/g2_b.csv"
gapcheck "a leading gap and a second one" "$tmpj/g2_a.csv" "$tmpj/g2_b.csv" -k id -i skip,t
printf 'id,skip,a\nk1,S1,p\n' > "$tmpj/gc_a.csv"
printf 'id,skip,a\r\nk1,S22,p\r\n' > "$tmpj/gc_b.csv"
gapcheck "a gap, and CRLF against LF" "$tmpj/gc_a.csv" "$tmpj/gc_b.csv" -k id -i skip
{ echo 'a,k,c'; echo 'x,K1,c1'; echo 'y,K2,cc'; } > "$tmpj/pre_a.csv"
{ echo 'a,k,c'; echo 'x,K1,c1'; echo 'y,K2,cccccccc'; } > "$tmpj/pre_b.csv"
gapcheck "a mate whose last column carries on" "$tmpj/pre_a.csv" "$tmpj/pre_b.csv" -k k

# --ignore-case refuses a value outside ASCII only where folding it decides the
# answer: a key, or a compared value that differs byte for byte. A value that is
# unchanged, or in a row only one side has, is answered -- with or without
# --json, which used to refuse the second while building the row samples.
echo "--ignore-case outside ASCII:"
fold() { # name, a-rows, b-rows (printf %b escapes), expected exit status
  printf 'k,v\n1,alpha\n%b\n' "$2" > "$tmpj/fa.csv"
  printf 'k,v\n1,ALPHA\n%b\n' "$3" > "$tmpj/fb.csv"
  for json in "" "--json $tmpj/fold.json"; do
    # shellcheck disable=SC2086  # $json is zero words or two
    code=0; ./build/csvdiff compare "$tmpj/fa.csv" "$tmpj/fb.csv" -k k --ignore-case $json \
      >/dev/null 2>&1 || code=$?
    if [ "$code" = "$4" ]; then echo "  ok    $1${json:+, --json}"
    else echo "  FAIL  $1${json:+, --json}: exit $code, wanted $4"; fail=1; fi
  done
}
fold "unchanged, outside ASCII" '2,CAF\xc3\x89' '2,CAF\xc3\x89' 0
fold "only in an added row" '2,x' '2,x\n3,na\xc3\xafve' 1
fold "absent on one side" '2,' '2,\xc3\x89' 1
fold "differs, outside ASCII: refused" '2,CAF\xc3\x89' '2,caf\xc3\xa9' 2
fold "a key outside ASCII: refused" '\xc3\x89,x' '\xc3\x89,x' 2

rm -rf "$tmpj"

echo "the threaded csv sweep, over quoted newlines and chunk boundaries:"
# --- the threaded CSV sweep -------------------------------------------------
#
# Rows are found on every core, so a thread starts mid-file and has to work out
# whether it landed inside a quoted field. Nothing else in this suite reaches
# the 4 MB size where that splitting turns on, so this builds a file that does,
# out of the shapes that make a boundary hard: newlines inside quotes, doubled
# quotes, delimiters inside quotes, CRLF, blank lines, duplicate keys and rows
# present on one side only.
#
# Getting the in-quote state wrong at a boundary does not fail loudly -- it cuts
# a row in half, and the counts move. The check is that the thread count cannot
# change the answer. Modelled on the same section in `c/test.sh`.
thr=$(mktemp -d)
python3 - "$thr" <<'PYEOF'
import sys
out_dir = sys.argv[1]
FIELDS = ["plain", '"has,comma"', '"has\nnewline\nand more"',
          '"doubled ""quote"" inside"', '"comma, and\nnewline together"', '""']
def rows(side):
    out = ["k,a,b,c\n"]
    for i in range(130000):
        if side == "b" and i % 5000 == 0:
            continue                                   # removed from B
        a = FIELDS[(i * 7 + (3 if side == "b" and i % 41 == 0 else 0)) % len(FIELDS)]
        b = str(i * 3 + (1 if side == "b" and i % 23 == 0 else 0))
        c = '"tail\nwith newline"' if i % 11 == 0 else "tail"
        sep = "\r\n" if i % 13 == 0 else "\n"
        out.append(f"K{i:07d},{a},{b},{c}{sep}")
        if i % 997 == 0:
            out.append(f"K{i:07d},{a},{b},{c}{sep}")    # a duplicate key
        if i % 1499 == 0:
            out.append("\n")                           # a blank line is not a row
    if side == "b":
        for i in range(40):
            out.append(f"NEW{i:05d},plain,0,tail\n")    # added in B
    return "".join(out)
for side in ("a", "b"):
    open(f"{out_dir}/t_{side}.csv", "w").write(rows(side))
PYEOF
bytes=$(wc -c < "$thr/t_a.csv")
if [ "$bytes" -lt 4194304 ]; then
  echo "  FAIL  the threading fixture is $bytes bytes, under the 4 MB split threshold"; fail=1
else
  one=$(build/csvdiff compare "$thr/t_a.csv" "$thr/t_b.csv" -k k --threads 1 2>&1 | summary) || true
  for t in 2 3 4 7 16; do
    many=$(build/csvdiff compare "$thr/t_a.csv" "$thr/t_b.csv" -k k --threads $t 2>&1 | summary) || true
    if [ "$one" = "$many" ]; then
      printf '  ok    %s threads finds what 1 thread finds\n' "$t"
    else
      printf '  FAIL  %s threads disagrees with 1 thread\n    1 : %s\n    %s : %s\n' \
             "$t" "$one" "$t" "$many"; fail=1
    fi
  done
  r=$("$RUST" compare "$thr/t_a.csv" "$thr/t_b.csv" -k k --engine turbo -o /dev/null 2>&1 | summary) || true
  [ "$one" = "$r" ] && echo "  ok    and what the rust port finds" \
    || { echo "  FAIL  the threaded sweep disagrees with the rust port"; echo "    rust: $r"; fail=1; }
fi
# The splits are guessed at the next newline and checked after the sweep; only
# a wrong guess pays for counting quotes. This pair makes the guess wrong on
# purpose: one quoted field of 20,000 lines sits across the middle of each file,
# so the halfway split lands on a newline inside it, and the rows on either side
# of it carry the changes that a chunk started there would misread.
python3 - "$thr" <<'PY'
import sys
d = sys.argv[1]
for side in ("a", "b"):
    rows = ["k,v,note\n"]
    rows += [f"R{i:06d},{i},plain\n" for i in range(110000)]
    rows.append(f"MID,{'x' if side == 'a' else 'y'},\"" + "inside\n" * 20000 + "\"\n")
    rows += [f"S{i:06d},{i + (side == 'b' and i == 7)},plain\n" for i in range(110000)]
    open(f"{d}/g_{side}.csv", "w").write("".join(rows))
PY
one=$(build/csvdiff compare "$thr/g_a.csv" "$thr/g_b.csv" -k k --threads 1 2>&1 | summary) || true
many=$(build/csvdiff compare "$thr/g_a.csv" "$thr/g_b.csv" -k k --threads 4 2>&1 | summary) || true
case "$one" in
  *"(changed 2) | added 0 | removed 0"*)
    if [ "$one" = "$many" ]; then
      echo "  ok    a split guessed inside a quoted field is caught and counted instead"
    else
      printf '  FAIL  a split guessed inside a quoted field changed the answer\n    1 : %s\n    4 : %s\n' "$one" "$many"; fail=1
    fi ;;
  *) echo "  FAIL  the guessed-split fixture: $one"; fail=1 ;;
esac
# The same pair with the join inside the sweep: the wrong guess has to throw
# away what the sweep already joined, not only what it indexed.
inside=$(CSVDIFF_FUSED_JOIN=1 build/csvdiff compare "$thr/g_a.csv" "$thr/g_b.csv" -k k --threads 4 2>&1 | summary) || true
if [ "$inside" = "$one" ]; then
  echo "  ok    and the join inside the sweep starts over with it"
else
  printf '  FAIL  the join inside the sweep kept rows from a wrong guess\n    after : %s\n    inside: %s\n' "$one" "$inside"; fail=1
fi

# Past page cache the join runs inside A's sweep, joining every row as though it
# were its key's first and taking back, afterwards, the rows that repeat a key.
# CSVDIFF_FUSED_JOIN=1 takes that path at any size; with --json it also runs the
# join after the sweep and fails unless the two agree, column counts included.
# The repeats here differ from their key's first row -- in a compared value, or
# in being absent from B -- and the file is past the 4 MB split, so a repeat that
# was not taken back, or a chunk's last row cut short, changes the counts.
python3 - "$thr" <<'PY'
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
    after=$(build/csvdiff compare "$thr/f_a.$ext" "$thr/f_b.$ext" -k k --threads $t 2>&1 | summary) || true
    inside=$(CSVDIFF_FUSED_JOIN=1 build/csvdiff compare "$thr/f_a.$ext" "$thr/f_b.$ext" -k k \
        --threads $t 2>&1 | summary) || true
    if [ "$after" = "$inside" ]; then
      printf '  ok    %s, %s thread(s): the join inside the sweep finds what the join after it finds\n' "$ext" "$t"
    else
      printf '  FAIL  %s, %s thread(s): the join inside the sweep disagrees\n    after : %s\n    inside: %s\n' \
          "$ext" "$t" "$after" "$inside"; fail=1
    fi
  done
  code=0
  out=$(CSVDIFF_FUSED_JOIN=1 build/csvdiff compare "$thr/f_a.$ext" "$thr/f_b.$ext" -k k \
      --json "$thr/f.json" 2>&1 >/dev/null) || code=$?
  if [ "$code" -le 1 ] && [ -z "$out" ]; then
    printf '  ok    %s: and its column counts and row lists, checked against the join after it\n' "$ext"
  else
    printf '  FAIL  %s: the join inside the sweep, with --json: %s\n' "$ext" "${out:-exit $code}"; fail=1
  fi
done
rm -rf "$thr"

echo "quoting, ragged rows and keys near the end of the file:"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
printf 'a,k,c\nx,K1,c1\ny,K2,cc\n'        > "$tmp/a.csv"
printf 'a,k,c\nx,K1,c1\ny,K2,cccccccc\n'  > "$tmp/b.csv"
r=$("$RUST" compare "$tmp/a.csv" "$tmp/b.csv" -k k --engine turbo -o /dev/null 2>&1 | head -1 | sed 's/ | turbo.*//') || true
c=$(build/csvdiff compare "$tmp/a.csv" "$tmp/b.csv" -k k 2>&1 | head -1 | sed 's/ | turbo.*//') || true
[ "$r" = "$c" ] && echo "  ok    key in the last bytes of the file" || { echo "  FAIL  key near end: rust=$r cpp=$c"; fail=1; }

echo "written natively and read back:"
# The generator writes Parquet from the same field-by-field recipe it writes CSV
# from, so the two can be held against each other without a third tool in the
# way -- and these checks run wherever the port builds, rather than only where
# DuckDB happens to be installed.
make gen-data >/dev/null
tmpg=$(mktemp -d)
gen() { build/gen-data --rows "$1" --out-dir "$tmpg" --prefix "$2" "${@:3}" >/dev/null; }
gencheck() { # label, rows, prefix, then generator flags
  local label=$1 rows=$2 prefix=$3; shift 3
  gen "$rows" "$prefix"
  gen "$rows" "$prefix" "$@"
  local ext=.parquet
  case " $* " in
    *" json "*) ext=.ndjson ;;
    *" none "*) ext=.unc.parquet ;;
  esac
  build/csvdiff compare "$tmpg/${prefix}_a.csv" "$tmpg/${prefix}_b.csv" \
    -k account_id,txn_id -i updated_at --json "$tmpg/csv.json" >/dev/null 2>&1 || true
  build/csvdiff compare "$tmpg/${prefix}_a$ext" "$tmpg/${prefix}_b$ext" \
    -k account_id,txn_id -i updated_at --json "$tmpg/pq.json" >/dev/null 2>&1 || true
  if python3 - "$tmpg/csv.json" "$tmpg/pq.json" <<'PYEOF'
import json, sys
def load(path):
    d = json.load(open(path))
    for gone in ("a", "b", "seconds"):
        d["meta"].pop(gone, None)
    return d
sys.exit(0 if load(sys.argv[1]) == load(sys.argv[2]) else 1)
PYEOF
  then printf '  ok    %s\n' "$label"
  else printf '  FAIL  %s: the parquet report differs from the csv one\n' "$label"; fail=1; fi
  rm -f "$tmpg/${prefix}"_*
}
gencheck "newline-delimited json" 20k s0 --format json
gencheck "snappy, one row group" 20k s1 --format parquet --compression snappy
gencheck "uncompressed" 20k s2 --format parquet --compression none
gencheck "many small row groups" 20k s3 --format parquet --compression snappy --row-group-size 512
# A dictionary budget the data crosses partway makes a column that is dictionary
# encoded in some row groups and plain in others -- the shape a real writer
# produces on a high-cardinality string, and the one the reader has to fold into
# a single form. With these numbers three of the twenty columns come out mixed.
gencheck "a column the dictionary gives up on" 20k s4 \
  --format parquet --compression none --dict-limit 175 --row-group-size 300
gencheck "every column plain" 20k s5 --format parquet --compression snappy --dict-limit 1
gencheck "rows that do not fill a group" 1k s6 --format parquet --compression none

# Round-tripping the writer through the reader only proves they agree with each
# other. Reading the file back with a tool that had no part in writing it is
# what says it is Parquet rather than something that merely looks like it.
DUCK=""
for cand in ../bench/external/tools/duckdb "$(command -v duckdb || true)"; do
  [ -n "$cand" ] && [ -x "$cand" ] && { DUCK=$cand; break; }
done
if [ -n "$DUCK" ]; then
  gen 20k iop
  gen 20k iop --format parquet --compression snappy
  ours=$("$DUCK" -noheader -csv -c "
    SELECT md5(string_agg(account_id||'|'||txn_id||'|'||coalesce(value_date,'')||'|'||
                          currency||'|'||amount||'|'||note, ':'))
    FROM read_parquet('$tmpg/iop_b.parquet');" 2>/dev/null)
  csv=$("$DUCK" -noheader -csv -c "
    SELECT md5(string_agg(account_id||'|'||txn_id||'|'||coalesce(value_date,'')||'|'||
                          currency||'|'||amount||'|'||note, ':'))
    FROM read_csv('$tmpg/iop_b.csv', all_varchar = true, header = true, sample_size = -1);" 2>/dev/null)
  if [ -n "$ours" ] && [ "$ours" = "$csv" ]; then
    echo "  ok    duckdb reads what we wrote, to the same digest as the csv"
  else
    echo "  FAIL  duckdb read our parquet differently from the csv"; fail=1
  fi
  # And the other way: our file against DuckDB's file of the same rows.
  "$DUCK" -c "COPY (SELECT * FROM read_csv('$tmpg/iop_b.csv', all_varchar = true,
              header = true, sample_size = -1)) TO '$tmpg/duck.parquet'
              (FORMAT PARQUET, COMPRESSION snappy);" >/dev/null 2>&1
  out=$(build/csvdiff compare "$tmpg/iop_b.parquet" "$tmpg/duck.parquet" \
        -k account_id,txn_id 2>&1 | head -1 | sed "s/ | turbo.*//") || true
  case "$out" in
    *"(changed 0)"*"added 0 | removed 0"*)
      echo "  ok    our parquet and duckdb's of the same rows are identical" ;;
    *) echo "  FAIL  ours against duckdb's: $out"; fail=1 ;;
  esac
else
  echo "  skip  no duckdb, so the interop checks did not run"
fi
rm -rf "$tmpg"

echo "parquet, read natively and compared columnwise:"
# The claim the columnar path has to earn is that it is the *same* comparison:
# the same data, in a format that stores it as columns of dictionary indices
# rather than as lines of text, must produce a byte-identical report. So the
# fixture is converted and the two JSON documents are diffed in full -- counts,
# per-column statistics, every changed cell, every added and removed row, the
# duplicate sections -- with only the file names and the timing removed.
DUCK=""
for cand in ../bench/external/tools/duckdb "$(command -v duckdb || true)"; do
  [ -n "$cand" ] && [ -x "$cand" ] && { DUCK=$cand; break; }
done
if [ -z "$DUCK" ]; then
  echo "  skip  no duckdb to write parquet with"
else
  tmpp=$(mktemp -d)
  for side in a b; do
    for spec in "snappy:$tmpp/$side.parquet:122880" "uncompressed:$tmpp/$side.unc.parquet:8"; do
      IFS=: read -r codec dest rg <<< "$spec"
      "$DUCK" -c "COPY (SELECT * FROM read_csv('../tests/fixtures/awkward_$side.csv',
                   all_varchar = true, header = true, sample_size = -1, null_padding = true))
                 TO '$dest' (FORMAT PARQUET, COMPRESSION $codec, ROW_GROUP_SIZE $rg);" >/dev/null
    done
  done
  # A small row group size on the uncompressed side is not decoration: it makes
  # the writer start a fresh dictionary per group, and at some point give up on
  # the dictionary and write plain pages instead, which is the mixed-encoding
  # column the reader has to fold together.
  pqcheck() { # label, then the flags
    local label=$1; shift
    build/csvdiff compare ../tests/fixtures/awkward_a.csv ../tests/fixtures/awkward_b.csv \
      -k k "$@" --json "$tmpp/csv.json" >/dev/null 2>&1 || true
    local bad=0
    for kind in "" ".unc"; do
      build/csvdiff compare "$tmpp/a$kind.parquet" "$tmpp/b$kind.parquet" \
        -k k "$@" --json "$tmpp/pq.json" >/dev/null 2>&1 || true
      python3 - "$tmpp/csv.json" "$tmpp/pq.json" <<'PYEOF' || bad=1
import json, sys
def load(path):
    d = json.load(open(path))
    for gone in ("a", "b", "seconds"):
        d["meta"].pop(gone, None)
    return d
sys.exit(0 if load(sys.argv[1]) == load(sys.argv[2]) else 1)
PYEOF
    done
    if [ "$bad" = 0 ]; then printf '  ok    %s\n' "$label"
    else printf '  FAIL  %s: the parquet report differs from the csv one\n' "$label"; fail=1; fi
  }
  pqcheck "same report as the csv it was written from"
  pqcheck "--trim" --trim
  pqcheck "--empty-is-null" --empty-is-null
  pqcheck "--tolerance falls back to the byte path" --tolerance 0.5
  pqcheck "--max-rows caps the same rows" --max-rows 2

  # A key column both sides store as a dictionary takes the shared-id path,
  # where the join never looks at a byte after the dictionaries are mapped.
  pqcheck "a dictionary key column" -k v

  # Parquet has real nulls; CSV has only empty fields. Both must read as absent.
  "$DUCK" -c "COPY (SELECT * FROM (VALUES ('k1','a',NULL),('k2',NULL,''),('k3','c','z'))
              v(k, p, q)) TO '$tmpp/n_a.parquet' (FORMAT PARQUET);
              COPY (SELECT * FROM (VALUES ('k1','a',''),('k2','',NULL),('k3','c','y'))
              v(k, p, q)) TO '$tmpp/n_b.parquet' (FORMAT PARQUET);" >/dev/null
  out=$(build/csvdiff compare "$tmpp/n_a.parquet" "$tmpp/n_b.parquet" -k k 2>&1 | head -1 |
        sed "s/ | turbo.*//") || true
  case "$out" in
    *"matched 3 (changed 1)"*) echo "  ok    a null and an empty string are both absent" ;;
    *) echo "  FAIL  parquet nulls: $out"; fail=1 ;;
  esac

  # A column a writer starts as a dictionary and gives up on partway is the
  # shape DuckDB produces on any high-cardinality string at scale, and the one
  # the reader has to fold into a single form. Built here on purpose: `v` is
  # five distinct values for the first two row groups and then all distinct,
  # `w` carries nulls, and the two sides are written with different codecs so
  # the compressed and uncompressed slice bases both get exercised in one
  # comparison.
  "$DUCK" -c "
    CREATE TABLE t AS SELECT 'k'||lpad(i::VARCHAR, 6, '0') AS k,
      CASE WHEN i < 8192 THEN 'lo'||(i%5) ELSE 'hi'||i END AS v,
      CASE WHEN i%3 = 0 THEN NULL ELSE 'w'||(i%7) END AS w FROM range(20000) t(i);
    CREATE TABLE u AS SELECT k, CASE WHEN i%1000 = 0 THEN 'X' ELSE v END AS v, w
      FROM (SELECT *, row_number() OVER () - 1 AS i FROM t);
    COPY (SELECT * FROM t) TO '$tmpp/m_a.parquet'
      (FORMAT PARQUET, COMPRESSION uncompressed, ROW_GROUP_SIZE 4096);
    COPY (SELECT * FROM u) TO '$tmpp/m_b.parquet'
      (FORMAT PARQUET, COMPRESSION snappy, ROW_GROUP_SIZE 4096);
    COPY (SELECT * FROM t) TO '$tmpp/m_a.csv' (HEADER, FORMAT CSV);
    COPY (SELECT * FROM u) TO '$tmpp/m_b.csv' (HEADER, FORMAT CSV);" >/dev/null
  build/csvdiff compare "$tmpp/m_a.csv" "$tmpp/m_b.csv" -k k --json "$tmpp/m1.json" \
    >/dev/null 2>&1 || true
  build/csvdiff compare "$tmpp/m_a.parquet" "$tmpp/m_b.parquet" -k k --json "$tmpp/m2.json" \
    >/dev/null 2>&1 || true
  if python3 - "$tmpp/m1.json" "$tmpp/m2.json" <<'PYEOF'
import json, sys
def load(path):
    d = json.load(open(path))
    for gone in ("a", "b", "seconds"):
        d["meta"].pop(gone, None)
    return d
sys.exit(0 if load(sys.argv[1]) == load(sys.argv[2]) else 1)
PYEOF
  then echo "  ok    a column that starts as a dictionary and gives up partway"
  else echo "  FAIL  mixed dictionary/plain column"; fail=1; fi

  # One side parquet and one side text is refused rather than half-answered.
  out=$(build/csvdiff compare "$tmpp/a.parquet" ../tests/fixtures/awkward_b.csv -k k 2>&1) || true
  case "$out" in
    *"one file is parquet and the other is not"*) echo "  ok    a mixed parquet/text pair is refused" ;;
    *) echo "  FAIL  mixed parquet/text: $out"; fail=1 ;;
  esac
  rm -rf "$tmpp"
fi

# A misspelled --ignore used to widen the comparison in silence: the column it
# meant to drop got compared and came back changed on every row. `--key` and
# `--compare` have always refused an unknown name; this is the third.
igdir=$(mktemp -d)
printf 'id,a,b\nk1,p,q\n' > "$igdir/a.csv"
printf 'id,a,b\nk1,p,X\n' > "$igdir/b.csv"
out=$(build/csvdiff compare "$igdir/a.csv" "$igdir/b.csv" -k id -i no_such_column 2>&1) || true
case "$out" in
  *"present in neither file"*) echo "  ok    an unknown --ignore name is refused" ;;
  *) echo "  FAIL  expected a refusal, got: $out"; fail=1 ;;
esac
# The other half: a real name still works, so the check refuses typos rather
# than refusing --ignore.
out=$(build/csvdiff compare "$igdir/a.csv" "$igdir/b.csv" -k id -i b 2>&1) || true
case "$out" in
  *"changed 0"*) echo "  ok    a real --ignore name is still accepted" ;;
  *) echo "  FAIL  a real --ignore name should be accepted, got: $out"; fail=1 ;;
esac
rm -rf "$igdir"

exit $fail
