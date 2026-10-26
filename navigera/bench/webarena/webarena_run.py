#!/usr/bin/env python3
"""WebArena-Verified pilot: navigera vs agent-browser on neutral ground.

The tasks, sites and scoring are ServiceNow's WebArena-Verified (the
official 258-task Hard subset): third-party tasks neither tool was built
for, deterministic evaluators, no judge. The driver is the same
OpenAI-compatible shell loop as bench/agent-eval/openrouter_run.py — same
prompt shape (each tool's own skill, read first), same 40-turn cap, same
240s shell timeout, same 900s run timeout — pointed at the suite's
self-hosted sites instead of the in-house Acme site.

How a task runs here:
  1. The sites the task touches are recreated from their pristine Docker
     images (am1n3e/webarena-verified-*): the suite ships no data-reset
     operation (its env-ctrl `init` only applies configuration), so a
     fresh container per task is the reset, as in the suite's own
     harness. env-ctrl /init is still called to apply the canonical
     external URL.
  2. A recording reverse proxy (bench/webarena/harproxy.py) listens on
     each site's canonical port and forwards to the container on a
     shadow port, injecting the site's auto-login header (the optimized
     images' header authentication) and capturing every exchange into
     the network.har the suite's NetworkEventEvaluator reads.
  3. The agent drives the CLI tool against the canonical URLs, and ends
     with the suite's response JSON (task_type / status / retrieved_data).
  4. Scoring is the suite's own: webarena_verified's evaluate_task on
     the agent response + the captured HAR. No local checks.

Wikipedia (~100 GB data download) and Map (~60 GB) cannot run on
GitHub-hosted runners; pilot_tasks.json therefore stratifies the 233
Hard tasks confined to shopping / shopping_admin / reddit / gitlab.

  webarena_run.py --tool navigera --model deepseek/deepseek-v4.1-flash \
                  --offset 0 --count 48 --out out/webarena

Key: OPENROUTER_API_KEY in the environment (the repo's `main`
environment in CI). Docker must be available.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import types
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "agent-eval"))
import run as evalrun  # noqa: E402  the eval's workspace/skill setup
import openrouter_run as orun  # noqa: E402  the OpenAI-compatible agent loop

# site -> image, canonical port (what the browser and the suite's config
# see), shadow port (where the container is actually published), env-ctrl
# ports, the auto-login header the optimized image accepts, the prompt's
# authentication line (the suite's own per-site prompt wording).
SITES = {
    "shopping": {
        "image": "am1n3e/webarena-verified-shopping", "port": 7770, "upstream": 17770,
        "ctrl": 7771, "ctrl_upstream": 17771, "internal": 80,
        "external_url": "http://localhost:7770/",
        "header": ("X-M2-Customer-Auto-Login", "emma.lopez@gmail.com:Password.123"),
        "label": "Shopping — Magento storefront",
        "auth": "You are already logged in as emma.lopez@gmail.com. To re-authenticate, use credentials: "
                "emma.lopez@gmail.com / Password.123.",
        "credentials": {"username": "emma.lopez@gmail.com", "password": "Password.123"},
        "boot_timeout": 600,
    },
    "shopping_admin": {
        "image": "am1n3e/webarena-verified-shopping_admin", "port": 7780, "upstream": 17780,
        "ctrl": 7781, "ctrl_upstream": 17781, "internal": 80,
        "external_url": "http://localhost:7780/",
        "header": ("X-M2-Admin-Auto-Login", "admin:admin1234"),
        "label": "Shopping Admin — Magento admin panel",
        "auth": "You are already logged in as admin. To re-authenticate, use credentials: "
                "admin / admin1234.",
        "credentials": {"username": "admin", "password": "admin1234"},
        "boot_timeout": 600,
    },
    "reddit": {
        "image": "am1n3e/webarena-verified-reddit", "port": 9999, "upstream": 19999,
        "ctrl": 9998, "ctrl_upstream": 19998, "internal": 80,
        "external_url": "http://localhost:9999/",
        "header": ("X-Postmill-Auto-Login", "MarvelsGrantMan136:test1234"),
        "label": "Reddit — Postmill forum",
        "auth": "You are already logged in as MarvelsGrantMan136. To re-authenticate, use credentials: "
                "MarvelsGrantMan136 / test1234.",
        "credentials": {"username": "MarvelsGrantMan136", "password": "test1234"},
        "boot_timeout": 420,
    },
    "gitlab": {
        "image": "am1n3e/webarena-verified-gitlab", "port": 8023, "upstream": 18023,
        "ctrl": 8024, "ctrl_upstream": 18024, "internal": 8023,
        "external_url": "http://localhost:8023",
        "header": None,
        "label": "GitLab",
        "auth": "You are already logged in as byteblaze. To re-authenticate, use credentials: "
                "byteblaze / hello1234.",
        "credentials": {"username": "byteblaze", "password": "hello1234"},
        "boot_timeout": 1200,
    },
}
PLACEHOLDERS = {"__SHOPPING_ADMIN__": "shopping_admin", "__SHOPPING__": "shopping",
                "__REDDIT__": "reddit", "__GITLAB__": "gitlab",
                "__WIKIPEDIA__": "wikipedia", "__MAP__": "map"}

PROMPT = """You are in {workdir}. The command-line browser tool `{cmd}` is installed, and its usage guide is available to you as the skill "{skill}". Read that skill first, then use `{cmd}` (through shell commands) for every browser interaction. Do not drive a browser through other libraries or scripts, and do not fetch the websites with curl, wget or similar.

The task is on these websites:
{sites_block}
{auth_block}
Task: {task}

Tasks fall into one of three categories based on the main objective: RETRIEVE (retrieving data is the main objective), MUTATE (creating, updating, or deleting data/state is the main objective), NAVIGATE (navigating to show a specific page or search result is the main objective).

When you are done, end your reply with the result as a single JSON object in exactly this form:
{{"task_type": "RETRIEVE|MUTATE|NAVIGATE", "status": "SUCCESS|NOT_FOUND_ERROR|ACTION_NOT_ALLOWED_ERROR|PERMISSION_DENIED_ERROR|DATA_VALIDATION_ERROR|UNKNOWN_ERROR", "retrieved_data": <a list of the retrieved items for RETRIEVE tasks (a list even for a single item), otherwise null>, "error_details": null}}
Use an error status instead of SUCCESS, with a short error_details explanation, when the objective cannot be achieved for that reason (the target does not exist, the platform does not allow the action, you lack permission, or the required input is invalid)."""


def base(site: str) -> str:
    return f"http://localhost:{SITES[site]['port']}"


def resolve_url(url: str) -> str:
    for ph in sorted(PLACEHOLDERS, key=len, reverse=True):
        if url.startswith(ph):
            return base(PLACEHOLDERS[ph]) + url[len(ph):]
    return url


def sh(cmd: list, timeout: int = 120) -> tuple[int, str]:
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    return p.returncode, (p.stdout or "") + (p.stderr or "")


def http_status(url: str, timeout: int = 10) -> int | None:
    try:
        req = urllib.request.Request(url, method="GET")
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return None


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def http_status_no_redirect(url: str, timeout: int = 10) -> int | None:
    """Any HTTP response counts, redirects included: the Magento sites
    301 to their configured canonical base URL, so following redirects
    from the shadow port chases a port nothing listens on yet."""
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        with opener.open(url, timeout=timeout) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return None


def container_logs(site: str, tail: int = 40) -> str:
    rc, out = sh(["docker", "logs", "--tail", str(tail), f"wav-{site}"],
                 timeout=60)
    return out.strip().replace("\n", " | ")[-1200:]


def http_post(url: str, timeout: int = 60) -> tuple[int | None, str]:
    try:
        req = urllib.request.Request(url, data=b"", method="POST")
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")[:400]
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")[:400]
    except Exception as e:
        return None, str(e)


class SiteManager:
    """Docker lifecycle for the suite's site images, one container per
    site, recreated per task (the reset) and removed after its last use."""

    def __init__(self):
        self.running: set[str] = set()
        self.pulled: set[str] = set()

    def ensure_image(self, site: str):
        img = SITES[site]["image"]
        if site in self.pulled:
            return
        rc, out = sh(["docker", "image", "inspect", img], timeout=30)
        if rc != 0:
            print(f"  pulling {img} ...", flush=True)
            rc, out = sh(["docker", "pull", img], timeout=3600)
            if rc != 0:
                raise RuntimeError(f"docker pull {img} failed: {out[-300:]}")
        self.pulled.add(site)

    def recreate(self, site: str) -> float:
        """Fresh container from the pristine image; returns setup seconds."""
        cfg = SITES[site]
        name = f"wav-{site}"
        started = time.time()
        self.ensure_image(site)
        sh(["docker", "rm", "-f", name], timeout=120)
        cmd = ["docker", "run", "-d", "--name", name,
               "-p", f"{cfg['upstream']}:{cfg['internal']}",
               "-p", f"{cfg['ctrl_upstream']}:8877",
               "-e", f"WA_ENV_CTRL_EXTERNAL_SITE_URL={cfg['external_url']}"]
        if site == "gitlab":
            # The image is pre-configured with this external URL; a
            # reconfigure on init would only cost minutes.
            cmd += ["-e", "WA_ENV_CTRL_SKIP_RECONFIGURE=true"]
        cmd.append(cfg["image"])
        rc, out = sh(cmd, timeout=300)
        if rc != 0:
            raise RuntimeError(f"docker run {site} failed: {out[-300:]}")
        self.running.add(site)
        self._wait_ready(site)
        return time.time() - started

    def _wait_ready(self, site: str):
        cfg = SITES[site]
        deadline = time.time() + cfg["boot_timeout"]
        ctrl = f"http://127.0.0.1:{cfg['ctrl_upstream']}"
        while time.time() < deadline:
            if http_status(ctrl + "/status", timeout=5) == 200:
                break
            time.sleep(5)
        else:
            raise RuntimeError(f"{site}: env-ctrl not up after {cfg['boot_timeout']}s; "
                               f"container log tail: {container_logs(site)}")
        code, body = http_post(ctrl + "/init", timeout=900)
        if code != 200:
            # init applies the external-URL configuration; the images boot
            # pre-configured for the canonical URLs, so a refusal here is
            # a warning as long as the site itself serves.
            print(f"  warning: {site} env-ctrl /init -> {code}: {body[:160]}", flush=True)
        deadline = time.time() + cfg["boot_timeout"]
        while time.time() < deadline:
            if http_status_no_redirect(f"http://127.0.0.1:{cfg['upstream']}/", timeout=10) is not None:
                return
            time.sleep(5)
        raise RuntimeError(f"{site}: site did not answer after {cfg['boot_timeout']}s; "
                           f"container log tail: {container_logs(site)}")

    def drop(self, site: str):
        sh(["docker", "rm", "-f", f"wav-{site}"], timeout=120)
        self.running.discard(site)
        sh(["docker", "rmi", SITES[site]["image"]], timeout=300)
        self.pulled.discard(site)


def start_proxy(site: str, entries: Path) -> subprocess.Popen:
    cfg = SITES[site]
    cmd = [sys.executable, str(HERE / "harproxy.py"),
           "--listen", str(cfg["port"]), "--upstream", str(cfg["upstream"]),
           "--entries", str(entries)]
    if cfg["header"]:
        cmd += ["--header", f"{cfg['header'][0]}: {cfg['header'][1]}"]
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def assemble_har(taskdir: Path, sites: list) -> tuple[Path, int]:
    entries = []
    for site in sites:
        f = taskdir / f"trace-{site}.jsonl"
        if f.exists():
            entries += [json.loads(l) for l in f.read_text().splitlines() if l.strip()]
    entries.sort(key=lambda e: e.get("startedDateTime", ""))
    har = taskdir / "network.har"
    har.write_text(json.dumps({"log": {"version": "1.2",
                                        "creator": {"name": "harproxy", "version": "1.0"},
                                        "entries": entries}}))
    return har, len(entries)


def extract_response(text: str):
    """The agent's final response object: the last JSON dict in its text
    that carries a task_type. None when the agent never produced one."""
    dec = json.JSONDecoder()
    found = None
    for i, ch in enumerate(text):
        if ch != "{":
            continue
        try:
            obj, _ = dec.raw_decode(text[i:])
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and "task_type" in obj:
            found = obj
    return found


def make_evaluator():
    from webarena_verified.api import WebArenaVerified
    from webarena_verified.types.config import WebArenaVerifiedConfig
    envs = {}
    for ph, site in PLACEHOLDERS.items():
        if site not in SITES:
            continue
        env = {"urls": [base(site)], "credentials": dict(SITES[site]["credentials"])}
        if site == "shopping_admin":
            env["use_header_login"] = True
        envs[ph] = env
    return WebArenaVerified(config=WebArenaVerifiedConfig(environments=envs))


def site_name(x) -> str:
    return str(getattr(x, "value", x)).lower()


def render(tool: str, model: str, runs: list, planned: int) -> str:
    done = [r for r in runs if not r.get("not_run")]
    scored = [r for r in done if not r.get("infra")]
    passed = sum(1 for r in scored if r["ok"])
    cost = sum(r["cost_usd"] for r in done if r.get("cost_usd") is not None)
    lines = [f"### WebArena-Verified Hard pilot — {tool} × {model}", "",
             f"Passed {passed}/{len(scored)} scored tasks ({len(done) - len(scored)} infra, "
             f"{planned - len(done)} not run). Totals over finished runs: "
             f"{sum(r.get('requests') or 0 for r in done)} turns, "
             f"{sum(r.get('tokens_total') or 0 for r in done)} tokens, ${cost:.4f}, "
             f"{sum(r.get('wall_s') or 0 for r in done):.0f}s agent wall, "
             f"{sum(r.get('setup_s') or 0 for r in done):.0f}s site setup.", "",
             "| site | passed |", "|---|---|"]
    for site in SITES:
        rs = [r for r in scored if r["site"] == site]
        if rs:
            lines.append(f"| {site} | {sum(1 for r in rs if r['ok'])}/{len(rs)} |")
    fails = [r for r in done if not r["ok"]]
    if fails:
        lines += ["", "Failures:"]
        for r in fails:
            lines.append(f"- task {r['task_id']} ({r['site']}): "
                         + ("⚠ infra — " if r.get("infra") else "")
                         + "; ".join(r.get("why", []))[:300])
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", required=True, choices=["navigera", "agent-browser"])
    ap.add_argument("--model", required=True)
    ap.add_argument("--tasks-file", default=str(HERE / "pilot_tasks.json"))
    ap.add_argument("--offset", type=int, default=0)
    ap.add_argument("--count", type=int, default=48)
    ap.add_argument("--run-timeout", type=int, default=900)
    ap.add_argument("--max-spend", type=float, default=0.70,
                    help="stop starting tasks once this job's reported cost passes this many USD")
    ap.add_argument("--deadline-minutes", type=float, default=140,
                    help="stop starting tasks this long after the job began")
    ap.add_argument("--nv-bin-dir", default=str(evalrun.REPO / "target" / "release"))
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        print("OPENROUTER_API_KEY is not set", file=sys.stderr)
        return 1
    pilot = json.loads(Path(args.tasks_file).read_text())
    all_tasks = pilot["tasks"]
    tasks = all_tasks[args.offset:args.offset + args.count]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    wa = make_evaluator()
    probe = wa.get_task(tasks[0]["task_id"])
    print(f"evaluator ready; first task {tasks[0]['task_id']}: {probe.intent}", flush=True)

    # Last slice index at which each site is needed (image cleanup).
    last_use = {}
    for i, t in enumerate(tasks):
        for s in t["sites"]:
            last_use[s] = i
    sites = SiteManager()
    agent_args = types.SimpleNamespace(run_timeout=args.run_timeout)
    setup_args = types.SimpleNamespace(model="", decider_url=None, nv_bin_dir=args.nv_bin_dir)
    started = time.time()
    runs, spent = [], 0.0

    for i, entry in enumerate(tasks):
        tid = entry["task_id"]
        if spent >= args.max_spend:
            print(f"spend cap ${args.max_spend:.2f} reached (${spent:.4f}); stopping", flush=True)
            break
        if time.time() - started > args.deadline_minutes * 60:
            print(f"deadline {args.deadline_minutes} min reached; stopping", flush=True)
            break
        task = wa.get_task(tid)
        task_sites = [site_name(s) for s in task.sites]
        intent = task.intent
        start_urls = [resolve_url(u) for u in (task.start_urls or [])] or \
                     [base(task_sites[0])]
        rec = {"tool": args.tool, "model": args.model, "task_id": tid,
               "site": entry["primary_site"], "sites": task_sites, "intent": intent,
               "mode": "skilled", "agent": "openrouter", "os": evalrun.OS_NAME}
        taskdir = out / f"task-{tid}"
        taskdir.mkdir(exist_ok=True)
        proxies, root, setup_s = [], None, 0.0
        try:
            for s in task_sites:
                setup_s += sites.recreate(s)
            rec["setup_s"] = round(setup_s, 1)
            for s in task_sites:
                proxies.append(start_proxy(s, taskdir / f"trace-{s}.jsonl"))
            time.sleep(1)
            root = Path(tempfile.mkdtemp(prefix=f"eval-wa-{tid}-"))
            work, env = evalrun.prepare(args.tool, "skilled", root, setup_args)
            spec = evalrun.TOOLS[args.tool]
            sites_block = "\n".join(f"- {base(s)} — {SITES[s]['label']}" +
                                    (f" (start: {', '.join(u for u in start_urls if u.startswith(base(s)))})"
                                     if any(u.startswith(base(s)) for u in start_urls) else "")
                                    for s in task_sites)
            auth_block = "\n".join(f"- {SITES[s]['label']}: {SITES[s]['auth']}" for s in task_sites) + "\n"
            prompt = PROMPT.format(workdir=work, cmd=spec["cmd"], skill=spec["skill"],
                                   sites_block=sites_block, auth_block=auth_block, task=intent)
            res = orun.run_agent(key, args.model, prompt, work, env, agent_args)
            for p in proxies:
                p.terminate()
            proxies = []
            har, n_entries = assemble_har(taskdir, task_sites)
            rec["trace_entries"] = n_entries
            response = extract_response(res["text"])
            if response is not None:
                (taskdir / "agent_response.json").write_text(json.dumps(response, indent=1))
                agent_response = response
            else:
                (taskdir / "agent_response.raw.txt").write_text(res["text"][-4000:])
                agent_response = res["text"]
            result = wa.evaluate_task(task_id=tid, agent_response=agent_response,
                                      network_trace=har)
            try:
                (taskdir / "eval_result.json").write_text(
                    json.dumps(result.model_dump(mode="json"), indent=1))
            except Exception:
                pass
            verdicts = [{"evaluator": getattr(v, "evaluator_name", "?"),
                         "status": str(getattr(v, "status", "")),
                         "score": getattr(v, "score", None),
                         "error": (getattr(v, "error_msg", None) or "")[:200]}
                        for v in (result.evaluators_results or [])]
            status = str(getattr(result.status, "value", result.status)).lower()
            score = result.score
            ok = status == "success" and score is not None and score >= 1.0
            why = [f"{v['evaluator']}: {v['status']} score={v['score']}"
                   + (f" — {v['error']}" if v["error"] else "")
                   for v in verdicts if v["status"] != "success"]
            if status == "error" and getattr(result, "error_msg", None):
                why.append(f"evaluator error: {result.error_msg}"[:240])
            infra = bool(res.get("errors")) and res.get("requests", 0) == 0
            if res.get("errors") and not infra:
                why += res["errors"]
            if res.get("stuck"):
                why.append(res["stuck"])
            rec.update(res)
            rec.pop("text", None)
            rec.update({"ok": ok, "score": score, "eval_status": status,
                        "verdicts": verdicts, "why": why, "infra": infra,
                        "agent_status": (response or {}).get("status"),
                        "shell_count": len(res["shell_commands"]),
                        "answer": json.dumps(response)[:300] if response else res["text"][-300:]})
            if res.get("cost_usd"):
                spent += res["cost_usd"]
        except Exception as e:  # site/proxy/evaluator problem: infra, keep going
            for p in proxies:
                p.terminate()
            rec.update({"ok": False, "infra": True, "why": [f"harness: {type(e).__name__}: {e}"],
                        "setup_s": round(setup_s, 1)})
        finally:
            if root:
                evalrun.cleanup(root)
        runs.append(rec)
        status = "PASS" if rec["ok"] else ("INFRA " if rec.get("infra") else "FAIL ") + \
            "; ".join(rec.get("why", []))[:220]
        print(f"[{i + 1}/{len(tasks)}] task {tid} ({rec['site']}): {status} "
              f"(turns={rec.get('requests')}, cost={rec.get('cost_usd')}, "
              f"wall={rec.get('wall_s')}s, setup={rec.get('setup_s')}s)", flush=True)
        (out / "runs.jsonl").open("a").write(json.dumps(rec) + "\n")
        for s in task_sites:
            if last_use.get(s) == i:
                sites.drop(s)

    table = render(args.tool, args.model, runs, len(tasks))
    (out / "table.md").write_text(table)
    (out / "summary.json").write_text(json.dumps(
        {"suite": "webarena-verified-hard", "tool": args.tool, "model": args.model,
         "offset": args.offset, "count": args.count, "runs": runs}, indent=1))
    summary_file = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_file:
        with open(summary_file, "a", encoding="utf-8") as f:
            f.write(table)
    print(table)
    return 0


if __name__ == "__main__":
    sys.exit(main())
