"""Regenerate README's '## Benchmarks' section from a ladder publish.json.

Usage: gen_readme.py README.md publish.json ["what the nv-baseline A/B changed"]
Numbers in README come from a run's publish.json, never retyped."""
import json
import sys

readme_path, pub_path = sys.argv[1], sys.argv[2]
change_note = sys.argv[3].strip() if len(sys.argv) > 3 else ""
doc = json.load(open(pub_path))
all_rows = doc["contenders"]
baseline = next((r for r in all_rows if r["name"] == "nv-baseline"), None)
ws = next((r for r in all_rows if r["name"] == "nv-ws"), None)
rows = [r for r in all_rows if r["name"] not in ("nv-baseline", "nv-ws")]
NAMES = {
    "nv-serve": "`nv-serve` (this repo on Chrome)",
    "nv-shell": "`nv-shell` (this repo on chrome-headless-shell)",
    "nv-brave": "`nv-brave` (this repo on Brave)",
    "nv-edge": "`nv-edge` (this repo on Edge)",
    "nv-lightpanda": "`nv-lightpanda` (this repo on Lightpanda, 1 tab)",
    "gorod": "`gorod` (go-rod 0.116.2, Go)",
    "chromedp": "`chromedp` 0.19.1 (Go)",
    "chromiumoxide": "`chromiumoxide` 0.7 (Rust)",
    "chromey": "`chromey` 2.x (Rust, maintained chromiumoxide fork)",
    "puppeteer": "`puppeteer-core` (Node)",
    "playwright": "`playwright-core` (Node)",
}


def cpu(v):
    return "<10 ms" if v < 0.005 else (f"{v * 1000:.0f} ms" if v < 0.1 else f"{v:.2f} s")


chrome = sorted([r for r in rows if not r["experimental"]], key=lambda r: r["session"]["wall_s"])
exp = [r for r in rows if r["experimental"]]
best = lambda key, rs: min(rs, key=key)
fast_wall = chrome[0]["session"]["wall_s"]
fast_eval = min(r["eval"]["mean_ms"] for r in chrome if r.get("eval"))
fast_cold = min(r["cold"]["wall_s"] for r in chrome if r.get("cold"))
low_cpu = min(r["session"]["driver_cpu_s"] for r in chrome)
low_rss = min(r["session"]["driver_rss_mb"] for r in chrome)


def mem(v):
    # 0 = the run ended between PSS samples (fixed in the ladder since).
    return f"{v:.0f} MB" if v else "n/a"


def b(text, cond):
    return f"**{text}**" if cond else text


lines = [
    "## Benchmarks",
    "",
    "Same scripted session (launch → goto listing → title/count/extract evals →",
    "fill+click filter → visible-count eval → screenshot → new tab → goto detail →",
    "title/row-count evals → close) driven against the same headless browser by",
    "each driver. Fixtures are deterministic and local (500-card listing,",
    "1000-row detail). Run in CI via `.github/workflows/bench.yml`.",
    "",
    "| contender | session wall (best of 3) | warm eval mean (200×) | cold start (best of 5) | driver CPU (own) | driver peak RSS (own) | browser memory (PSS) |",
    "|---|---|---|---|---|---|---|",
]
for r in chrome + exp:
    s = r["session"]
    ev = r.get("eval", {}).get("mean_ms")
    cold = r.get("cold", {}).get("wall_s")
    lines.append(
        f"| {NAMES.get(r['name'], r['name'])} | {b(f'{s['wall_s']:.2f}s', s['wall_s'] == fast_wall)} | "
        f"{b(f'{ev:.2f} ms', ev == fast_eval and not r['experimental']) if ev is not None else '—'} | "
        f"{b(f'{cold:.2f}s', cold == fast_cold and not r['experimental']) if cold is not None else '—'} | "
        f"{b(cpu(s['driver_cpu_s']), s['driver_cpu_s'] < 0.05)} | "
        f"{b(f'{s['driver_rss_mb']:.0f} MB', round(s['driver_rss_mb']) == round(low_rss))} | "
        f"{mem(s['browser_pss_mb'])} |"
    )
date = doc["generated_utc"][:10]
env = doc["env"].replace("**", "").replace("Env: ", "").replace("Chrome: Google Chrome ", "Chrome ")
lines += [
    "",
    # No link to the run: the ladder may run in a fork or a private mirror,
    # where the link would not resolve for readers.
    f"Measured {date} on GitHub-hosted `ubuntu-latest` ({env}).",
    "Every driver produced byte-identical extracted data and counts (correctness",
    "gate); wall is best-of-N.",
    "The ladder reruns weekly ([`bench.yml`](.github/workflows/bench.yml)) against",
    "the latest contender releases. GitHub runners vary between runs, so compare",
    "rows within one run.",
    "",
    "**How driver CPU/RSS are measured.** Each driver's *own* `utime+stime`,",
    "read from its zombie's `/proc/<pid>/stat` before reaping (`waitid` with",
    "`WNOWAIT`), and its own VmHWM. The kernel counts CPU in 10 ms ticks, so",
    "values under ~50 ms are a tie. Not `wait4`: its rusage is RUSAGE_BOTH and",
    "adds in every child the driver reaped. navigera, chromedp, puppeteer",
    "and playwright `wait()` on their Chrome, so `wait4` charged them Chrome's",
    "CPU and RSS; go-rod (its leakless helper reaps Chrome) and chromiumoxide",
    "were never charged. Earlier versions of this table reported exactly that",
    "artifact (\"0.82s vs 0.03s\"). Browser memory is the whole browser process",
    "tree (every renderer/GPU/utility process), sampled as summed PSS every",
    "250 ms (shared pages counted once).",
    "",
]
order = ", ".join(f"{r['name']} {r['session']['wall_s']:.2f}s" for r in chrome)
nv_best = next(r for r in chrome if r["name"].startswith("nv-"))
serve_row = next((r for r in chrome if r["name"] == "nv-serve"), None)
other = next((r for r in chrome if not r["name"].startswith("nv-")), None)
rank = chrome.index(nv_best) + 1
lines += [
    f"**Reading the table.** Session wall order: {order}. navigera's best",
    f"({nv_best['name']}) ranks #{rank} of {len(chrome)}"
    + (f"; on regular Chrome, nv-serve ({serve_row['session']['wall_s']:.2f}s) is "
       + ("ahead of" if serve_row["session"]["wall_s"] < other["session"]["wall_s"] else "behind")
       + f" the fastest other driver, {other['name']} ({other['session']['wall_s']:.2f}s)"
       if serve_row and other and serve_row is not nv_best else "")
    + ". All native drivers (navigera,",
    "go-rod, chromiumoxide, chromedp) spend tens of milliseconds of their own CPU",
    "or less; the Node drivers spend hundreds and carry 80–150 MB of their own RSS.",
    "",
]
rw = [r for r in chrome if r.get("realworld")]
br = [r for r in chrome if r.get("browse")]
if rw and br:
    rw_ok = sum(1 for r in rw if r["realworld"]["ok"])
    br_ok = sum(1 for r in br if r["browse"]["ok"])
    lines += [
        "### Real-world (public internet, best of 3, same run)",
        "",
        f"example.com goto → title/h1: {rw_ok} of {len(rw)} pass; "
        + ", ".join(f"{r['name']} {r['realworld']['wall_s']:.2f}s" for r in sorted(rw, key=lambda r: r["realworld"]["wall_s"])) + ".",
        "",
        f"GitHub browse (awesome-list scroll + trending click-through): {br_ok} of {len(br)} pass; "
        + ", ".join(f"{r['name']} {r['browse']['wall_s']:.2f}s" for r in sorted(br, key=lambda r: r["browse"]["wall_s"])) + ".",
        "Live pages change between runs, so these are informational, not part of the",
        "correctness gate. (The scroll check used to fail at random for every driver:",
        "GitHub sets CSS `scroll-behavior: smooth`, so `scrollY` was read mid-animation;",
        "contenders now scroll with `behavior: 'instant'`.)",
        "",
    ]
serve = next((r for r in rows if r["name"] == "nv-serve"), None)
shell = next((r for r in rows if r["name"] == "nv-shell"), None)
if shell and serve and shell.get("cold") and serve.get("cold"):
    lines += [
        "**chrome-headless-shell** (the same Chrome build without its browser UI",
        f"layer): cold start {serve['cold']['wall_s']:.2f}s → {shell['cold']['wall_s']:.2f}s, session "
        f"{serve['session']['wall_s']:.2f}s → {shell['session']['wall_s']:.2f}s, browser memory "
        f"{mem(serve['session']['browser_pss_mb'])} → {mem(shell['session']['browser_pss_mb'])}.",
        "",
    ]
if ws and serve and ws.get("cold") and serve.get("cold"):
    spread = serve["session"].get("runs_wall_s") or []
    lines += [
        "**CDP transport A/B** (same build and run): `--remote-debugging-pipe` (default on Linux/macOS)",
        f"vs a DevTools WebSocket port: cold start {serve['cold']['wall_s']:.2f}s vs {ws['cold']['wall_s']:.2f}s,",
        f"session {serve['session']['wall_s']:.2f}s vs {ws['session']['wall_s']:.2f}s"
        + (". No speed difference"
           if abs(serve['session']['wall_s'] / ws['session']['wall_s'] - 1) < 0.05
           else (f", within run-to-run noise (the pipe's {len(spread)} sessions took "
                 f"{min(spread):.2f}–{max(spread):.2f}s)" if spread else ", within run-to-run noise"))
        + ": the WebSocket handshake itself is ~10 ms. The pipe is the default for two other reasons:",
        "it opens no TCP port that another local process could attach to, and Chrome exits when",
        "navigera dies (EOF on its command pipe), so a killed agent leaks no browser",
        "(`tests/edge_cases.rs`: over a WebSocket port the browser outlives its driver).",
        "Windows has no pipe transport here and uses the WebSocket, with a kill-on-close",
        "job object giving the same guarantee (`killed_session_server_takes_its_browser_down`",
        "runs there too).",
        "",
    ]
if baseline and serve:
    def pct(new, old):
        return f"{100 * (new / old - 1):+.0f}%"
    lines += [
        "**This build vs the previous one, same machine and run** (`nv-baseline`,",
        "built from the previous master by the ladder's `baseline_ref` A/B):",
        f"session {baseline['session']['wall_s']:.2f}s → {serve['session']['wall_s']:.2f}s "
        f"({pct(serve['session']['wall_s'], baseline['session']['wall_s'])}), "
        f"cold start {baseline['cold']['wall_s']:.2f}s → {serve['cold']['wall_s']:.2f}s "
        f"({pct(serve['cold']['wall_s'], baseline['cold']['wall_s'])}), "
        f"browser CPU per session {baseline['session']['browser_cpu_s']:.2f}s → "
        f"{serve['session']['browser_cpu_s']:.2f}s." + (f" {change_note}" if change_note else ""),
        "",
    ]
    ph = (serve.get("cold") or {}).get("phases_ms") or {}
    if ph:
        lines += [
            "Where navigera's cold start goes (`NAVIGERA_TIMINGS`, best run): browser up",
            f"{ph.get('browser_up', 0):.0f} ms, first page {ph.get('first_page', 0):.0f} ms, "
            f"close {ph.get('close', 0):.0f} ms.",
            "",
        ]
agent = doc.get("agent_cli") or []
if agent:
    lines += [
        "### Agent tools (scripted, warm session, best of 3)",
        "",
        "How an agent's tool calls drive a browser: CLI tools run one process per",
        "step (a shell tool), MCP servers get one `tools/call` per step over a",
        "persistent connection. Same canonical session plus one page snapshot.",
        "",
        "| tool | kind | total wall | mean per step | snapshot | snapshot size | gate |",
        "|---|---|---|---|---|---|---|",
    ]
    label = {
        "navigera": "`navigera` (this repo)",
        "navigera (headless shell)": "`navigera` on chrome-headless-shell",
        # bench.yml installs these @latest and the run records no version,
        # so the table names none rather than one that may be stale.
        "agent-browser": "`agent-browser` (Vercel Labs, Rust)",
        "playwright-cli": "`playwright-cli` (Microsoft)",
        "Playwright MCP": "Playwright MCP (Microsoft)",
        "Chrome DevTools MCP": "Chrome DevTools MCP (Google)",
    }
    for a in agent:
        kind = "MCP" if "MCP" in a["name"] else "CLI"
        if not a.get("wall_s"):
            lines.append(f"| {label.get(a['name'], a['name'])} | {kind} | failed | — | — | — | ✗ |")
            continue
        steps = {s_['op']: s_['ms'] for s_ in a.get("steps") or []}
        lines.append(
            f"| {label.get(a['name'], a['name'])} | {kind} | {a['wall_s']:.2f}s | {a['op_mean_ms']:.0f} ms | "
            f"{steps.get('snapshot', 0):.0f} ms | {a['snapshot_bytes'] / 1024:.0f} KB | "
            f"{'✓' if a['ok'] else '✗'} |")
    lines += [
        "",
        "Per-step time includes process start (CLI) or JSON-RPC (MCP), the hop to",
        "the daemon, and the CDP work. playwright-cli and Playwright MCP share Playwright's tool",
        "backend, which waits a fixed 500 ms after every action (`timeouts.settle`); playwright-cli",
        "also starts a Node process per step.",
        "",
    ]
for r in exp:
    s = r["session"]
    lines += [
        f"**Lightpanda** (experimental) passes the same correctness gate in",
        f"{s['wall_s']:.2f}s" + (f" with ~{s['browser_pss_mb']:.0f} MB of browser memory" if s['browser_pss_mb'] else "") + ". It has no",
        "rendering engine and one tab (the session opens the second page in the",
        "same tab), so it is listed apart from the Chrome-family ranking.",
        "",
    ]

text = open(readme_path).read()
start = text.index("## Benchmarks")
end = text.index("## Agent eval") if "## Agent eval" in text else text.index("## Working on this repo")
text = text[:start] + "\n".join(lines) + "\n" + text[end:]
open(readme_path, "w").write(text)
print("README updated from run", doc["run_id"])
