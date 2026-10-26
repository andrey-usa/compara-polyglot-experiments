#!/usr/bin/env bash
# T-0047 result recovery (agent fb). Called once per phase by
# .github/workflows/publish-t0047.yml with PHASE=probe|download|summarize|publish.
#
# Why phases: the paid eval workflows upload their raw output as artifacts, but
# agent sandboxes cannot download artifacts (that API always needs auth), and
# the eval jobs' own publish steps have died on this branch before. Each phase
# below appends to publish/diagnostics/<phase>.log and then commits and pushes
# that log to $TARGET_BRANCH (experiment-44), so a phase that dies still leaves
# everything up to its last command readable over git. Each phase also reports
# success/failure as its own CI step.
#
# Queue entries live in publish/jobs/*.txt:
#   mode=miniwob|webarena
#   run=<run id>  artifact=<artifact name>  dest=<where results land>
#   note=<free text recorded in the provenance>
#
# No `set -e`: every step's status is checked and logged explicitly.
set -uo pipefail

phase="${PHASE:?set PHASE}"
target="${TARGET_BRANCH:-experiment-44}"
run="${GITHUB_RUN_ID:-local}"
log="publish/diagnostics/${phase}.log"
mkdir -p publish/diagnostics

say() { echo "$*" | tee -a "$log"; }
blob() { printf '%s\n' "$*" >>"$log"; }

# cmd <label> <command...>: log the command, its output, its exit status.
cmd() {
  local label="$1"; shift
  local out st
  blob "\$ $*"
  out=$("$@" 2>&1); st=$?
  blob "$out"
  blob "[$label exit $st]"
  return $st
}

# queue <key> <file>: first value of a key in a queue file
queue() { grep -m1 "^$1=" "$2" 2>/dev/null | cut -d= -f2-; }

push_results() {
  git config user.name "andrey-usa"
  git config user.email "62624421+andrey-usa@users.noreply.github.com"
  git add -f publish/diagnostics "$@" >>"$log" 2>&1
  if ! git commit -q -m "publish-t0047 ${phase} run ${run} [skip ci]" >>"$log" 2>&1; then
    say "nothing new to commit"
  fi
  local pushed=0 i
  for i in 1 2 3 4 5; do
    git fetch -q origin "$target" >>"$log" 2>&1
    if git push origin "HEAD:refs/heads/$target" >>"$log" 2>&1; then
      say "push to $target ok on attempt $i"
      pushed=1
      break
    fi
    say "push attempt $i rejected"
    if git rebase -q "origin/$target" >>"$log" 2>&1; then
      say "rebased onto origin/$target"
    else
      git rebase --abort >>"$log" 2>&1
    fi
    sleep $((i * 5))
  done
  if [ "$pushed" != 1 ]; then
    say "push to $target failed after 5 attempts"
    if [ -n "${GITHUB_REF_NAME:-}" ] && [ "$GITHUB_REF_NAME" != "$target" ]; then
      say "pushing the same commit to $GITHUB_REF_NAME so the logs are readable"
      git push origin "HEAD:refs/heads/$GITHUB_REF_NAME" >>"$log" 2>&1 || say "fallback push failed"
    fi
    return 1
  fi
  return 0
}

say "=== phase ${phase} $(date -u +%Y-%m-%dT%H:%M:%SZ) run ${run} attempt ${GITHUB_RUN_ATTEMPT:-0}"
say "head $(git rev-parse HEAD) branch ${GITHUB_REF_NAME:-local} repo ${GITHUB_REPOSITORY:-local}"
say "queue: $(ls publish/jobs/*.txt 2>/dev/null | tr '\n' ' ')"

case "$phase" in
  probe)
    say "gh: $(command -v gh || echo missing) $(gh --version 2>&1 | head -1)"
    say "token chars: ${#GH_TOKEN}"
    cmd gh-auth gh auth status
    say "remote: $(git remote get-url origin 2>&1)"
    ;;
  download)
    rm -rf publish/raw
    mkdir -p publish/raw
    for job in publish/jobs/*.txt; do
      [ -e "$job" ] || continue
      artifact="$(queue artifact "$job")"
      run_id="$(queue run "$job")"
      if [ -z "$artifact" ] || [ -z "$run_id" ]; then
        say "$job: missing artifact= or run=; skipped"
        continue
      fi
      cmd "download-$artifact" gh run download "$run_id" --repo "${GITHUB_REPOSITORY:-}" \
        --name "$artifact" --dir "publish/raw/$artifact"
      find "publish/raw/$artifact" -type f -printf '%s\t%p\n' 2>/dev/null | sort -k2 | head -40 >>"$log"
      say "$artifact: $(find "publish/raw/$artifact" -type f 2>/dev/null | wc -l) files, $(du -sb "publish/raw/$artifact" 2>/dev/null | cut -f1) bytes"
    done
    ;;
  summarize)
    for job in publish/jobs/*.txt; do
      [ -e "$job" ] || continue
      mode="$(queue mode "$job")"
      artifact="$(queue artifact "$job")"
      run_id="$(queue run "$job")"
      dest="$(queue dest "$job")"
      note="$(queue note "$job")"
      raw="publish/raw/$artifact"
      if [ ! -d "$raw" ] || [ -z "$dest" ]; then
        say "$artifact: no downloaded artifact or no dest=; skipped"
        continue
      fi
      prov="recovered from artifact \`$artifact\` of CI run $run_id by publish-t0047 run $run"
      case "$mode" in
        miniwob)
          cmd "summary-$artifact" python3 bench/miniwob/t0047_summary.py "$raw" --dest "$dest" \
            --provenance "$prov" \
            --provenance "branch commit: $(git rev-parse HEAD)" \
            --provenance "note: ${note:-none}"
          ls -la "$dest" >>"$log" 2>&1
          ;;
        webarena)
          mkdir -p "$dest"
          for f in table.md runs.jsonl summary.json; do
            if [ -f "$raw/$f" ]; then cp "$raw/$f" "$dest/"; say "copied $f"; fi
          done
          while IFS= read -r f; do
            cp -n "$f" "$dest/" 2>/dev/null && say "copied nested $f"
          done < <(find "$raw" -type f \( -name table.md -o -name runs.jsonl -o -name summary.json \))
          {
            echo "$prov"
            echo "branch commit: $(git rev-parse HEAD)"
            echo "note: ${note:-none}"
          } >"$dest/PROVENANCE.txt"
          ls -la "$dest" >>"$log" 2>&1
          ;;
        *)
          say "$artifact: unknown mode '$mode'; nothing summarized"
          ;;
      esac
    done
    ;;
  publish)
    say "dest dirs:"
    for job in publish/jobs/*.txt; do
      [ -e "$job" ] || continue
      d="$(queue dest "$job")"
      [ -n "$d" ] && find "$d" -type f -printf '%s\t%p\n' 2>/dev/null | sort -k2 >>"$log"
    done
    ;;
  *)
    say "unknown phase '$phase'"
    exit 2
    ;;
esac

dests=""
for job in publish/jobs/*.txt; do
  [ -e "$job" ] || continue
  d="$(queue dest "$job")"
  if [ -n "$d" ] && [ -d "$d" ] && [ -n "$(ls -A "$d" 2>/dev/null)" ]; then
    dests="$dests $d"
  fi
done
# shellcheck disable=SC2086
push_results $dests
