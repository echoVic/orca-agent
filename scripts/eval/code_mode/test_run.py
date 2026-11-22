"""Tests for run.py.

Run with: python3 -m unittest scripts/eval/code_mode/test_run.py
"""

import json
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


def row(variant, right=True, rounds=5, cost=0.5, exit=0, repetition=0, tools=None):
    if tools is None:
        tools = ["grep", "run_script"] if variant == "on" else ["grep"]
    return {"variant": variant, "right": right, "rounds": rounds, "cost_usd": cost, "exit": exit,
            "repetition": repetition, "tools": tools}


def both(off, on, repetitions=2):
    """One task run `repetitions` times each way."""
    return [row(variant, repetition=repetition, **overrides)
            for repetition in range(repetitions)
            for variant, overrides in (("off", off), ("on", on))]


class Compare(unittest.TestCase):
    def test_the_rule_holding_turns_it_on(self):
        summary = run.compare(both({}, {"rounds": 3, "cost": 0.3}))
        self.assertTrue(summary["decidable"])
        self.assertTrue(summary["used"])
        self.assertTrue(summary["beyond_noise"])
        self.assertTrue(summary["default_on"])
        self.assertEqual(summary["off"]["incomplete"], 0)
        self.assertEqual(summary["on"]["script_runs"], 2)
        self.assertEqual(summary["on"]["rounds_per_repetition"], [3, 3])
        self.assertEqual(set(summary), {"off", "on", "decidable", "used", "beyond_noise", "default_on"})

    def test_lower_totals_without_the_tool_being_called_keep_it_off(self):
        summary = run.compare(both({}, {"rounds": 3, "cost": 0.3, "tools": ["grep"]}))
        self.assertTrue(summary["decidable"])
        self.assertFalse(summary["used"])
        self.assertFalse(summary["default_on"])

    def test_the_tool_called_in_half_the_runs_is_not_most_of_them(self):
        rows = [row("off", repetition=0), row("on", rounds=3, cost=0.3, repetition=0),
                row("off", repetition=1), row("on", rounds=3, cost=0.3, repetition=1, tools=["grep"])]
        summary = run.compare(rows)
        self.assertEqual(summary["on"]["script_runs"], 1)
        self.assertFalse(summary["used"])
        self.assertFalse(summary["default_on"])

    def test_one_repetition_cannot_show_a_saving(self):
        summary = run.compare(both({}, {"rounds": 3, "cost": 0.3}, repetitions=1))
        self.assertTrue(summary["decidable"])
        self.assertFalse(summary["beyond_noise"])
        self.assertFalse(summary["default_on"])

    def test_a_saving_smaller_than_the_spread_between_repetitions_keeps_it_off(self):
        # Lower in total, but one repetition with the tool on took as many
        # rounds as one with it off.
        rows = [row("off", rounds=5, repetition=0), row("on", rounds=2, cost=0.2, repetition=0),
                row("off", rounds=7, repetition=1), row("on", rounds=5, cost=0.3, repetition=1)]
        summary = run.compare(rows)
        self.assertLess(summary["on"]["rounds"], summary["off"]["rounds"])
        self.assertFalse(summary["beyond_noise"])
        self.assertFalse(summary["default_on"])

    def test_a_cost_saving_smaller_than_the_spread_keeps_it_off(self):
        rows = [row("off", cost=0.5, repetition=0), row("on", rounds=3, cost=0.2, repetition=0),
                row("off", cost=0.9, repetition=1), row("on", rounds=3, cost=0.5, repetition=1)]
        summary = run.compare(rows)
        self.assertLess(summary["on"]["cost_usd"], summary["off"]["cost_usd"])
        self.assertFalse(summary["beyond_noise"])
        self.assertFalse(summary["default_on"])

    def test_repetitions_are_totalled_over_their_tasks(self):
        rows = [row(variant, rounds=rounds, cost=cost, repetition=repetition)
                for repetition in range(2)
                for variant, rounds, cost in (("off", 5, 0.5), ("on", 3, 0.3))
                for _task in range(3)]
        summary = run.compare(rows)
        self.assertEqual(summary["off"]["rounds_per_repetition"], [15, 15])
        self.assertEqual(summary["on"]["cost_usd_per_repetition"], [0.9, 0.9])
        self.assertTrue(summary["default_on"])

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
        self.assertEqual(summary["on"]["incomplete"], 2)
        self.assertLess(summary["on"]["rounds"], summary["off"]["rounds"])
        self.assertFalse(summary["decidable"])
        self.assertFalse(summary["default_on"])

    def test_each_kind_of_incomplete_run_counts(self):
        for bad in ({"exit": 1}, {"cost": None}, {"rounds": 0}):
            with self.subTest(bad=bad):
                summary = run.compare(both({}, {"rounds": 3, "cost": 0.3, **bad}))
                self.assertEqual(summary["on"]["incomplete"], 2)
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


class Summarize(unittest.TestCase):
    def test_a_scripts_calls_are_its_summary_lines_not_its_preview_lines(self):
        chunks = [
            '  1   grep {"pattern":"TODO"} · ok · 12ms · 2.1 KiB\n',
            "          src/a.rs:3: // a · b · ok · 5ms\n",
            "          … 40 more lines\n",
            '  2 ┬ read_file {"path":"a.rs"} · ok · 3ms · 1.2 KiB\n',
            '  3 └ read_file {"path":"c.rs"} · failed · 1ms\n',
            "          c.rs does not exist\n",
            "3 tool calls · 412ms\n",
        ]
        jsonl = "\n".join(
            json.dumps({"type": "tool.output.delta", "payload": {"chunk": chunk}}) for chunk in chunks
        )

        self.assertEqual(run.summarize(jsonl)["script_tool_calls"], 3)

    def calls_in(self, *chunks, ensure_ascii=True):
        jsonl = "\n".join(
            json.dumps({"type": "tool.output.delta", "payload": {"chunk": chunk}}, ensure_ascii=ensure_ascii)
            for chunk in chunks
        )
        return run.summarize(jsonl)["script_tool_calls"]

    def test_a_preview_line_shaped_like_a_summary_line_is_not_counted(self):
        self.assertEqual(self.calls_in(
            '  1   read_file {"path":"a"} · ok · 2ms · 40 B\n',
            "            9   grep {} · ok · 1ms · 2 B\n",
            "          12 | foo bar · ok · 3ms\n",
            "1 tool call · 4ms\n",
        ), 1)

    def test_a_summary_line_with_a_line_separator_in_its_arguments_is_counted_once(self):
        # Orca writes JSON with U+2028 as it is, not as an escape.
        for ensure_ascii in (True, False):
            with self.subTest(ensure_ascii=ensure_ascii):
                self.assertEqual(self.calls_in(
                    '  1   grep {"pattern":"a b"} · ok · 1ms · 2 B\n',
                    "1 tool call · 2ms\n",
                    ensure_ascii=ensure_ascii,
                ), 1)


if __name__ == "__main__":
    unittest.main()
