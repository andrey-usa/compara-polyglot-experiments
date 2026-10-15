#!/usr/bin/env bash
# Binary-searches the smallest memory limit a comparison finishes in.
#
# Peak RSS says what a process used with room to spare, which for an engine that
# maps its input says almost nothing: mapped pages are reclaimable, so the
# figure is whatever the kernel let it keep, not what it needed. This runs the
# comparison inside a memory cgroup and lowers the limit until it is killed.
#
#   scripts/memory_floor.sh cpp/build/csvdiff compare a.csv b.csv -k id
#
# Takes the whole command, so it works on any of the ports.
#
# Needs root and a memory cgroup. Both versions are handled, because the hosts
# that matter disagree: cgroup v1 (/sys/fs/cgroup/memory) is what a lot of
# older images give, and v2 -- one unified tree with memory.max -- is what
# Ubuntu 22.04 and later boot with, GitHub's runners among them. A script that
# only knew v1 would report "no controller" on exactly the host this is most
# worth running on.
#
# Each probe gets a fresh cgroup: reusing one silently fails to lower the limit
# once pages are charged to it, which reads as a pass when nothing was limited.
set -o pipefail
set +m          # the OOM kill is the measurement, not an incident to announce

if [ -d /sys/fs/cgroup/memory ] && [ -w /sys/fs/cgroup/memory ]; then
  CG_VERSION=1
  ROOT=/sys/fs/cgroup/memory
elif [ -f /sys/fs/cgroup/cgroup.controllers ]; then
  CG_VERSION=2
  ROOT=/sys/fs/cgroup
  # The memory controller has to be delegated to children before a child cgroup
  # can set memory.max. On a host where this shell is not the root of the tree
  # that write fails, and the probe says so rather than passing silently.
  grep -qw memory /sys/fs/cgroup/cgroup.controllers || {
    echo "cgroup v2 is mounted but the memory controller is not available"; exit 2; }
  echo "+memory" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
else
  echo "no usable memory cgroup: neither v1 at /sys/fs/cgroup/memory nor v2"; exit 2
fi
[ $# -ge 3 ] || { echo "usage: $0 COMMAND [args...]   (the full compare command)"; exit 2; }

HI=${CSVDIFF_FLOOR_HI:-8192}   # a limit that is expected to pass
LO=${CSVDIFF_FLOOR_LO:-64}     # a limit that is expected to fail

CMD=("$@")

probe() { # $1 = limit in MB; runs CMD inside a fresh cgroup at that limit
  local mb=$1
  local cg="$ROOT/floor_$$_$mb"
  local rc
  mkdir -p "$cg" 2>/dev/null || return 2
  if [ "$CG_VERSION" = 1 ]; then
    if ! echo $((mb * 1024 * 1024)) > "$cg/memory.limit_in_bytes" 2>/dev/null; then
      rmdir "$cg" 2>/dev/null
      return 2
    fi
    echo $((mb * 1024 * 1024)) > "$cg/memory.memsw.limit_in_bytes" 2>/dev/null
  else
    if ! echo $((mb * 1024 * 1024)) > "$cg/memory.max" 2>/dev/null; then
      rmdir "$cg" 2>/dev/null
      return 2
    fi
    # Without this the kernel swaps instead of killing, and the floor measured
    # is the floor of the swap file rather than of the engine.
    echo 0 > "$cg/memory.swap.max" 2>/dev/null
  fi
  ( echo $BASHPID > "$cg/cgroup.procs"; exec "${CMD[@]}" ) >/dev/null 2>&1
  rc=$?
  rmdir "$cg" 2>/dev/null
  # 0 identical, 1 differences: both mean it ran. 137 is the OOM kill.
  [ "$rc" = 0 ] || [ "$rc" = 1 ]
}

printf 'cgroup v%s, searching between %s and %s MB\n' "$CG_VERSION" "$LO" "$HI"
if ! probe "$HI"; then
  echo "did not finish even at ${HI} MB; raise CSVDIFF_FLOOR_HI"; exit 1
fi
while [ $((HI - LO)) -gt 32 ]; do
  MID=$(((HI + LO) / 2))
  if probe "$MID"; then
    printf '  %5s MB  completed\n' "$MID"; HI=$MID
  else
    printf '  %5s MB  killed\n' "$MID"; LO=$MID
  fi
done
printf '\nsmallest limit that finishes: about %s MB\n' "$HI"
[ -n "$CSVDIFF_FLOOR_JSON" ] && printf '{"floor_mb": %s, "cgroup": %s}\n' "$HI" "$CG_VERSION" \
    > "$CSVDIFF_FLOOR_JSON"
