#!/usr/bin/env bash
# Save a browser's own CDP schema: protocol_dump.sh <browser binary> <out.json>
# Starts the browser headless on a free port, fetches /json/protocol, kills it.
# On failure prints the HTTP status seen and the browser's stderr tail.
set -uo pipefail
bin="$1"; out="$2"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
profile=$(mktemp -d)
log=$(mktemp)
"$bin" --headless=new --no-sandbox --disable-gpu --remote-debugging-port="$port" \
  --user-data-dir="$profile" about:blank >"$log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true; wait $pid 2>/dev/null || true; rm -rf "$profile" 2>/dev/null || true' EXIT
status=""
for _ in $(seq 300); do
  status=$(curl -s -o "$out" -w '%{http_code}' "http://127.0.0.1:$port/json/protocol" || true)
  if [ "$status" = "200" ]; then
    echo "protocol: $(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(len(d["domains"]), "domains")' "$out")"
    exit 0
  fi
  if ! kill -0 $pid 2>/dev/null; then break; fi
  sleep 0.1
done
echo "could not fetch /json/protocol from $bin (last HTTP status: ${status:-none}; browser alive: $(kill -0 $pid 2>/dev/null && echo yes || echo no))"
curl -s "http://127.0.0.1:$port/json/version" | head -c 600 || true
echo "--- browser stderr (tail) ---"
grep -v -E 'dbus|Fontconfig' "$log" | tail -c 3000
exit 1
