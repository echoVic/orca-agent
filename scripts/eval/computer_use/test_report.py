import json
from pathlib import Path
import tempfile
import unittest

from report import aggregate, write_report
from run import select_tasks
from tasks import load_tasks


def row(tid, kind, passed, seconds):
    return {'id': tid, 'kind': kind, 'passed': passed, 'reason': 'completed',
            'metrics': {'wall_seconds': seconds, 'computer_calls': 3, 'tool_calls': 4,
                        'model_turns': 5, 'actions': {'observe': 3}, 'foreground_marks': 1,
                        'tool_errors': [], 'approvals_shown': 2, 'approvals': [],
                        'usage': {'input_tokens': 100, 'output_tokens': 20, 'cache_tokens': 40, 'estimated_cost_usd': .01},
                        'largest_observation_chars': 200}, 'cleanup': {'ok': True}, 'smoke_passed': True}


class ReportTests(unittest.TestCase):
    def test_window_panels_and_unapproved_touches_are_aggregated(self):
        first, second = row('S3', 'single', True, 2), row('M3', 'multi', True, 4)
        first['metrics'].update(user_window_touches=[24538], window_panels=2)
        a = aggregate([first, second], False)
        self.assertEqual((a['user_window_touches'], a['window_panels']), (1, 2))
        self.assertEqual(a['tasks']['S3']['user_window_touches'], [24538])
        self.assertEqual(a['tasks']['M3']['user_window_touches'], [])

    def test_aggregate_selected_tasks_and_dry_run_are_separate(self):
        rows = [row('S1', 'single', True, 2), row('M1', 'multi', False, 6)]
        a = aggregate(rows, False)
        self.assertEqual((a['single_success'], a['multi_outcome_success']), (1, 0))
        self.assertEqual(a['median_wall_s'], 4)
        self.assertEqual(a['primary_usage']['input_tokens'], 200)
        dry = aggregate(rows, True)
        self.assertIsNone(dry['single_success'])
        self.assertFalse(dry['valid_protocol_baseline'])
        self.assertEqual(dry['mock_smoke_passed'], 2)

    def test_comparison_accepts_original_baseline_and_warns_invalid(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            baseline = out / 'old.json'
            baseline.write_text(json.dumps({'valid_protocol_baseline': False, 'median_wall_s': 10, 'single_success': 8, 'single_total': 8}))
            write_report(out, [row('S1', 'single', True, 2)], {'dry_run': False}, baseline)
            text = (out / 'report.md').read_text()
            self.assertIn('Previous', text)
            self.assertIn('invalid', text)
            self.assertIn('median_wall_s', text)
            self.assertEqual(json.loads((out / 'aggregates.json').read_text())['single_success'], 1)

    def test_task_selection_rejects_unknown_duplicates_and_preserves_order(self):
        tasks = load_tasks()
        self.assertEqual([t['id'] for t in select_tasks(tasks, 'M2,S1')], ['S1', 'M2'])
        for value in ['S1,S1', 'S9', '']:
            with self.assertRaises(ValueError):
                select_tasks(tasks, value)
