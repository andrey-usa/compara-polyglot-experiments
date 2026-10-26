#!/usr/bin/env python3
"""T-0047 MiniWoB cell: headline summary + slim per-episode table.

Reads the harness's ``miniwob-episodes-*.jsonl`` from an out dir (or an
explicit file), writes ``summary.md`` and ``episodes.tsv`` to ``--dest``.

Scoring is the T-0046 canonical rule, strict: solved = ``done`` and
``raw_reward == 1``.

Two behaviours the brief asks about are approximated from the records, with
the rule stated here so it can be challenged:

* ``used_eval``  - any model command in the episode matched ``\\beval\\b``
  (the model reaching for the DOM itself).
* ``used_clickable`` - a ``click N`` command used an N that appeared earlier
  in that same episode's tool output as ``clickable [ref=N]``.

With ``--baseline <jsonl>`` it also diffs task-level solves (0-3) against
that cell and lists the flipped tasks.
"""
import argparse
import glob
import json
import os
import re

CLICKABLE_RE = re.compile(r"clickable \[ref=(\d+)\]")
CLICK_RE = re.compile(r"\bclick (\d+)\b")
EVAL_RE = re.compile(r"\beval\b")


def load(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def find_episodes(out_dir, explicit):
    if explicit:
        return explicit
    hits = sorted(glob.glob(os.path.join(out_dir, "miniwob-episodes-*.jsonl")))
    if not hits:
        raise SystemExit(f"no episode records under {out_dir}")
    return hits[-1]


def analyse(rec):
    refs = set()
    used_clickable = False
    used_eval = False
    for command in rec.get("commands") or []:
        cmd = command.get("cmd") or ""
        if EVAL_RE.search(cmd):
            used_eval = True
        out = command.get("out") or ""
        refs.update(CLICKABLE_RE.findall(out))
        for ref in CLICK_RE.findall(cmd):
            if ref in refs:
                used_clickable = True
    return {
        "task": rec["task"],
        "seed": rec["seed"],
        "solved": 1 if (rec.get("done") and rec.get("raw_reward") == 1) else 0,
        "steps": rec.get("steps"),
        "tokens": rec.get("tokens", 0),
        "cost": rec.get("cost", 0.0),
        "wall_s": rec.get("wall_s"),
        "used_eval": int(used_eval),
        "used_clickable": int(used_clickable),
        "error": (rec.get("error") or "").split(":")[0],
    }


def per_task(rows):
    tasks = {}
    for row in rows:
        tasks.setdefault(row["task"], {})[row["seed"]] = row["solved"]
    return {task: sum(seeds.values()) for task, seeds in tasks.items()}


def totals(rows):
    return {
        "episodes": len(rows),
        "solved": sum(r["solved"] for r in rows),
        "cost": sum(r["cost"] for r in rows),
        "tokens": sum(r["tokens"] for r in rows),
        "eval": sum(r["used_eval"] for r in rows),
        "clickable": sum(r["used_clickable"] for r in rows),
        "errors": sum(1 for r in rows if r["error"]),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out_dir", nargs="?", default="out/miniwob")
    ap.add_argument("--episodes", help="explicit episodes jsonl (wins over out_dir)")
    ap.add_argument("--dest", default="bench/t0047-miniwob")
    ap.add_argument("--baseline", help="baseline episodes jsonl to diff task solves against")
    ap.add_argument("--provenance", action="append", default=[],
                    help="free-form 'key=value' provenance lines (run id, commits)")
    args = ap.parse_args()

    path = find_episodes(args.out_dir, args.episodes)
    rows = [analyse(rec) for rec in load(path)]
    if not rows:
        raise SystemExit(f"no episodes in {path}")
    t = totals(rows)
    tasks = per_task(rows)

    os.makedirs(args.dest, exist_ok=True)
    with open(os.path.join(args.dest, "episodes.tsv"), "w") as f:
        cols = ["task", "seed", "solved", "steps", "tokens", "cost", "wall_s",
                "used_eval", "used_clickable", "error"]
        f.write("\t".join(cols) + "\n")
        for row in rows:
            f.write("\t".join(str(row[c]) for c in cols) + "\n")

    lines = []
    lines.append("# T-0047 MiniWoB cell - navigera with clickable refs (strict scoring)")
    lines.append("")
    for item in args.provenance:
        lines.append(f"- {item}")
    lines.append(f"- episodes file: `{os.path.basename(path)}`")
    lines.append("- scoring: solved = `done` and `raw_reward == 1`")
    lines.append("")
    lines.append(f"**{t['solved']}/{t['episodes']} strict solves "
                 f"({100.0 * t['solved'] / t['episodes']:.1f}%)**; "
                 f"computed spend ${t['cost']:.6f}; {t['tokens']} tokens; "
                 f"{t['errors']} episodes with an error; "
                 f"episodes using `eval` to read the DOM: {t['eval']}; "
                 f"episodes clicking a `clickable` ref: {t['clickable']}.")
    lines.append("")

    if args.baseline:
        base_rows = [analyse(rec) for rec in load(args.baseline)]
        base_tasks = per_task(base_rows)
        base_t = totals(base_rows)
        lines.append("## Against the baseline cell")
        lines.append("")
        lines.append(f"- baseline: {base_t['solved']}/{base_t['episodes']} strict, "
                     f"${base_t['cost']:.6f}, eval episodes {base_t['eval']}, "
                     f"clickable-ref click episodes {base_t['clickable']}")
        lines.append(f"- this cell: {t['solved']}/{t['episodes']} strict, "
                     f"${t['cost']:.6f}, eval episodes {t['eval']}, "
                     f"clickable-ref click episodes {t['clickable']}")
        lines.append("")
        flips = []
        for task in sorted(set(tasks) | set(base_tasks)):
            now, before = tasks.get(task, 0), base_tasks.get(task, 0)
            if now == before:
                continue
            used = sorted({r["seed"] for r in rows if r["task"] == task and r["used_clickable"]})
            flips.append((task, before, now, now - before, used))
        lines.append(f"Tasks whose seed-solve count changed: {len(flips)} "
                     f"({sum(1 for f in flips if f[3] > 0)} up, "
                     f"{sum(1 for f in flips if f[3] < 0)} down)")
        lines.append("")
        lines.append("| task | before | after | delta | seeds using a clickable ref |")
        lines.append("|---|---|---|---|---|")
        for task, before, now, delta, used in sorted(flips, key=lambda x: (-abs(x[3]), x[0])):
            lines.append(f"| {task} | {before}/3 | {now}/3 | {delta:+d} | {used or '-'} |")
        lines.append("")

    lines.append("## Per task (seeds solved out of 3)")
    lines.append("")
    lines.append("| task | solved |")
    lines.append("|---|---|")
    for task in sorted(tasks):
        lines.append(f"| {task} | {tasks[task]}/3 |")
    lines.append("")

    out = os.path.join(args.dest, "summary.md")
    with open(out, "w") as f:
        f.write("\n".join(lines) + "\n")
    print("\n".join(lines[:6]))
    print(f"wrote {out} and {os.path.join(args.dest, 'episodes.tsv')}")


if __name__ == "__main__":
    main()
