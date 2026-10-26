#!/usr/bin/env python3
"""Pick the Chrome builds CI tests against, from Chrome for Testing.

Support policy (README "Supported browsers"): the current Stable milestone
and the three before it (~4 months of releases, covering Extended Stable),
plus chrome-headless-shell of the current Stable, plus Beta as an early
warning (non-blocking). Prints `matrix=<json>` for $GITHUB_OUTPUT.

Each entry: {label, milestone, version, channel, kind, url, exe}.
"""
import argparse
import json
import sys
import urllib.request

CFT = "https://googlechromelabs.github.io/chrome-for-testing"
PLATFORM = "linux64"


def get(name):
    with urllib.request.urlopen(f"{CFT}/{name}", timeout=30) as r:
        return json.load(r)


def download(entry, kind):
    for d in entry.get("downloads", {}).get(kind, []):
        if d["platform"] == PLATFORM:
            return d["url"]
    return None


EXE = {"chrome": "chrome-linux64/chrome", "chrome-headless-shell": "chrome-headless-shell-linux64/chrome-headless-shell"}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--back", type=int, default=3, help="stable milestones before the current one")
    ap.add_argument("--no-beta", action="store_true")
    args = ap.parse_args()

    channels = get("last-known-good-versions-with-downloads.json")["channels"]
    per_milestone = get("latest-versions-per-milestone-with-downloads.json")["milestones"]
    stable = channels["Stable"]
    stable_m = int(stable["version"].split(".")[0])

    matrix = []
    for m in range(stable_m, stable_m - args.back - 1, -1):
        entry = stable if m == stable_m else per_milestone.get(str(m))
        if not entry or not download(entry, "chrome"):
            continue
        matrix.append({"label": f"stable M{m}" if m == stable_m else f"M{m}", "milestone": m,
                       "version": entry["version"], "channel": "stable", "kind": "chrome",
                       "url": download(entry, "chrome"), "exe": EXE["chrome"]})
    if download(stable, "chrome-headless-shell"):
        matrix.append({"label": f"headless-shell M{stable_m}", "milestone": stable_m,
                       "version": stable["version"], "channel": "stable", "kind": "chrome-headless-shell",
                       "url": download(stable, "chrome-headless-shell"), "exe": EXE["chrome-headless-shell"]})
    beta = channels.get("Beta")
    if beta and not args.no_beta and download(beta, "chrome"):
        matrix.append({"label": f"beta M{beta['version'].split('.')[0]}", "milestone": int(beta["version"].split(".")[0]),
                       "version": beta["version"], "channel": "beta", "kind": "chrome",
                       "url": download(beta, "chrome"), "exe": EXE["chrome"]})
    print("matrix=" + json.dumps(matrix))
    for e in matrix:
        print(f"{e['label']}: {e['version']}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
