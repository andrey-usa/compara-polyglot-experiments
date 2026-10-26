#!/usr/bin/env bash
# Run a command, keep its output in the job log, and when it exits non-zero
# publish the last 6 KB as an annotation, so a crash explains itself through
# the checks API even where job logs can't be downloaded.
#
#   bash bench/agent-eval/logged.sh "agent run log (purchase, windows)" python bench/agent-eval/run.py ...
title=$1; shift
log=$(mktemp)
set +e
"$@" 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
if [ "$rc" -ne 0 ]; then
  tail -c 6000 "$log" > "$log.tail"
  python "$(dirname "$0")/../ladder/annotate.py" "$log.tail" "$title (exit $rc)"
fi
exit "$rc"
