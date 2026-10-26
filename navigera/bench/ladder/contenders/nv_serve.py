#!/usr/bin/env python3
"""Contender: `navigera serve` (Chrome, no Node).

Drives one warm `navigera serve` process through the canonical session
over its JSON-lines stdin/stdout protocol, then reports exact per-process
CPU/RSS for the navigera binary itself via wait4 (the Python parent's
own overhead is excluded).

Modes:
  session   — full scripted session (goto, evals, fill, click, screenshot,
              second tab, quit)
  eval      — warm eval round-trip micro: goto page A, then 200x eval title
              (per-op latencies written to the stats file)
  cold      — not used here; cold start is measured by spawning the one-shot
              `navigera eval` directly from ladder.py

Env:
  NAVIGERA  path to the navigera binary (required)
  CHROME_BIN    chrome executable override (optional)
  LADDER_BASE   base URL of the fixture server, e.g. http://127.0.0.1:8123

Writes --stats-out JSON: {wall_s, cpu_s, maxrss_kb, counts, eval_ms:[...]}.
The extracted cards JSON goes to --extract-out; the screenshot to --shot-out.
"""
import argparse
import json
import os
import subprocess
import sys
import time

CLK_TCK = os.sysconf("SC_CLK_TCK")
PROFILE = os.environ.get("LADDER_PROFILE") == "1"


def proc_cpu_s(pid: int) -> float | None:
    """utime+stime in seconds for pid (all threads), or None."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            parts = f.read().rsplit(b")", 1)[1].split()
        return (int(parts[11]) + int(parts[12])) / CLK_TCK
    except (FileNotFoundError, ProcessLookupError, IndexError, ValueError):
        return None


def zombie_cpu_s(pid: int) -> tuple[float, float] | None:
    """(own utime+stime, reaped-children cutime+cstime) of an exited, not
    yet reaped pid; /proc/<pid>/stat of a zombie holds the final totals."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            parts = f.read().rsplit(b")", 1)[1].split()
        return ((int(parts[11]) + int(parts[12])) / CLK_TCK,
                (int(parts[13]) + int(parts[14])) / CLK_TCK)
    except (FileNotFoundError, ProcessLookupError, IndexError, ValueError):
        return None


def read_phases(stderr_path: str) -> dict:
    """The `NAVIGERA_TIMINGS {...}` line navigera prints at shutdown, if any."""
    try:
        with open(stderr_path, encoding="utf-8", errors="replace") as f:
            for line in f:
                if line.startswith("NAVIGERA_TIMINGS "):
                    return json.loads(line[len("NAVIGERA_TIMINGS "):])
    except (OSError, ValueError):
        pass
    return {}


def proc_rss_kb(pid: int) -> int | None:
    """Resident set size in KB for pid, or None."""
    try:
        with open(f"/proc/{pid}/statm", "rb") as f:
            parts = f.read().split()
        import os as _os
        return int(parts[1]) * _os.sysconf("SC_PAGE_SIZE") // 1024
    except (FileNotFoundError, ProcessLookupError, IndexError, ValueError):
        return None

EVAL_TITLE = "() => document.title"
EVAL_CARD_COUNT = "() => document.querySelectorAll('.card').length"
EVAL_EXTRACT = (
    "() => Array.from(document.querySelectorAll('.card')).map(c => ({"
    "t: c.querySelector('.t').textContent, "
    "p: c.querySelector('.p').textContent, "
    "v: c.dataset.vendor}))"
)
EVAL_VISIBLE = "() => document.querySelectorAll('.card:not(.hidden)').length"
EVAL_ROWS = "() => document.querySelectorAll('#rows tr').length"


class Driver:
    def __init__(self, navigera: str, chrome_bin: str | None, engine: str = "chrome"):
        argv = [navigera, "serve"]
        if engine and engine != "chrome":
            argv += ["--engine", engine]
        if chrome_bin:
            argv += ["--chromium", chrome_bin]
        # stderr -> file: serve logs there; stdout must stay protocol-clean.
        # (ladder.py captures a copy via --debug-log when needed.)
        self._stderr_file = open(f"/tmp/nv-serve-{os.getpid()}.stderr", "w")
        child_env = dict(os.environ)
        # Always request the in-process peak-RSS report: wait4's ru_maxrss
        # is misreported by the kernel when the child has spawned Chrome.
        child_env["NAVIGERA_RSS_REPORT"] = "1"
        # Phase timings (launch / first page / close) for the results.
        child_env["NAVIGERA_TIMINGS"] = "1"

        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr_file,
            text=True,
            bufsize=1,
            env=child_env,
        )
        self._next_id = 0
        self.prof: list[tuple[str, float, float]] = []  # (op, wall_ms, cpu_ms)
        self.rss_prof: list[tuple[str, int | None]] = []  # (op, rss_kb after op)
        # Background peak-RSS tracker (PROFILE only): samples every 20ms.
        self._rss_peak = 0
        self._rss_stop = False
        if PROFILE:
            import threading as _th
            pid = self.proc.pid
            def _sample():
                while not self._rss_stop:
                    v = proc_rss_kb(pid)
                    if v and v > self._rss_peak:
                        self._rss_peak = v
                    _th.Event().wait(0.02)
            self._rss_thread = _th.Thread(target=_sample, daemon=True)
            self._rss_thread.start()
        self._t_init = time.perf_counter()
        self._c_init = proc_cpu_s(self.proc.pid) if PROFILE else None
        self._launch_reported = False

    def cmd(self, op: dict) -> dict:
        self._next_id += 1
        op = {"id": self._next_id, **op}
        assert self.proc.stdin and self.proc.stdout
        t0 = time.perf_counter()
        c0 = proc_cpu_s(self.proc.pid) if PROFILE else None
        if PROFILE and not self._launch_reported:
            self._launch_reported = True
            c1 = c0
            if self._c_init is not None and c1 is not None:
                self.prof.append((
                    "launch",
                    (t0 - self._t_init) * 1000,
                    (c1 - self._c_init) * 1000,
                ))
        self.proc.stdin.write(json.dumps(op) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        wall_ms = (time.perf_counter() - t0) * 1000
        if PROFILE:
            c1 = proc_cpu_s(self.proc.pid)
            cpu_ms = (c1 - c0) * 1000 if c0 is not None and c1 is not None else float("nan")
            self.prof.append((op.get("op", "?"), wall_ms, cpu_ms))
        if not line:
            self._stderr_file.flush()
            with open(self._stderr_file.name) as f:
                stderr_tail = f.read()[-2000:]
            raise RuntimeError(f"navigera closed stdout on op {op}\nstderr: {stderr_tail}")
        resp = json.loads(line)
        if not resp.get("ok"):
            raise RuntimeError(f"op {op} failed: {resp.get('error')}")
        if PROFILE:
            self.rss_prof.append((op.get("op", "?"), proc_rss_kb(self.proc.pid)))
        return resp

    def finish(self) -> tuple[float, float, int]:
        """Close stdin (EOF ends serve), wait4 => (wall, cpu, maxrss_kb)."""
        assert self.proc.stdin
        # NOTE: don't stop the RSS sampler here; it keeps sampling through
        # the teardown phase until wait4 returns.
        # Diagnostic: read VmHWM from /proc just before wait4 to compare
        # with wait4's ru_maxrss.
        self._vmm_hwm = None
        if PROFILE:
            try:
                with open(f"/proc/{self.proc.pid}/status") as f:
                    for line in f:
                        if line.startswith("VmHWM:"):
                            self._vmm_hwm = int(line.split()[1])
                            break
            except Exception:
                pass
        c_pre = proc_cpu_s(self.proc.pid) if PROFILE else None
        self.proc.stdin.close()
        t0 = time.perf_counter()
        # Wait for exit WITHOUT reaping (WNOWAIT): the zombie's /proc stat
        # still separates navigera's own CPU from the CPU of the Chrome
        # it reaped. wait4's rusage lumps both together (RUSAGE_BOTH), which
        # is where the "27x more driver CPU than go-rod" came from.
        os.waitid(os.P_PID, self.proc.pid, os.WEXITED | os.WNOWAIT)
        zombie = zombie_cpu_s(self.proc.pid)
        _, status, ru = os.wait4(self.proc.pid, 0)
        self._rss_stop = True
        # Prefer navigera's self-reported peak (VmHWM) over wait4's
        # ru_maxrss: the kernel misattributes ~200MB to the child when it
        # has spawned Chrome (proven by /proc sampling: true peak is ~5MB).
        # Fall back to wait4 if the report is missing.
        try:
            with open(self._stderr_file.name) as f:
                stderr_text = f.read()
            import re as _re
            m = _re.search(r"peak RSS at \w+: Some\((\d+)\) KB", stderr_text)
            if m:
                true_peak = int(m.group(1))
                if PROFILE:
                    print(f"[profile] using in-process VmHWM {true_peak} KB "
                          f"instead of wait4 {ru.ru_maxrss} KB")
                # Build a fake rusage-like with the corrected maxrss.
                # We only need ru_maxrss downstream.
                class _Ru:
                    pass
                ru2 = _Ru()
                ru2.ru_utime = ru.ru_utime
                ru2.ru_stime = ru.ru_stime
                ru2.ru_maxrss = true_peak
                ru = ru2
        except Exception:
            pass
        wall = time.perf_counter() - t0
        code = os.waitstatus_to_exitcode(status)
        if code != 0:
            # A crashed/failed navigera is a failed run, never a datapoint.
            self._stderr_file.flush()
            with open(self._stderr_file.name) as f:
                stderr_tail = f.read()[-2000:]
            raise RuntimeError(f"navigera exited with {code}\nstderr: {stderr_tail}")
        total_cpu = ru.ru_utime + ru.ru_stime
        own_cpu, reaped_cpu = zombie if zombie else (total_cpu, float("nan"))
        self.cpu_reaped_s = reaped_cpu
        if PROFILE and c_pre is not None:
            self.prof.append(("shutdown", wall * 1000, (own_cpu - c_pre) * 1000))
            print(f"[profile] own cpu {own_cpu:.3f}s, reaped-children cpu "
                  f"{reaped_cpu:.3f}s, wait4 (RUSAGE_BOTH) {total_cpu:.3f}s")
        return wall, own_cpu, ru.ru_maxrss


def run_session(drv: Driver, base: str, extract_out: str, shot_out: str,
                one_tab: bool = False) -> dict:
    counts: dict = {}
    drv.cmd({"op": "goto", "url": f"{base}/page_a.html"})
    counts["title_a"] = drv.cmd({"op": "eval", "expression": EVAL_TITLE})["result"]
    counts["cards_a"] = drv.cmd({"op": "eval", "expression": EVAL_CARD_COUNT})["result"]
    cards = drv.cmd({"op": "eval", "expression": EVAL_EXTRACT})["result"]
    with open(extract_out, "w", encoding="utf-8") as f:
        json.dump(cards, f)
    drv.cmd({"op": "fill", "selector": "#q", "value": "widget"})
    drv.cmd({"op": "click", "selector": "#search"})
    counts["visible"] = drv.cmd({"op": "eval", "expression": EVAL_VISIBLE})["result"]
    shot = drv.cmd({"op": "screenshot", "path": shot_out})
    counts["shot_bytes"] = shot["result"]["bytes"]
    if one_tab:
        # Single-tab engines (Lightpanda): same pages, second one in place.
        drv.cmd({"op": "goto", "url": f"{base}/page_b.html"})
    else:
        drv.cmd({"op": "tab-new", "url": f"{base}/page_b.html"})
    counts["title_b"] = drv.cmd({"op": "eval", "expression": EVAL_TITLE})["result"]
    counts["rows_b"] = drv.cmd({"op": "eval", "expression": EVAL_ROWS})["result"]
    drv.cmd({"op": "quit"})
    return counts


def run_eval_micro(drv: Driver, base: str, n: int) -> tuple[list[float], list[float]]:
    """(client-side ms incl. the stdin/stdout hop, engine-side ms) per eval.

    Client-side is what a program driving `serve` sees; the engine-side
    `elapsed_ms` (µs resolution) is navigera's own CDP round trip and
    is the number comparable with in-process drivers like go-rod."""
    drv.cmd({"op": "goto", "url": f"{base}/page_a.html"})
    lat, engine = [], []
    for _ in range(n):
        t0 = time.perf_counter()
        resp = drv.cmd({"op": "eval", "expression": EVAL_TITLE})
        lat.append((time.perf_counter() - t0) * 1000.0)
        if isinstance(resp.get("elapsed_ms"), (int, float)):
            engine.append(float(resp["elapsed_ms"]))
    drv.cmd({"op": "quit"})
    return lat, engine


def run_realworld(drv: Driver) -> dict:
    """Real-world scenario: example.com — goto, title, h1, paragraph, quit.

    Exercises real DNS/TLS/HTTP against the public internet. Returns the
    extracted facts for correctness checking plus per-op timings.
    """
    ops: dict = {}
    t0 = time.perf_counter()
    drv.cmd({"op": "goto", "url": "https://example.com"})
    ops["goto_ms"] = (time.perf_counter() - t0) * 1000.0
    t0 = time.perf_counter()
    title = drv.cmd({"op": "eval", "expression": "() => document.title"})["result"]
    ops["title_ms"] = (time.perf_counter() - t0) * 1000.0
    h1 = drv.cmd({"op": "eval", "expression": "() => document.querySelector('h1')?.textContent ?? null"})["result"]
    para = drv.cmd({"op": "eval", "expression": "() => document.querySelector('p')?.textContent?.trim().slice(0, 80) ?? null"})["result"]
    drv.cmd({"op": "quit"})
    return {"title": title, "h1": h1, "para": para, "ops": ops}


def run_browse(drv: Driver) -> dict:
    """Real-world gentle browsing: GitHub awesome list + trending repos.

    Scenario 1 (awesome list): goto a long curated page, scroll in gentle
    steps, extract headings — exercises scroll + content extraction on a
    real, JS-rendered page.

    Scenario 2 (trending): goto GitHub trending, count repo cards, read the
    top-3 names, click through to the first repo — exercises navigation via
    click. All evals are null-safe.
    """
    facts: dict = {}
    ops: dict = {}

    # --- awesome list: long page, gentle scrolling ---
    t0 = time.perf_counter()
    drv.cmd({"op": "goto", "url": "https://github.com/sindresorhus/awesome"})
    ops["awesome_goto_ms"] = (time.perf_counter() - t0) * 1000.0
    facts["awesome_title"] = drv.cmd(
        {"op": "eval", "expression": "() => document.title ?? null"})["result"]
    facts["awesome_links"] = drv.cmd(
        {"op": "eval",
         "expression": "() => document.querySelectorAll('a').length"})["result"]
    scroll_ys = []
    for i in range(5):
        t0 = time.perf_counter()
        y = drv.cmd({"op": "eval", "expression":
                     "() => { window.scrollBy({ top: 800, behavior: 'instant' }); return window.scrollY; }"}
                    )["result"]
        scroll_ys.append(y)
        ops[f"awesome_scroll{i}_ms"] = (time.perf_counter() - t0) * 1000.0
        time.sleep(0.3)
    facts["awesome_scroll_ys"] = scroll_ys
    facts["awesome_headings"] = drv.cmd(
        {"op": "eval", "expression":
         "() => [...document.querySelectorAll('h2')].slice(0, 5)"
         ".map(h => h.textContent?.trim() ?? null)"})["result"]

    # --- trending: dynamic list, click-through ---
    t0 = time.perf_counter()
    drv.cmd({"op": "goto", "url": "https://github.com/trending"})
    ops["trending_goto_ms"] = (time.perf_counter() - t0) * 1000.0
    facts["trending_title"] = drv.cmd(
        {"op": "eval", "expression": "() => document.title ?? null"})["result"]
    facts["trending_repos"] = drv.cmd(
        {"op": "eval", "expression":
         "() => document.querySelectorAll('article.Box-row').length"})["result"]
    facts["trending_top3"] = drv.cmd(
        {"op": "eval", "expression":
         "() => [...document.querySelectorAll('article.Box-row h2 a')]"
         ".slice(0, 3).map(a => (a.textContent ?? '').replace(/\\s+/g, ''))"}
        )["result"]
    # click the first repo card link, poll until navigation leaves /trending
    t0 = time.perf_counter()
    drv.cmd({"op": "click", "selector": "article.Box-row h2 a"})
    new_url = None
    for _ in range(24):
        time.sleep(0.25)
        url_info = drv.cmd({"op": "url"})["result"]
        # url op returns {"tab":..,"tabs":..,"url":..} or a plain string
        new_url = url_info.get("url") if isinstance(url_info, dict) else url_info
        if new_url and "trending" not in new_url:
            break
    ops["trending_click_ms"] = (time.perf_counter() - t0) * 1000.0
    facts["trending_clicked_url"] = new_url
    facts["trending_clicked_title"] = drv.cmd(
        {"op": "eval", "expression": "() => document.title ?? null"})["result"]

    drv.cmd({"op": "quit"})
    return {"facts": facts, "ops": ops}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["session", "eval", "realworld", "browse"],
                    required=True)
    ap.add_argument("--stats-out", required=True)
    ap.add_argument("--extract-out", default="")
    ap.add_argument("--shot-out", default="")
    ap.add_argument("--n-eval", type=int, default=200)
    ap.add_argument("--navigera", default="",
                    help="navigera binary (default $NAVIGERA); the ladder's "
                         "A/B baseline contender passes a second build here")
    ap.add_argument("--engine", default="chrome",
                    help="browser engine: chrome, edge, brave, lightpanda")
    ap.add_argument("--chromium", default="",
                    help="override browser binary path (for edge/brave)")
    ap.add_argument("--transport", default="",
                    help="CDP transport: pipe (default) or ws (the nv-ws A/B contender)")
    args = ap.parse_args()
    if args.transport:
        os.environ["NAVIGERA_CDP_TRANSPORT"] = args.transport

    navigera = args.navigera or os.environ["NAVIGERA"]
    # CHROME_BIN is only a fallback for the chrome engine. Passing it to
    # `--engine lightpanda` made navigera launch `google-chrome serve ...`
    # as if it were lightpanda (its --chromium flag doubles as the lightpanda
    # binary override), which then never exposed /json/version.
    chrome_bin = args.chromium or (
        os.environ.get("CHROME_BIN") if args.engine == "chrome" else None) or None
    base = os.environ["LADDER_BASE"]

    t0 = time.perf_counter()
    print("PYTHON: starting", file=sys.stderr, flush=True)
    drv = Driver(navigera, chrome_bin, args.engine)
    print("PYTHON: driver created", file=sys.stderr, flush=True)
    run_error = None
    realworld: dict = {}
    eval_engine_ms: list[float] = []
    try:
        if args.mode == "session":
            counts = run_session(drv, base, args.extract_out, args.shot_out,
                                 one_tab=args.engine == "lightpanda")
            print("PYTHON: run_session completed", file=sys.stderr, flush=True)
            eval_ms: list[float] = []
        elif args.mode == "realworld":
            counts = {}
            eval_ms = []
            realworld = run_realworld(drv)
            print("PYTHON: run_realworld completed", file=sys.stderr, flush=True)
        elif args.mode == "browse":
            counts = {}
            eval_ms = []
            realworld = run_browse(drv)
            print("PYTHON: run_browse completed", file=sys.stderr, flush=True)
        else:
            counts = {}
            eval_ms, eval_engine_ms = run_eval_micro(drv, base, args.n_eval)
            print("PYTHON: run_eval_micro completed", file=sys.stderr, flush=True)
    except Exception as e:
        run_error = e
        print(f"PYTHON: run failed: {e}", file=sys.stderr, flush=True)
    finally:
        # finish() closes stdin; if an op raised, still reap the child.
        try:
            tail_wall, cpu, maxrss = drv.finish()
            print("PYTHON: finish completed", file=sys.stderr, flush=True)
        except Exception as fe:
            print(f"PYTHON: finish failed: {fe}", file=sys.stderr, flush=True)
            drv.proc.kill()
            if run_error:
                raise run_error
            raise
    if run_error:
        raise run_error
    wall = time.perf_counter() - t0
    stats = {
        "wall_s": wall,
        "cpu_s": cpu,
        "cpu_reaped_s": getattr(drv, "cpu_reaped_s", float("nan")),
        "maxrss_kb": maxrss,
        "counts": counts,
        "eval_ms": eval_ms,
        "eval_engine_ms": eval_engine_ms,
        "phases": read_phases(drv._stderr_file.name),
        "realworld": realworld,
    }
    with open(args.stats_out, "w", encoding="utf-8") as f:
        json.dump(stats, f)
    if PROFILE and drv.prof:
        # stdout is not parsed for nv-serve (harness reads the stats file),
        # so the table is safe here; ladder.py echoes it into ladder.log.
        print("[profile] per-op wall/cpu for navigera:")
        for op, w, c in drv.prof:
            print(f"[profile]   {op:12s} wall {w:8.2f} ms  cpu {c:8.2f} ms")
    if PROFILE and drv.rss_prof:
        print("[profile] per-op RSS KB for navigera:")
        for op, rss in drv.rss_prof:
            print(f"[profile]   {op:12s} rss {rss} KB")
    if PROFILE:
        import resource as _res
        _ru_children = _res.getrusage(_res.RUSAGE_CHILDREN)
        print(f"[profile] background peak RSS: {drv._rss_peak} KB")
        print(f"[profile] wait4 maxrss: {maxrss} KB")
        print(f"[profile] RUSAGE_CHILDREN maxrss: {_ru_children.ru_maxrss} KB")
        if drv._vmm_hwm is not None:
            print(f"[profile] /proc VmHWM just before wait4: {drv._vmm_hwm} KB")
        # navigera's own VmHWM report (NAVIGERA_RSS_REPORT=1 in env)
        try:
            with open(drv._stderr_file.name) as f:
                for line in f:
                    if "peak RSS at" in line:
                        print(f"[profile] {line.strip()}")
        except Exception:
            pass
    print(json.dumps({"wall_s": round(wall, 3), "cpu_s": round(cpu, 3)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
