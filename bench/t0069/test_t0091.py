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


if __name__ == "__main__":
    unittest.main()
