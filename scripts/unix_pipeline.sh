#!/usr/bin/env bash
# Compare two CSV files on a composite key with sort(1) and join(1).
#
# This is the pipeline people write when they do not reach for a tool: project the key and
# the compared columns, sort both sides, join them, and count what differs. It is the same
# algorithm as this project's sortmerge engine — external sort, merge join — in C that has
# been tuned for forty years, so it is the honest ceiling for that approach.
#
# What it cannot do is the rest of the job. sort and join have no idea what CSV is, so it
# assumes no field contains a comma, a quote or a newline; it has no concept of a duplicate
# key, so a repeated key becomes a cross product exactly as in SQL; and it produces three
# numbers rather than a diff.
#
# Usage: unix_pipeline.sh A.csv B.csv WORKDIR
set -euo pipefail

a=$1; b=$2; work=$3
mkdir -p "$work"

# join(1) splits on the delimiter, so a line has to be exactly two fields: the key, then
# everything being compared as one opaque blob. US (0x1f) separates the columns inside that
# blob and RS (0x1e) the two key columns; neither can appear in a CSV field. Column 20 is
# updated_at, the ignored one, so the projection stops at 19.
project() {
  tail -n +2 "$1" | awk -F, -v OFS='' '{
    rest = $3
    for (i = 4; i <= 19; i++) rest = rest "\037" $i
    printf "%s\036%s\t%s\n", $1, $2, rest
  }' | LC_ALL=C sort -t $'\t' -k1,1 -S 25% -T "$work"
}

project "$a" > "$work/a.tsv"
project "$b" > "$work/b.tsv"

# -a1 -a2 keeps the lines that pair with nothing, and -e fills the absent half with a marker
# no field can hold, so the counting pass can tell a missing row from an empty value.
LC_ALL=C join -t $'\t' -j 1 -a 1 -a 2 -e $'\035' -o '0,1.2,2.2' \
  "$work/a.tsv" "$work/b.tsv" \
  | awk -F'\t' -v gone=$'\035' '
      $2 == gone { added++;   next }
      $3 == gone { removed++; next }
      $2 != $3   { changed++ }
      END { printf "changed=%d\nadded=%d\nremoved=%d\n", changed, added, removed }
    '
