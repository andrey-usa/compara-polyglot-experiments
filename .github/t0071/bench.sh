#!/usr/bin/env bash
# T-0071 container bench, run on a GitHub-hosted runner.
# Servers (shop + gauntlet) run on the host; both images run with
# --network host, so container localhost is host localhost.
# Every leg appends one RESULT line to bench.log; per-leg stdout goes to
# out/<label>/stdout.log. Failures are recorded, never fatal to later legs.
set -u
S="primavera/bench/suite40"
LOG="bench.log"
: > "$LOG"
mkdir -p out

now_ms() { date +%s%3N; }

# --- servers on the host -------------------------------------------------
python3 "$S/site/server.py" --port 18001 > site.out 2>&1 &
python3 "$S/gauntlet/server.py" --port 18201 --xo-port 18202 \
  --seed 7 --jitter 0 --telemetry 1 > gauntlet.out 2>&1 &
for _ in $(seq 150); do
  curl -sf -o /dev/null http://127.0.0.1:18001/ \
    && curl -sf -o /dev/null http://127.0.0.1:18202/xo-form && break
  sleep 0.2
done
curl -sf -o /dev/null http://127.0.0.1:18001/ \
  && curl -sf -o /dev/null http://127.0.0.1:18202/xo-form \
  && echo "SERVERS up" | tee -a "$LOG" \
  || echo "SERVERS FAILED TO COME UP" | tee -a "$LOG"

# --- helpers -------------------------------------------------------------
# mem_mib <docker-stats-MemUsage-first-field> -> integer MiB (floor)
mem_mib() {
  case "$1" in
    *GiB) awk -v v="${1%GiB}" 'BEGIN { printf "%d", v * 1024 }' ;;
    *MiB) awk -v v="${1%MiB}" 'BEGIN { printf "%d", v }' ;;
    *KiB) awk -v v="${1%KiB}" 'BEGIN { printf "%d", v / 1024 }' ;;
    *) echo 0 ;;
  esac
}

# run_pv <workers> <label> [extra docker args...]
# Detached run so peak RSS can be sampled; RESULT line has wall + peak + tally.
run_pv() {
  local workers="$1" label="$2"; shift 2
  local outdir="out/$label"; mkdir -p "$outdir"
  local start peak code wall tally
  start=$(now_ms); peak=0
  docker run -d --name pvbench --network host "$@" \
    -e GAUNTLET_URL=http://127.0.0.1:18201 \
    -v "$PWD/$S/pv:/scenarios:ro" -v "$PWD/$outdir:/out" -w /out \
    primavera:local run /scenarios \
    --base-url http://127.0.0.1:18001 --workers "$workers" \
    --junit junit.xml > "$outdir/container.id" 2>&1
  while [ "$(docker inspect -f '{{.State.Running}}' pvbench 2>/dev/null)" = "true" ]; do
    local m raw
    raw=$(docker stats --no-stream --format '{{.MemUsage}}' pvbench 2>/dev/null | awk '{print $1}')
    m=$(mem_mib "${raw:-0}")
    [ "$m" -gt "$peak" ] && peak="$m"
    sleep 1
  done
  docker wait pvbench > "$outdir/exit.code" 2>&1
  docker logs pvbench > "$outdir/stdout.log" 2>&1
  docker rm pvbench > /dev/null 2>&1
  code=$(cat "$outdir/exit.code" 2>/dev/null || echo "?")
  wall=$(( ($(now_ms) - start) / 1000 ))
  tally=$(grep -E "^[0-9]+ passed" "$outdir/stdout.log" | tail -1)
  echo "RESULT tool=primavera workers=$workers label=$label exit=$code wall_s=$wall peak_mib=$peak tally=${tally:-none}" | tee -a "$LOG"
}

# run_pw <workers> <label>
run_pw() {
  local workers="$1" label="$2"
  local outdir="out/$label"; mkdir -p "$outdir"
  local start peak code wall tally
  start=$(now_ms); peak=0
  docker run -d --name pwbench --network host \
    -e BASE_URL=http://127.0.0.1:18001 -e GAUNTLET_URL=http://127.0.0.1:18201 \
    -e PW_WORKERS="$workers" \
    pw:local sh -c '
      SHELL_BIN=$(ls /ms-playwright/chromium_headless_shell-*/chrome-headless-shell* 2>/dev/null | head -1);
      [ -n "$SHELL_BIN" ] || SHELL_BIN=$(ls /ms-playwright/chromium-*/chrome-linux/chrome 2>/dev/null | head -1);
      echo "CHROME_SHELL=$SHELL_BIN";
      CHROME_SHELL="$SHELL_BIN" npx playwright test --workers=$PW_WORKERS'
  > "$outdir/container.id" 2>&1
  while [ "$(docker inspect -f '{{.State.Running}}' pwbench 2>/dev/null)" = "true" ]; do
    local m raw
    raw=$(docker stats --no-stream --format '{{.MemUsage}}' pwbench 2>/dev/null | awk '{print $1}')
    m=$(mem_mib "${raw:-0}")
    [ "$m" -gt "$peak" ] && peak="$m"
    sleep 1
  done
  docker wait pwbench > "$outdir/exit.code" 2>&1
  docker logs pwbench > "$outdir/stdout.log" 2>&1
  docker rm pwbench > /dev/null 2>&1
  code=$(cat "$outdir/exit.code" 2>/dev/null || echo "?")
  wall=$(( ($(now_ms) - start) / 1000 ))
  tally=$(grep -E "[0-9]+ passed" "$outdir/stdout.log" | tail -1)
  echo "RESULT tool=playwright workers=$workers label=$label exit=$code wall_s=$wall peak_mib=$peak tally=${tally:-none}" | tee -a "$LOG"
}

# --- cold start: one scenario, docker start to exit ----------------------
cold() {
  local start end ms code
  start=$(now_ms)
  docker run --rm --network host \
    -e GAUNTLET_URL=http://127.0.0.1:18201 \
    -v "$PWD/$S/pv:/scenarios:ro" -v "$PWD/out/cold:/out" -w /out \
    primavera:local run /scenarios/s01-cookie-consent.pv \
    --base-url http://127.0.0.1:18001 > out/cold/stdout.log 2>&1
  code=$?
  end=$(now_ms); ms=$((end - start))
  echo "RESULT tool=primavera label=cold exit=$code wall_ms=$ms" | tee -a "$LOG"
}
mkdir -p out/cold

# --- 5 reps ---------------------------------------------------------------
for rep in 1 2 3 4 5; do
  echo "=== rep $rep ===" | tee -a "$LOG"
  cold
  run_pv 1 "rep$rep-pv-w1"
  run_pv 4 "rep$rep-pv-w4"
  run_pw 1 "rep$rep-pw-w1"
  run_pw 4 "rep$rep-pw-w4"
  # Kubernetes-example constraints, primavera leg only (the image under test).
  run_pv 4 "rep$rep-pv-constrained" \
    --read-only --tmpfs /tmp --user 12345 --cap-drop ALL
done

echo "BENCH DONE" | tee -a "$LOG"
