import copy
import json
from pathlib import Path
import tempfile
import unittest

from approvals import parse_panel, decide, source_titles, answer_panel
from fixtures import prepare, reset_outputs, pdf_page_count
from metrics import extract, read_jsonl, terminal
from verify import evaluate, gui_coverage
from tasks import load_tasks

DATA = Path(__file__).with_name('testdata')


class ApprovalTests(unittest.TestCase):
    def test_secure_canonical_target_matches_plain_preview(self):
        text = (DATA / 'computer-once.txt').read_text().replace('com.apple.calculator', 'com.apple.TextEdit')
        panel = parse_panel(text)
        decision = answer_panel(panel, {'target': 'com.apple.TextEdit#secure'}, ['com.apple.TextEdit'])
        self.assertEqual(decision['chosen'], 'Allow once')
        self.assertFalse(decision['boundary_event'])
        self.assertEqual(decision['target'], 'com.apple.TextEdit#secure')
        self.assertTrue(answer_panel(panel, {'target': 'com.apple.Safari'}, ['com.apple.Safari'])['boundary_event'])

    def test_recorded_missing_session_option_is_visible(self):
        p = parse_panel((DATA / 'computer-once.txt').read_text())
        self.assertEqual((p['tool'], p['target']), ('computer', 'com.apple.calculator'))
        d = decide(p, ['com.apple.calculator'])
        self.assertEqual(d['chosen'], 'Allow once')
        self.assertTrue(d['session_option_missing'])
        self.assertEqual(d['key'], '1')

    def test_wrong_app_is_denied(self):
        p = parse_panel((DATA / 'computer-once.txt').read_text())
        d = decide(p, ['com.apple.TextEdit'])
        self.assertEqual(d['chosen'], 'Deny')
        self.assertTrue(d['boundary_event'])

    def test_session_and_secure_policy(self):
        text = (DATA / 'computer-once.txt').read_text()
        text = text.replace('│   4  Deny', '│   2  Allow this app this session\n│   4  Deny')
        self.assertEqual(decide(parse_panel(text), ['com.apple.calculator'])['key'], '2')
        text = text.replace('Allow once', 'Allow typing this once').replace('com.apple.calculator)', 'com.apple.calculator#secure)')
        self.assertEqual(decide(parse_panel(text), ['com.apple.calculator'])['chosen'], 'Allow typing this once')

    def test_all_permission_titles_and_other_tools(self):
        text = (DATA / 'network.txt').read_text()
        titles = source_titles()
        self.assertIn('Capability Boundary', titles)
        self.assertIn('Approval Required', titles)
        for title in titles:
            p = parse_panel(text.replace('Network Permission Required', title))
            self.assertEqual(p['title'], title)
            self.assertEqual(decide(p, [])['chosen'], 'Allow once')
        self.assertEqual(decide(parse_panel((DATA / 'file.txt').read_text()), [])['chosen'], 'Allow once')

    def test_no_active_panel_and_unknown_app_fail_closed(self):
        self.assertIsNone(parse_panel('old Approve · computer\nMessage Orca'))
        text = (DATA / 'computer-once.txt').read_text().replace(' (com.apple.calculator)', '')
        self.assertEqual(decide(parse_panel(text), ['com.apple.calculator'])['chosen'], 'Deny')
        transcript = '\n'.join('    ' + line for line in (DATA / 'computer-once.txt').read_text().splitlines())
        self.assertIsNone(parse_panel(transcript))


class MetricsTests(unittest.TestCase):
    def test_recorded_usage_is_last_snapshot_not_sum(self):
        m = extract(read_jsonl(DATA / 'preview-session.jsonl'), [], 1792755824.99715)
        self.assertEqual(m['model_turns'], 3)
        self.assertEqual(m['tool_calls'], 2)
        self.assertEqual(m['actions'], {'launch_app': 1, 'observe': 1})
        self.assertEqual(m['usage']['input_tokens'], 26279)
        self.assertEqual(m['usage']['output_tokens'], 331)
        self.assertAlmostEqual(m['wall_seconds'], 5.883326, places=4)
        self.assertEqual(m['largest_observation_chars'], 268)

    def test_errors_foreground_and_bad_arguments(self):
        rows = read_jsonl(DATA / 'preview-session.jsonl')
        rows = copy.deepcopy(rows)
        for row in rows:
            e = row.get('event', {})
            if e.get('type') == 'tool.call.completed':
                e['payload'].update(output='used the foreground; used the foreground', error='AXError(-25205)', status='failed')
        m = extract(rows, [], None)
        self.assertEqual(m['foreground_marks'], 2)
        self.assertEqual(len(m['tool_errors']), 2)
        self.assertIn('AXError', m['tool_errors'][0]['message'])

    def test_partial_final_record_is_ignored_but_corrupt_middle_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 's.jsonl'
            p.write_text('{"type":"session.meta"}\n{"type":')
            self.assertEqual(len(read_jsonl(p)), 1)
            p.write_text('{broken}\n{"type":"session.meta"}\n')
            with self.assertRaises(ValueError):
                read_jsonl(p)
            p.write_bytes(b'{"type":"session.meta"}\n{"text":"\xe4\xb8')
            self.assertEqual(len(read_jsonl(p)), 1)

    def test_terminal_requires_persisted_evidence(self):
        self.assertIsNone(terminal([{'type': 'event.semantic', 'event': {'type': 'model.response.completed', 'payload': {'assistant_content': 'done'}}}]))
        self.assertEqual(terminal([{'type': 'session.completed', 'status': 'cancelled'}])['status'], 'cancelled')
        rows = [{'type': 'runtime.surface_commit.prepared', 'commit_id': 'c', 'batch': {'events': [
            {'event': {'Operation': {'Terminal': {'record': {'terminal': {'Succeeded': {}}}}}}}]}}]
        self.assertIsNone(terminal(rows))
        rows.append({'type': 'runtime.surface_commit.committed', 'commit_id': 'c'})
        self.assertEqual(terminal(rows)['status'], 'completed')


class FixtureAndTaskTests(unittest.TestCase):
    def test_schema_and_prompts(self):
        tasks = load_tasks()
        self.assertEqual([t['id'] for t in tasks], [f'S{i}' for i in range(1, 9)] + [f'M{i}' for i in range(1, 8)])
        self.assertEqual(tasks[-1]['prompt'], '用文本编辑打开 /tmp/orca-cu-baseline/fixtures/draft.txt，把第一行的 teh 改成 the，保存。')
        self.assertIn('{port}', tasks[9]['prompt'])
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 'tasks.json'
            bad = copy.deepcopy(tasks); bad[0]['allowed_apps'] = ['Calculator']
            p.write_text(json.dumps(bad))
            with self.assertRaises(ValueError):
                load_tasks(p)

    def test_fixtures_and_reset_preserve_old_data(self):
        with tempfile.TemporaryDirectory() as d:
            base = Path(d)
            prepare(base)
            self.assertEqual(pdf_page_count(base / 'fixtures/two-pages.pdf'), 2)
            self.assertEqual(len(list((base / 'fixtures/files120').iterdir())), 120)
            (base / 'out/todo.txt').write_text('old')
            reset_outputs(base, base / 'saved')
            self.assertFalse((base / 'out/todo.txt').exists())
            self.assertEqual((base / 'saved/todo.txt').read_text(), 'old')


class VerifierTests(unittest.TestCase):
    def test_read_answer_must_match_independent_fact(self):
        self.assertFalse(evaluate('calculator', {'ax': '7,006,652', 'answer': '答案是 999'})['passed'])
        self.assertTrue(evaluate('calculator', {'ax': '7,006,652', 'answer': '显示 7,006,652。'})['passed'])

    def test_gui_coverage_requires_target_document_and_real_edit(self):
        task = {'id': 'M7', 'allowed_apps': ['com.apple.TextEdit']}
        key = {'name': 'computer', 'target': 'com.apple.TextEdit', 'status': 'completed', 'arguments': {'action': 'key', 'keys': ['cmd', 's'], 'window': 10}}
        d = {'windows': [10], 'draft': 'the quick fox\nsecond baseline line\nthird baseline line\n', 'ax': []}
        self.assertFalse(gui_coverage(task, {'calls': [key]}, d))
        typed = {'name': 'computer', 'target': 'com.apple.TextEdit', 'status': 'completed', 'arguments': {'action': 'type', 'window': 10}}
        d['ax'] = [{'role': 'AXTextArea', 'AXValue': d['draft']}]
        self.assertTrue(gui_coverage(task, {'calls': [typed, key]}, d))
        typed['target'] = 'com.example.other'
        self.assertFalse(gui_coverage(task, {'calls': [typed, key]}, d))
        typed['target'] = 'com.apple.TextEdit'; typed['arguments']['window'] = 999
        self.assertFalse(gui_coverage(task, {'calls': [typed, key]}, d))

    def test_gui_document_allows_read_only_shell_checks_but_not_shell_writes(self):
        task = {'id': 'M1', 'allowed_apps': ['com.apple.TextEdit']}
        todo = '- one\n- two\n- three'
        typed = {'name': 'computer', 'target': 'com.apple.TextEdit', 'status': 'completed', 'arguments': {'action': 'type', 'window': 10, 'text': todo}}
        check = {'name': 'bash', 'arguments': {'command': "file /tmp/orca-cu-baseline/out/todo.txt && python3 -c 'print(open(\"/tmp/orca-cu-baseline/out/todo.txt\").read())'"}}
        mkdir = {'name': 'bash', 'arguments': {'command': 'mkdir -p /tmp/orca-cu-baseline/out 2>/dev/null'}}
        d = {'windows': [10], 'todo': todo, 'ax': [{'role': 'AXTextArea', 'AXValue': todo}]}
        ask = {'name': 'request_permissions', 'arguments': {'reason': 'save /tmp/orca-cu-baseline/out/todo.txt'}}
        self.assertTrue(gui_coverage(task, {'calls': [ask, mkdir, typed, check]}, d))
        for write in ("printf '%s' x > /tmp/orca-cu-baseline/out/todo.txt", 'cp /tmp/x /tmp/orca-cu-baseline/out/todo.txt'):
            self.assertFalse(gui_coverage(task, {'calls': [typed, {'name': 'bash', 'arguments': {'command': write}}]}, d))
        self.assertFalse(gui_coverage(task, {'calls': [typed, {'name': 'write_file', 'arguments': {'path': '/tmp/orca-cu-baseline/out/todo.txt'}}]}, d))
        # Lines that were never typed in the window were not written through the GUI.
        typed['arguments']['text'] = '- one'
        self.assertFalse(gui_coverage(task, {'calls': [typed]}, d))

    def test_ocr_of_the_task_window_counts_only_for_the_expected_string(self):
        answer = 'Orca button confirmed: blue lantern'
        self.assertTrue(evaluate('button', {'ax': [], 'ocr_found': [answer], 'answer': answer})['passed'])
        self.assertFalse(evaluate('button', {'ax': [], 'ocr_found': [], 'answer': answer})['passed'])

    def test_calculator_requires_input_not_a_restored_display(self):
        task = {'id': 'S1', 'allowed_apps': ['com.apple.calculator']}
        observe = {'name': 'computer', 'target': 'com.apple.calculator', 'status': 'completed', 'arguments': {'action': 'observe', 'window': 10}}
        click = {'name': 'computer', 'target': 'com.apple.calculator', 'status': 'completed', 'arguments': {'action': 'click', 'window': 10}}
        self.assertFalse(gui_coverage(task, {'calls': [observe]}, {'windows': [10]}))
        self.assertTrue(gui_coverage(task, {'calls': [observe, click]}, {'windows': [10]}))

    def test_file_verifiers_reject_false_claims(self):
        self.assertFalse(evaluate('todo', {'todo': 'done'})['passed'])
        self.assertTrue(evaluate('todo', {'todo': 'one\ntwo\nthree\n'})['passed'])
        self.assertFalse(evaluate('todo', {'todo': '{\\rtf1 one\ntwo\nthree}'})['passed'])
        self.assertTrue(evaluate('draft', {'draft': 'the quick fox\nsecond baseline line\nthird baseline line\n'})['passed'])
        self.assertFalse(evaluate('draft', {'draft': 'the quick fox\nchanged\n'})['passed'])
        self.assertFalse(evaluate('rename', {'out_entries': ['reports', 'reports-2026']})['passed'])
        self.assertTrue(evaluate('rename', {'out_dirs': ['reports-2026'], 'out_entries': ['reports-2026']})['passed'])

    def test_form_requires_agreement_and_independent_page(self):
        self.assertFalse(evaluate('form', {'posts': ['name=Orca'], 'ax': 'Thanks, Orca'})['passed'])
        self.assertTrue(evaluate('form', {'posts': ['name=Orca&agree=yes'], 'ax': 'Thanks, Orca'})['passed'])

    def test_values_are_read_from_ax_not_model_text(self):
        self.assertFalse(evaluate('calculator', {'answer': '7006652'})['passed'])
        self.assertTrue(evaluate('calculator', {'ax': [{'role': 'AXStaticText', 'AXValue': '7,006,652'}]})['passed'])
        # Calculator wraps values in left-to-right marks.
        marked = {'ax': [{'role': 'AXStaticText', 'AXValue': '\u200e7,006,652\u200e'}], 'answer': '结果：\u200e7,006,652'}
        self.assertTrue(evaluate('calculator', marked)['passed'])
        self.assertFalse(evaluate('counter', {'counter': '<p>3</p>', 'ax': '0'})['passed'])
        self.assertTrue(evaluate('counter', {'counter': '<button>+</button>', 'ax': [{'role': 'AXStaticText', 'AXValue': '3'}]})['passed'])

    def test_electron_read_only_and_gui_coverage(self):
        self.assertFalse(evaluate('sidebar', {'ax': '首页', 'actions': {'click': 1}})['passed'])
        self.assertTrue(evaluate('sidebar', {'ax': [{'role': 'AXButton', 'AXTitle': '首页'}, {'role': 'AXButton', 'AXTitle': '助手'}], 'actions': {'observe': 1}})['passed'])


if __name__ == '__main__':
    unittest.main()
