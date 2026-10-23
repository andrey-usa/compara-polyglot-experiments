#!/usr/bin/env python3
"""primavera against Playwright Test on the same scenarios, same runner,
same Chrome, rounds interleaved.

Measures, per tool: the whole suite's wall time (cold first round, then the
median), one scenario from a cold start, peak memory of the runner's own
processes (everything but the browser) and of everything, and whether every
scenario passed. Prints a Markdown table.
"""
import argparse
import os
import shutil
import statistics
import subprocess
import sys
import threading
import time


def children():
    """parent pid -> its children."""
    kids = {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/stat") as f:
                ppid = int(f.read().rsplit(")", 1)[1].split()[1])
            kids.setdefault(ppid, []).append(int(d))
        except (OSError, ValueError, IndexError):
            pass
    return kids


def rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def is_browser(pid):
    try:
        exe = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        # A process on its way out has no exe link any more: go by its
        # command line.
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                exe = f.read().split(b"\0")[0].decode(errors="replace")
        except OSError:
            return False
    return "chrome" in exe or "headless_shell" in exe


def runner_and_browser(root):
    """(runner pids, browser pids) under root: a browser process and
    everything below it count as the browser, whatever their exe says."""
    kids = children()
    runner, browser = [], []
    todo = [(root, False)]
    while todo:
        p, under = todo.pop()
        here = under or is_browser(p)
        (browser if here else runner).append(p)
        todo.extend((k, here) for k in kids.get(p, []))
    return runner, browser


def run(cmd, cwd, env):
    """Wall ms, exit code, peak runner KB, peak total KB, output."""
    peak = {"runner": 0, "total": 0}
    started = time.perf_counter()
    proc = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    done = threading.Event()

    def sample():
        while not done.is_set():
            pids, browser = runner_and_browser(proc.pid)
            runner = sum(rss_kb(p) for p in pids)
            total = runner + sum(rss_kb(p) for p in browser)
            peak["runner"] = max(peak["runner"], runner)
            peak["total"] = max(peak["total"], total)
            done.wait(0.02)

    t = threading.Thread(target=sample, daemon=True)
    t.start()
    out, _ = proc.communicate()
    ms = (time.perf_counter() - started) * 1000
    done.set()
    t.join()
    return ms, proc.returncode, peak["runner"], peak["total"], out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--primavera", required=True)
    ap.add_argument("--scenarios", required=True, nargs="+", help="primavera scenario files or directories")
    ap.add_argument("--pw", required=True, help="Playwright Test project directory")
    ap.add_argument("--site", required=True)
    ap.add_argument("--rounds", type=int, default=15)
    ap.add_argument("--grep", default="empty cart")
    ap.add_argument("--chromium", help="the Chrome primavera runs (Playwright: channel chrome)")
    a = ap.parse_args()

    env = dict(os.environ, BASE_URL=a.site, CI="1")
    pv = os.path.abspath(a.primavera)
    scen = [os.path.abspath(x) for x in a.scenarios]
    npx = shutil.which("npx") or "npx"
    extra = ["--chromium", a.chromium] if a.chromium else []
    tools = {
        "primavera (1 worker)": ([pv, "run", *scen, "--base-url", a.site] + extra, None),
        "primavera (4 workers)": ([pv, "run", *scen, "--base-url", a.site, "--workers", "4"] + extra, None),
        "playwright test (1 worker)": ([npx, "playwright", "test", "--workers=1"], a.pw),
        "playwright test (4 workers)": ([npx, "playwright", "test", "--workers=4"], a.pw),
    }
    one = {
        "primavera (1 worker)": ([pv, "run", *scen, "--base-url", a.site, "--grep", a.grep] + extra, None),
        "playwright test (1 worker)": ([npx, "playwright", "test", "--workers=1", "-g", a.grep], a.pw),
    }
    suite = {k: [] for k in tools}
    single = {k: [] for k in one}
    mem = {k: [] for k in tools}
    failures = []
    for r in range(a.rounds):
        names = list(tools)
        if r % 2:
            names.reverse()
        for name in names:
            cmd, cwd = tools[name]
            ms, code, runner_kb, total_kb, out = run(cmd, cwd, env)
            if code != 0:
                failures.append((name, r, out[-2000:]))
            suite[name].append(ms)
            mem[name].append((runner_kb, total_kb))
        for name in (list(one) if r % 2 == 0 else list(reversed(list(one)))):
            cmd, cwd = one[name]
            ms, code, _, _, out = run(cmd, cwd, env)
            if code != 0:
                failures.append((name + " one scenario", r, out[-2000:]))
            single[name].append(ms)

    def med(xs):
        return statistics.median(xs) if xs else float("nan")

    base = med(suite["primavera (1 worker)"][1:])
    lines = [
        f"#### primavera vs Playwright Test: 7 scenarios, {a.rounds} interleaved rounds",
        "",
        "| | suite, first round (ms) | suite, median of the rest (ms) | vs primavera, 1 worker | one scenario, median (ms) | runner peak RSS (MB) | total peak RSS incl. Chrome (MB) |",
        "|---|---:|---:|---:|---:|---:|---:|",
    ]
    for name in tools:
        rest = med(suite[name][1:])
        one_ms = f"{med(single[name]):.0f}" if name in single else "–"
        runner = max(m[0] for m in mem[name]) / 1024
        total = max(m[1] for m in mem[name]) / 1024
        lines.append(
            f"| {name} | {suite[name][0]:.0f} | {rest:.0f} | {rest / base:.2f}x | {one_ms} | {runner:.1f} | {total:.1f} |"
        )
    lines.append("")
    if failures:
        lines.append(f"**{len(failures)} run(s) failed**: " + ", ".join(f"{n} (round {r})" for n, r, _ in failures[:10]))
        # Details first, so the table stays at the end (annotations keep the tail).
        details = [f"<details><summary>{n}, round {r}</summary>\n\n```\n{out[-1200:]}\n```\n</details>\n" for n, r, out in failures[:2]]
        lines = details + lines
    else:
        lines.append("Every run passed all scenarios.")
    print("\n".join(lines))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
