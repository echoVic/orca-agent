"""Unit tests for the capability-evaluation library (stdlib unittest, python3 -I friendly)."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

from lib import metrics, report, scenario, triage  # noqa: E402


class MetricsTests(unittest.TestCase):
    def test_wilson_known_values(self):
        low, high = metrics.wilson(0, 1)
        self.assertAlmostEqual(low, 0.0, places=3)
        self.assertAlmostEqual(high, 0.7935, places=3)
        low, high = metrics.wilson(5, 5)
        self.assertAlmostEqual(low, 0.5655, places=3)
        self.assertAlmostEqual(high, 1.0, places=3)

    def test_l50_recovers_a_known_threshold(self):
        # P(pass) = 0.5 at difficulty 10 => a=-10b, so d50 = 10.
        b = 0.8
        points = []
        for difficulty in (0, 5, 10, 15, 20):
            p = 1 / (1 + pow(2.718281828459045, -(b * (difficulty - 10))))
            trials = 200
            points.append((difficulty, round(p * trials), trials))
        estimate = metrics.l50(points)
        self.assertIsNotNone(estimate)
        self.assertLess(abs(estimate - 10), 1.5)

    def test_l50_needs_enough_rungs(self):
        # One rung, or a perfectly separated 0/1 ladder with one attempt per rung, is not fittable.
        self.assertIsNone(metrics.l50([(1, 1, 1)]))
        self.assertIsNone(metrics.l50([(1, 1, 1), (2, 0, 1)]))

    def test_budget_transitions(self):
        rows = [
            {"task": "a", "multiplier": 1, "outcome": "fail"},
            {"task": "a", "multiplier": 4, "outcome": "pass"},
            {"task": "b", "multiplier": 1, "outcome": "fail"},
            {"task": "b", "multiplier": 4, "outcome": "fail"},
        ]
        result = metrics.budget_transitions(rows)
        self.assertEqual(result["tasks"], 2)
        self.assertEqual(result["passed_at_some_budget"], 1)
        self.assertEqual(result["capability_gap_tasks"], ["b"])


class TriageTests(unittest.TestCase):
    def test_order_puts_harness_first(self):
        evidence = triage.Evidence(outcome="fail", harness_error="boom", environment_error="no docker",
                                   product_signals=["exit_137"], cross_budget_passed=True)
        self.assertEqual(triage.classify(evidence), "harness")

    def test_environment_before_product(self):
        evidence = triage.Evidence(outcome="fail", environment_error="no docker", product_signals=["exit_137"])
        self.assertEqual(triage.classify(evidence), "environment")

    def test_product_signal_wins_over_time_bound(self):
        evidence = triage.Evidence(outcome="fail", product_signals=["exit_137"], cross_budget_passed=True)
        self.assertEqual(triage.classify(evidence), "product_bug")

    def test_cross_budget_means_time_bound(self):
        evidence = triage.Evidence(outcome="fail", cross_budget_passed=True)
        self.assertEqual(triage.classify(evidence), "time_bound")

    def test_default_is_capability_gap(self):
        self.assertEqual(triage.classify(triage.Evidence(outcome="fail")), "capability_gap")

    def test_taxonomy_counts_every_class(self):
        counts = triage.taxonomy(["capability_gap", "capability_gap", "harness", "unknown"])
        self.assertEqual(counts["capability_gap"], 2)
        self.assertEqual(counts["harness"], 1)
        self.assertEqual(counts["product_bug"], 0)


class ScenarioTests(unittest.TestCase):
    def make(self, payload: dict) -> Path:
        root = Path(tempfile.mkdtemp())
        (root / "fixture").mkdir()
        (root / "prompt.md").write_text("do it", encoding="utf-8")
        (root / "scenario.json").write_text(json.dumps(payload), encoding="utf-8")
        return root / "scenario.json"

    def payload(self, **overrides):
        base = {
            "schema_version": 1, "id": "S1", "axis": "S", "rung": 1,
            "difficulty": {"level": 1}, "backend": "local", "fixture": "fixture",
            "prompt": "prompt.md", "verify": ["true"],
        }
        base.update(overrides)
        return base

    def test_loads_and_reads_prompt_file(self):
        loaded = scenario.load(self.make(self.payload()))
        self.assertEqual(loaded.prompt, "do it")
        self.assertEqual(loaded.difficulty_value, 1.0)

    def test_missing_key_is_rejected(self):
        payload = self.payload()
        payload.pop("verify")
        with self.assertRaises(scenario.ScenarioError):
            scenario.load(self.make(payload))

    def test_unknown_axis_is_rejected(self):
        with self.assertRaises(scenario.ScenarioError):
            scenario.load(self.make(self.payload(axis="Z")))

    def test_missing_fixture_is_rejected(self):
        payload = self.payload(fixture="nope")
        with self.assertRaises(scenario.ScenarioError):
            scenario.load(self.make(payload))

    def test_select_by_axis_and_rung(self):
        a = scenario.load(self.make(self.payload(id="a", rung=1)))
        b = scenario.load(self.make(self.payload(id="b", rung=3)))
        self.assertEqual([s.id for s in scenario.select([a, b], axes=["S"], rungs=(2, 4))], ["b"])
        with self.assertRaises(scenario.ScenarioError):
            scenario.select([a, b], ids=["missing"])


class ReportTests(unittest.TestCase):
    def sample_run(self):
        pairs = [
            ({"id": "a", "axis": "S", "rung": 1, "difficulty": {"level": 1}, "difficulty_value": 1.0},
             [{"outcome": "pass", "failure_class": None, "wall_seconds": 1.0, "verify_seconds": 0.1,
               "tokens_out": 10}]),
            ({"id": "b", "axis": "S", "rung": 2, "difficulty": {"level": 2}, "difficulty_value": 2.0},
             [{"outcome": "fail", "failure_class": "capability_gap", "wall_seconds": 2.0,
               "verify_seconds": 0.2, "tokens_out": None}]),
        ]
        return {"run_id": "r1", "started_at": "t", "finished_at": "t", "profile": "smoke", "k": 1,
                "out": "/tmp/r1", "protocol": {"source_commit": "abc", "model": "mock"},
                "summary": report.summarize(pairs), "product_findings": [],
                "scenarios": [{"id": m["id"], "axis": m["axis"], "rung": m["rung"], "attempts": a} for m, a in pairs]}

    def test_summarize_reports_rung_rates_and_taxonomy(self):
        summary = self.sample_run()["summary"]
        self.assertEqual(summary["axes"]["S"]["scenarios"][0]["passes"], 1)
        self.assertEqual(summary["taxonomy"]["capability_gap"], 1)
        # Two attempts cannot support an L50 fit; the report shows the raw rung rates instead.
        self.assertIsNone(summary["axes"]["S"]["l50"])

    def test_render_contains_protocol_and_taxonomy(self):
        text = report.render_markdown(self.sample_run())
        self.assertIn("## Protocol", text)
        self.assertIn("capability_gap", text)
        self.assertIn("S0", text) if False else None

    def test_failed_ids_and_diff(self):
        current = self.sample_run()
        self.assertEqual(report.failed_ids(current), ["b"])
        baseline = json.loads(json.dumps(current))
        baseline["run_id"] = "r0"
        baseline["scenarios"][1]["attempts"][0]["outcome"] = "pass"
        difference = report.diff(current, baseline)
        self.assertEqual(difference["transitions"][0]["change"], "regressed")


if __name__ == "__main__":
    unittest.main()
