#!/usr/bin/env python3
"""T-0091 unit tests (no model calls, zero spend):
- every task has a seeded reference solution in both arms, files exist
- stall recovery: second re-prompt repeats the command contract + one worked example
- partial-solution salvage: materializes the model's last attempted write, never
  overwrites an existing solution file, leaves no-file cases untouched
"""
import importlib.util, os, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("harness", os.path.join(HERE, "harness-t0069_run.py"))
H = importlib.util.module_from_spec(spec)
spec.loader.exec_module(H)


class TestSelftestSeeds(unittest.TestCase):
    def test_every_task_seeded_both_arms(self):
        tasks = set(H.TASKS)
        self.assertEqual(set(H.PV_SELFTEST), tasks)
        self.assertEqual(set(H.PW_SELFTEST), tasks)

    def test_seed_files_exist(self):
        d = os.path.join(HERE, "selftest-solutions")
        for name in list(H.PV_SELFTEST.values()) + list(H.PW_SELFTEST.values()):
            self.assertTrue(os.path.getsize(os.path.join(d, name)) > 0, name)


class TestRepromptText(unittest.TestCase):
    def test_first_reprompt_is_generic(self):
        for arm in ("primavera", "playwright"):
            t = H.reprompt_text(arm, 1)
            self.assertIn("EXACTLY ONE shell command", t)
            self.assertNotIn("```", t)

    def test_second_reprompt_has_contract_and_example(self):
        t = H.reprompt_text("primavera", 2)
        self.assertIn("primavera run solution.pv --base-url $BASE_URL", t)
        self.assertIn("```sh", t)
        self.assertIn("cat > solution.pv", t)
        t = H.reprompt_text("playwright", 2)
        self.assertIn("npx playwright test solution.spec.ts", t)
        self.assertIn("```sh", t)
        self.assertIn("cat > solution.spec.ts", t)


class TestSalvage(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()

    def test_materializes_last_heredoc_write_to_separate_file(self):
        cmds = [
            {"turn": 3, "cmd": "cat > solution.pv <<'EOF'\nbase $BASE_URL\nold\nEOF"},
            {"turn": 5, "cmd": "cat > solution.pv <<'EOF'\nbase $BASE_URL\nnew\nEOF"},
        ]
        turn, spath = H.salvage_partial_solution("primavera", self.dir, cmds)
        self.assertEqual(turn, 5)
        # the scored solution path stays untouched (primary score keeps
        # executed-file semantics); the partial is a separate diagnostic file
        self.assertFalse(os.path.exists(os.path.join(self.dir, "solution.pv")))
        self.assertEqual(os.path.basename(spath), "salvaged-partial-solution.pv")
        with open(spath) as f:
            self.assertIn("new", f.read())

    def test_never_overwrites_existing_solution(self):
        with open(os.path.join(self.dir, "solution.spec.ts"), "w") as f:
            f.write("// agent file\n")
        cmds = [{"turn": 2, "cmd": "cat > solution.spec.ts <<'EOF'\nstale\nEOF"}]
        self.assertIsNone(H.salvage_partial_solution("playwright", self.dir, cmds))
        with open(os.path.join(self.dir, "solution.spec.ts")) as f:
            self.assertEqual(f.read(), "// agent file\n")

    def test_no_write_seen_leaves_missing(self):
        cmds = [{"turn": 1, "cmd": "ls -la"}, {"turn": 2, "cmd": None}]
        self.assertIsNone(H.salvage_partial_solution("primavera", self.dir, cmds))
        self.assertFalse(os.path.exists(os.path.join(self.dir, "solution.pv")))

    def test_unrelated_heredoc_not_salvaged(self):
        cmds = [{"turn": 1, "cmd": "cat > notes.txt <<'EOF'\nnot a test\nEOF"}]
        self.assertIsNone(H.salvage_partial_solution("primavera", self.dir, cmds))
        self.assertFalse(os.path.exists(os.path.join(self.dir, "solution.pv")))




# ---------------- T-0096: max_tokens env, empty-length re-prompt, fence guard,
# cell dry run (stubbed model; no tokens spent) ----------------

import json, subprocess


class TestMaxTokensEnv(unittest.TestCase):
    def _probe(self, env_extra):
        env = {k: v for k, v in os.environ.items() if k != "BENCH_MAX_TOKENS"}
        env.update(env_extra)
        code = ("import importlib.util; s=importlib.util.spec_from_file_location('h', r'%s');"
                "h=importlib.util.module_from_spec(s); s.loader.exec_module(h);"
                "print(h.MAX_TOKENS, f'{h.REQ_MAX_USD:.5f}')" % os.path.join(HERE, "harness-t0069_run.py"))
        r = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.strip()

    def test_default_is_round2_value(self):
        self.assertEqual(self._probe({}), "1024 0.15796")

    def test_env_override(self):
        # CTX_MAX*PRICE_IN/1e6 + 4096*PRICE_OUT/1e6 = 0.15735 + 0.0024576 = 0.1598076
        self.assertEqual(self._probe({"BENCH_MAX_TOKENS": "4096"}), "4096 0.15981")


class TestEmptyLengthReprompt(unittest.TestCase):
    def test_text_asks_short_command_and_chunks(self):
        t = H.reprompt_empty_length_text()
        self.assertIn("ONE short shell command", t)
        self.assertIn("small appended chunks", t)
        self.assertIn("empty", t.lower())

    def test_distinct_from_contract_reprompt(self):
        self.assertNotEqual(H.reprompt_empty_length_text(), H.reprompt_text("primavera", 1))
        self.assertNotEqual(H.reprompt_empty_length_text(), H.reprompt_text("playwright", 2))


class TestFenceMarkerFallback(unittest.TestCase):
    def test_truncated_fence_marker_line_not_executed(self):
        # the round-2 case: response cut right after the fence opener
        self.assertEqual(H.extract_command("```bash"), "")
        self.assertEqual(H.extract_command("```bash\n"), "")

    def test_truncated_block_inner_line_not_executed(self):
        # a bare heredoc opener would create an empty solution file if run
        self.assertEqual(H.extract_command("```bash\ncat > solution.pv <<'EOF'\npartial"), "")

    def test_empty_first_line_of_complete_block_not_executed(self):
        self.assertEqual(H.extract_command("```bash\n\n```"), "")

    def test_normal_commands_still_extract(self):
        self.assertEqual(H.extract_command("```bash\nls -la\n```"), "ls -la")
        self.assertEqual(H.extract_command("ls -la"), "ls -la")
        self.assertEqual(H.extract_command("# comment\nls -la"), "ls -la")


class TestCellDryRun(unittest.TestCase):
    """Dry run of the cell configuration with the model call stubbed: exercises the
    RESERVE-CEILING print, max_tokens recording, and the empty-length re-prompt
    selection. No network, no tokens spent."""

    def test_dry_run_4096(self):
        outdir = tempfile.mkdtemp(prefix="t0096-dry-")
        driver = os.path.join(outdir, "driver.py")
        with open(driver, "w") as f:
            f.write("""
import importlib.util, sys
spec = importlib.util.spec_from_file_location("harness", r"%s")
H = importlib.util.module_from_spec(spec); spec.loader.exec_module(H)
calls = {"n": 0}
def fake_chat(api_key, model, messages, ledger, max_spend, deadline_s=None):
    calls["n"] += 1
    if calls["n"] == 1:
        return "", {"total_tokens": 0}, 0.0, "length"   # empty, budget-exhausted
    assert any("small appended chunks" in m.get("content", "") for m in messages), \\
        "empty-length re-prompt not sent"
    return "echo TASK-DONE", {"total_tokens": 0}, 0.0, "stop"
H.chat = fake_chat
sys.argv = ["h", "--arm", "playwright", "--tasks", "W4", "--trials", "1",
            "--max-turns", "5", "--port", "19517", "--out", r"%s"]
H.main()
""" % (os.path.join(HERE, "harness-t0069_run.py"), outdir))
        env = {k: v for k, v in os.environ.items() if k != "BENCH_MAX_TOKENS"}
        env["BENCH_MAX_TOKENS"] = "4096"
        env["SILICONFLOW_API_KEY"] = "dry-run-stub-not-used"
        r = subprocess.run([sys.executable, driver], env=env, capture_output=True,
                           text=True, timeout=180, cwd=HERE)
        self.assertEqual(r.returncode, 0, r.stderr[-2000:])
        # reserve ceiling printed BEFORE any (stubbed) request, with the 4096 figure
        self.assertIn("RESERVE-CEILING tasks=1(W4) trials=1 max_turns=5 max_tokens=4096 "
                      "req_max_usd=0.15981 worst_case=$0.80", r.stdout)
        # episode records the cap and the re-prompt selection
        eps = [json.loads(l) for l in open(
            os.path.join(outdir, [f for f in os.listdir(outdir) if f.startswith("t0069-episodes-")][0]))]
        self.assertEqual(len(eps), 1)
        rec = eps[0]
        self.assertEqual(rec["max_tokens"], 4096)
        self.assertTrue(rec["done_declared"])
        first = rec["commands"][0]
        self.assertIsNone(first["cmd"])
        self.assertEqual(first["reply_kind"], "commandless")
        self.assertEqual(first["finish_reason"], "length")
        self.assertEqual(first["reprompt_sent"], "empty-length")


if __name__ == "__main__":
    unittest.main()
