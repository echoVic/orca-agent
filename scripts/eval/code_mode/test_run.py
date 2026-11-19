"""Tests for run.py.

Run with: python3 -m unittest scripts/eval/code_mode/test_run.py
"""

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run  # noqa: E402

try:
    import tomllib
except ImportError:  # Python before 3.11
    tomllib = None


def row(variant, right=True, rounds=5, cost=0.5, exit=0):
    return {"variant": variant, "right": right, "rounds": rounds, "cost_usd": cost, "exit": exit}


def both(off, on):
    return [row("off", **off), row("on", **on)]


class Compare(unittest.TestCase):
    def test_the_rule_holding_turns_it_on(self):
        summary = run.compare(both({}, {"rounds": 3, "cost": 0.3}))
        self.assertTrue(summary["decidable"])
        self.assertTrue(summary["default_on"])
        self.assertEqual(summary["off"]["incomplete"], 0)
        self.assertEqual(set(summary), {"off", "on", "decidable", "default_on"})

    def test_equal_right_is_allowed(self):
        self.assertTrue(run.compare(both({"right": True}, {"right": True, "rounds": 3, "cost": 0.3}))["default_on"])

    def test_fewer_right_answers_keeps_it_off(self):
        self.assertFalse(run.compare(both({"right": True}, {"right": False, "rounds": 3, "cost": 0.3}))["default_on"])

    def test_equal_rounds_is_not_lower(self):
        summary = run.compare(both({}, {"rounds": 5, "cost": 0.3}))
        self.assertTrue(summary["decidable"])
        self.assertFalse(summary["default_on"])

    def test_equal_cost_is_not_lower(self):
        summary = run.compare(both({}, {"rounds": 3, "cost": 0.5}))
        self.assertTrue(summary["decidable"])
        self.assertFalse(summary["default_on"])

    def test_a_timed_out_run_is_not_a_cheap_one(self):
        rows = both({}, {"rounds": 0, "cost": None, "exit": None, "right": True})
        summary = run.compare(rows)
        self.assertEqual(summary["on"]["incomplete"], 1)
        self.assertLess(summary["on"]["rounds"], summary["off"]["rounds"])
        self.assertFalse(summary["decidable"])
        self.assertFalse(summary["default_on"])

    def test_each_kind_of_incomplete_run_counts(self):
        for bad in ({"exit": 1}, {"cost": None}, {"rounds": 0}):
            with self.subTest(bad=bad):
                summary = run.compare(both({}, {"rounds": 3, "cost": 0.3, **bad}))
                self.assertEqual(summary["on"]["incomplete"], 1)
                self.assertFalse(summary["default_on"])

    def test_a_row_without_exit_is_incomplete(self):
        rows = both({}, {"rounds": 3, "cost": 0.3})
        del rows[1]["exit"]
        self.assertFalse(run.compare(rows)["decidable"])

    def test_unequal_run_counts_are_undecidable(self):
        rows = both({}, {"rounds": 3, "cost": 0.3}) + [row("on", rounds=1, cost=0.1)]
        summary = run.compare(rows)
        self.assertFalse(summary["decidable"])
        self.assertFalse(summary["default_on"])

    def test_no_runs_is_undecidable(self):
        summary = run.compare([])
        self.assertFalse(summary["decidable"])
        self.assertFalse(summary["default_on"])


class MakeHome(unittest.TestCase):
    def setUp(self):
        self.dirs = []

    def tearDown(self):
        for path in self.dirs:
            shutil.rmtree(path, ignore_errors=True)

    def home(self, config, scripts_on):
        source = Path(tempfile.mkdtemp(prefix="code-mode-source-"))
        self.dirs.append(source)
        (source / "config.toml").write_text(config, encoding="utf-8")
        (source / "auth.json").write_text("{}", encoding="utf-8")
        home = run.make_home(source, scripts_on)
        self.dirs.append(home)
        self.assertTrue((home / "auth.json").exists())
        return (home / "config.toml").read_text(encoding="utf-8")

    def test_off_and_on_write_the_setting_last(self):
        off = self.home("[budget]\nmax_tool_calls = 9\n", False)
        on = self.home("[budget]\nmax_tool_calls = 9", True)
        self.assertTrue(off.rstrip().endswith("[tools.script]\nenabled = false"))
        self.assertTrue(on.rstrip().endswith("[tools.script]\nenabled = true"))
        self.assertIn("max_tool_calls = 9", on)

    def test_an_existing_script_table_is_replaced(self):
        source = ("[tools]\nmax_read_parallel = 4\n\n[tools.script]\nenabled = true\n"
                  "timeout_secs = 5\n\n[budget]\nmax_tool_calls = 9\n")
        for scripts_on in (False, True):
            with self.subTest(scripts_on=scripts_on):
                text = self.home(source, scripts_on)
                self.assertIn("max_read_parallel = 4", text)
                self.assertIn("max_tool_calls = 9", text)
                self.assertEqual(text.count("[tools.script]"), 1)
                self.assertIn(f"enabled = {'true' if scripts_on else 'false'}", text)
                self.assertNotIn("timeout_secs = 5", text)
                self.assertEqual(text.count("enabled"), 1)
                if tomllib:
                    parsed = tomllib.loads(text)
                    self.assertEqual(parsed["tools"]["script"], {"enabled": scripts_on})
                    self.assertEqual(parsed["tools"]["max_read_parallel"], 4)


if __name__ == "__main__":
    unittest.main()
