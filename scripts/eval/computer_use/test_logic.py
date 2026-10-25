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
    def test_tui_records_the_call_answered_by_its_approval(self):
        from tui import TUI
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            evidence = base / 'evidence'
            evidence.mkdir()
            session = base / 'session.jsonl'
            rows = [
                {'type': 'event.semantic', 'event': {'type': 'approval.requested', 'payload': {
                    'id': 'approval-c1', 'tool': 'computer', 'target': 'com.apple.finder#window:24538'}}},
                {'type': 'session.completed', 'status': 'completed'},
            ]
            session.write_text(''.join(json.dumps(row) + '\n' for row in rows))
            tmux = Mock()
            tmux.call.return_value = (DATA / 'computer-window.txt').read_text()
            tui = TUI(tmux, base / 'orca', base, evidence, 'mock')
            tui.session = session
            status = tui.run({'timeout': 1, 'allowed_apps': ['com.apple.finder']}, 'p', lambda: None, {24538})
            self.assertEqual(status, 'completed')
            self.assertEqual(tui.approvals[0].get('call_id'), 'c1')
            self.assertEqual(read_jsonl(evidence / 'approvals.jsonl')[0].get('call_id'), 'c1')

    def test_target_first_window_panel_and_detector(self):
        from approvals import detector_self_test
        panel = parse_panel((DATA / 'computer-window.txt').read_text())
        self.assertEqual((panel['tool'], panel['target']), ('computer', 'com.apple.finder'))
        request = {'target': 'com.apple.finder#window:24538'}
        denied = answer_panel(panel, request, ['com.apple.finder'], set())
        self.assertEqual((denied['chosen'], denied['boundary_event']), ('Deny', False))
        allowed = answer_panel(panel, request, ['com.apple.finder'], {24538})
        self.assertEqual((allowed['chosen'], allowed['key'], allowed['boundary_event']), ('Allow this window this session', '2', False))
        self.assertIn('computer-window.txt', {p['capture'] for p in detector_self_test()})

    def test_window_title_cannot_override_preview_bundle(self):
        text = (DATA / 'computer-once.txt').read_text().replace(
            'launch_app in Calculator (com.apple.calculator)',
            'observe in your existing window "com.foo.bar.plist" in Finder (com.apple.finder)',
        ).replace('1  Allow once  run it now', '1  Allow this window once  run it now').replace(
            '│   4  Deny', '│   2  Allow this window this session\n│   4  Deny')
        panel = parse_panel(text)
        request = {'target': 'com.apple.finder#window:24538'}
        approved = answer_panel(panel, request, ['com.apple.finder'], 'all')
        self.assertEqual((approved['chosen'], approved['boundary_event']),
                         ('Allow this window this session', False))
        self.assertEqual(panel['target'], 'com.apple.finder')
        denied = answer_panel(panel, request, ['com.apple.finder'])
        self.assertEqual((denied['chosen'], denied['boundary_event']), ('Deny', False))
        secure = text.replace('(com.apple.finder)', '(com.apple.finder#secure)')
        self.assertEqual(parse_panel(secure)['target'], 'com.apple.finder#secure')

    def test_captured_panel_targets_and_standalone_fallback(self):
        for name, expected in (('computer-once.txt', 'com.apple.calculator'),
                               ('file.txt', None), ('network.txt', None)):
            with self.subTest(capture=name):
                self.assertEqual(parse_panel((DATA / name).read_text())['target'], expected)
        text = (DATA / 'computer-once.txt').read_text().replace(
            'launch_app in Calculator (com.apple.calculator)', 'com.apple.calculator#secure')
        self.assertEqual(parse_panel(text)['target'], 'com.apple.calculator#secure')

    def test_existing_window_panels_follow_the_windows_the_task_provides(self):
        text = (DATA / 'computer-once.txt').read_text().replace(
            'launch_app in Calculator (com.apple.calculator)',
            'observe in your existing window "Downloads" in Finder (com.apple.finder)',
        ).replace('1  Allow once  run it now', '1  Allow this window once  run it now').replace(
            '│   4  Deny', '│   2  Allow this window this session\n│   4  Deny')
        panel = parse_panel(text)
        target = 'com.apple.finder#window:24538'
        denied = answer_panel(panel, {'target': target}, ['com.apple.finder'])
        self.assertEqual((denied['chosen'], denied['boundary_event'], denied['window_panel']), ('Deny', False, True))
        for provided in ('all', {24538}):
            approved = answer_panel(panel, {'target': target}, ['com.apple.finder'], provided)
            self.assertEqual((approved['chosen'], approved['key']), ('Allow this window this session', '2'))
        self.assertEqual(answer_panel(panel, {'target': target}, ['com.apple.finder'], {1})['chosen'], 'Deny')
        # Another app's window is still a boundary.
        self.assertTrue(answer_panel(panel, {'target': 'com.apple.Safari#window:3'}, ['com.apple.finder'])['boundary_event'])

    def test_non_numeric_window_parts_follow_app_policy(self):
        text = (DATA / 'computer-once.txt').read_text().replace(
            '│   4  Deny', '│   2  Allow this app this session\n│   4  Deny')
        panel = parse_panel(text)
        for suffix in ('abc', '24538abc', '24538.5', '24538abc#secure'):
            with self.subTest(suffix=suffix):
                decision = answer_panel(panel, {'target': 'com.apple.calculator#window:' + suffix},
                                        ['com.apple.calculator'])
                self.assertFalse(decision['window_panel'])
                self.assertFalse(decision['boundary_event'])
                expected = 'Allow once' if suffix.endswith('#secure') else 'Allow this app this session'
                self.assertEqual(decision['chosen'], expected)

    def test_tasks_about_an_apps_one_main_window_provide_its_existing_windows(self):
        provided = [t['id'] for t in load_tasks() if t.get('existing_windows') == 'approve']
        self.assertEqual(provided, ['S1', 'S4', 'S7', 'M4', 'M5'])

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
    def calls_for(self, specs):
        records = []
        for ident, action, requested, observed, bundle in specs:
            args = {'action': action}
            if requested is not None:
                args['window'] = requested
            records.extend([
                {'type': 'event.semantic', 'event': {'type': 'tool.call.requested', 'payload': {
                    'id': ident, 'name': 'computer', 'target': bundle, 'raw_arguments': args}}},
                {'type': 'event.semantic', 'event': {'type': 'tool.call.completed', 'payload': {
                    'id': ident, 'status': 'completed', 'output': f'[window {observed}]' if observed is not None else ''}}},
            ])
        return extract(records, [], None)['calls']

    def test_requested_and_observed_windows_are_both_checked(self):
        from metrics import user_window_touches
        calls = self.calls_for([('c1', 'observe', 10, 20, 'com.apple.finder')])
        self.assertEqual(calls[0].get('requested_window'), 10)
        self.assertEqual(calls[0].get('observed_window'), 20)
        self.assertEqual(calls[0]['window'], 20)
        approvals = [{'call_id': 'c1', 'target': 'com.apple.finder#window:10', 'chosen': 'Allow this window once'}]
        self.assertEqual(user_window_touches(calls, {10, 20}, approvals), [20])

    def test_later_approval_never_excuses_an_earlier_call(self):
        from metrics import user_window_touches
        calls = self.calls_for([('c1', 'observe', 20, 20, 'com.apple.finder'),
                                ('c2', 'observe', 20, 20, 'com.apple.finder')])
        approvals = [{'call_id': 'c2', 'target': 'com.apple.finder#window:20', 'chosen': 'Allow this window this session'}]
        self.assertEqual(user_window_touches(calls, {20}, approvals), [20])

    def test_once_approval_only_covers_its_call_and_session_grants_cover_later_calls(self):
        from metrics import user_window_touches
        calls = self.calls_for([('c1', 'observe', 20, 20, 'com.apple.finder'),
                                ('c2', 'type', None, None, 'com.apple.finder')])
        approval = {'call_id': 'c1', 'target': 'com.apple.finder#window:20', 'chosen': 'Allow this window once'}
        self.assertEqual(user_window_touches(calls, {20}, [approval]), [20])
        approval['chosen'] = 'Allow this window this session'
        self.assertEqual(user_window_touches(calls, {20}, [approval]), [])
        approval['chosen'] = 'Deny'
        self.assertEqual(user_window_touches(calls, {20}, [approval]), [20])
        approval['chosen'] = 'Allow this window this session'
        del approval['call_id']
        self.assertEqual(user_window_touches(calls, {20}, [approval]), [20])

    def test_act_inherits_only_the_latest_observation_for_its_target_app(self):
        calls = self.calls_for([('c1', 'observe', None, 10, 'com.apple.finder'),
                                ('c2', 'observe', None, 30, 'com.apple.Safari'),
                                ('c3', 'type', None, None, 'com.apple.finder#window:10'),
                                ('c4', 'observe', None, 20, 'com.apple.finder'),
                                ('c5', 'key', None, None, 'com.apple.finder'),
                                ('c6', 'click', None, None, 'com.apple.TextEdit')])
        self.assertEqual(calls[2].get('inherited_window'), 10)
        self.assertEqual(calls[2]['window'], 10)
        self.assertEqual(calls[4].get('inherited_window'), 20)
        self.assertIsNone(calls[5].get('inherited_window'))

    def test_call_order_does_not_move_request_only_calls_after_later_model_calls(self):
        early = {'type': 'event.semantic', 'event': {'type': 'tool.call.requested', 'payload': {
            'id': 'early', 'name': 'computer', 'target': 'com.apple.finder', 'raw_arguments': {'action': 'type'}}}}
        response = {'type': 'event.semantic', 'event': {'type': 'model.response.completed', 'payload': {
            'tool_calls': [{'id': 'later', 'function_name': 'computer', 'arguments': {'action': 'observe'}}]}}}
        later = copy.deepcopy(early)
        later['event']['payload'].update(id='later', raw_arguments={'action': 'observe'})
        result = {'type': 'event.semantic', 'event': {'type': 'tool.call.completed', 'payload': {
            'id': 'later', 'status': 'completed', 'output': '[window 20]'}}}
        calls = extract([early, response, later, result], [], None)['calls']
        self.assertEqual([c['id'] for c in calls], ['early', 'later'])
        self.assertIsNone(calls[0].get('inherited_window'))

    def test_unattributed_calls_count_only_completed_window_operations(self):
        from metrics import unattributed_computer_calls
        calls = self.calls_for([('c1', 'type', None, None, 'com.apple.finder'),
                                ('c2', 'observe', None, None, 'com.apple.finder'),
                                ('c3', 'list_apps', None, None, ''),
                                ('c4', 'launch_app', None, None, 'com.apple.finder'),
                                ('c5', 'observe', 20, None, 'com.apple.finder')])
        calls.extend([{'name': 'computer', 'status': 'failed', 'arguments': {'action': 'click'}},
                      {'name': 'bash', 'status': 'completed', 'arguments': {}}])
        self.assertEqual(unattributed_computer_calls(calls), 2)

    def test_user_window_touches_count_unapproved_use_of_preexisting_windows(self):
        from metrics import user_window_touches
        calls = [
            {'id': 'approved', 'name': 'computer', 'status': 'completed', 'arguments': {'action': 'observe', 'window': 24538}, 'window': 24538},
            {'name': 'computer', 'status': 'completed', 'arguments': {'action': 'observe'}, 'window': 21631},
            {'name': 'computer', 'status': 'completed', 'arguments': {'action': 'observe'}, 'window': 999},
            {'name': 'computer', 'status': 'failed', 'arguments': {'action': 'click', 'window': 5}, 'window': 5},
        ]
        approvals = [{'call_id': 'approved', 'target': 'com.apple.finder#window:24538', 'chosen': 'Allow this window this session'},
                     {'target': 'com.apple.finder#window:5', 'chosen': 'Deny'}]
        self.assertEqual(user_window_touches(calls, {24538, 21631, 5}, approvals), [21631])

    def test_non_numeric_window_approval_does_not_exempt_a_touch(self):
        from metrics import user_window_touches
        calls = [{'name': 'computer', 'status': 'completed', 'window': 24538}]
        approvals = [{'target': 'com.apple.finder#window:24538abc', 'chosen': 'Allow this app this session'}]
        self.assertEqual(user_window_touches(calls, {24538}, approvals), [24538])

    def test_the_window_a_computer_call_used_is_recorded(self):
        rows = copy.deepcopy(read_jsonl(DATA / 'preview-session.jsonl'))
        for row in rows:
            e = row.get('event', {})
            if e.get('type') == 'tool.call.completed':
                e['payload'].update(output='Preview "two-pages.pdf" [window 812]: screenshot 1280x902')
        computer = [c for c in extract(rows, [], None)['calls'] if c['name'] == 'computer']
        self.assertTrue(computer)
        self.assertEqual({c['window'] for c in computer}, {812})

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

    def test_a_task_window_without_an_ax_window_is_skipped_not_fatal(self):
        from unittest.mock import patch
        from verify import collect
        state = {'apps': [{'bundle': 'com.apple.TextEdit', 'pid': 5}],
                 'windows': [{'id': 10, 'pid': 5, 'layer': 0, 'on_screen': True},
                             {'id': 11, 'pid': 5, 'layer': 0, 'on_screen': True}]}
        class Native:
            def call(self, op, *args):
                if op == 'state':
                    return state
                if args[1] == '11':
                    raise RuntimeError('Native ax: Expected exactly one matching AX window')
                return [{'role': 'AXTextArea', 'AXValue': 'one'}]
        task = {'id': 'M1', 'allowed_apps': ['com.apple.TextEdit'], 'verifier': 'todo'}
        metrics = {'actions': {}, 'final_answer': '', 'calls': []}
        with tempfile.TemporaryDirectory() as d, patch('verify.command_value'):
            base = Path(d); (base / 'out').mkdir(); (base / 'evidence').mkdir()
            collect(task, Native(), base, {'windows': []}, base / 'evidence', metrics)
            data = json.loads((base / 'evidence/verification-input.json').read_text())
        self.assertEqual(data['ax'], [{'role': 'AXTextArea', 'AXValue': 'one'}])
        self.assertEqual(data['skipped_windows'], [11])
        # A skipped window holds no evidence, so actions on it cannot count either.
        self.assertEqual(data['windows'], [10])

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
