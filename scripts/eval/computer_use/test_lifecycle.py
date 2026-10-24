import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from preflight import check_state, overlays, Native, restoration_check, browser_window
from tui import bind_session, pending_approvals
from run import run_task, retry_task_cleanup, settled_browser_window
from types import SimpleNamespace
from services import FormHandler
from http.server import ThreadingHTTPServer
from threading import Thread
from urllib.request import urlopen, Request


class PreflightTests(unittest.TestCase):
    def test_cleanup_checks_launched_apps_even_without_windows(self):
        native = object.__new__(Native)
        before = {'apps': [], 'windows': []}
        state = {'apps': [{'pid': 42, 'bundle': 'com.apple.TextEdit'}], 'windows': []}
        with patch.object(native, 'call', side_effect=lambda op, *args: state if op == 'state' else {'terminated': False}):
            result = native.cleanup({'allowed_apps': ['com.apple.TextEdit']}, before, settle_seconds=0)
        self.assertFalse(result['ok'])
        self.assertEqual(result['remaining_launched_apps'], [42])

    def test_cleanup_tolerates_apps_that_quit_with_their_last_window(self):
        native = object.__new__(Native)
        user = [{'id': 1, 'pid': 1, 'layer': 0, 'on_screen': True}]
        before = {'apps': [{'pid': 1, 'bundle': 'com.example.user'}],
                  'windows': user + [{'id': 2, 'pid': 1, 'layer': 0, 'on_screen': False}]}
        running = {'apps': before['apps'] + [{'pid': 42, 'bundle': 'com.apple.calculator'}],
                   'windows': user + [{'id': 7, 'pid': 42, 'layer': 0, 'on_screen': True},
                                      {'id': 8, 'pid': 42, 'layer': 0, 'on_screen': False}]}
        # Closing window 7 quits Calculator; the user's helper window 2 also went away on its own.
        states, calls = [running], []
        def call(op, *args):
            calls.append((op, *args))
            if op == 'state':
                return states.pop() if states else {'apps': before['apps'], 'windows': user}
            return {'window': int(args[1]), 'close_error': 0}
        with patch.object(native, 'call', side_effect=call):
            result = native.cleanup({'allowed_apps': ['com.apple.calculator']}, before, settle_seconds=0)
        self.assertTrue(result['ok'], result)
        self.assertEqual([c for c in calls if c[0] != 'state'], [('close', 'com.apple.calculator', '7', 'keep')])

    def test_cleanup_leaves_status_bubbles_of_user_windows_alone(self):
        native = object.__new__(Native)
        before = {'apps': [{'pid': 5, 'bundle': 'com.google.Chrome'}],
                  'windows': [{'id': 1, 'pid': 5, 'layer': 0, 'on_screen': True}]}
        bubble = {'id': 9, 'pid': 5, 'layer': 0, 'on_screen': True}
        state = {'apps': before['apps'], 'windows': before['windows'] + [bubble]}
        def call(op, *args):
            return state if op == 'state' else {'window': int(args[1]), 'transient': True, 'reason': 'AXCloseButton absent'}
        with patch.object(native, 'call', side_effect=call):
            result = native.cleanup({'allowed_apps': ['com.google.Chrome']}, before, settle_seconds=0)
        self.assertTrue(result['ok'], result)
        self.assertEqual(result['transient_windows'], [9])

    def test_cleanup_retries_a_window_whose_save_sheet_was_late(self):
        native = object.__new__(Native)
        before = {'apps': [{'pid': 42, 'bundle': 'com.apple.TextEdit'}], 'windows': []}
        doc = {'id': 7, 'pid': 42, 'layer': 0, 'on_screen': True}
        closes = []
        def call(op, *args):
            if op == 'state':
                return {'apps': before['apps'], 'windows': [] if len(closes) >= 2 else [doc]}
            closes.append(args)
            return {'window': 7, 'close_error': 0}
        with patch.object(native, 'call', side_effect=call), patch('preflight.time.sleep'):
            result = native.cleanup({'allowed_apps': ['com.apple.TextEdit']}, before, settle_seconds=1)
        self.assertTrue(result['ok'], result)
        self.assertEqual(len(closes), 2)

    def test_native_timeouts_become_runtime_errors(self):
        native = object.__new__(Native)
        native.binary = Path('/usr/bin/true')
        with patch('preflight.subprocess.run', side_effect=__import__('subprocess').TimeoutExpired('x', 20)):
            with self.assertRaisesRegex(RuntimeError, 'calculator-reset timed out after 40 s'):
                native.call('calculator-reset', timeout=40)

    def test_cleanup_records_quit_races_and_fails_on_a_closed_visible_original(self):
        native = object.__new__(Native)
        toast = {'id': 3, 'pid': 1, 'layer': 0, 'on_screen': True, 'bounds': {'Width': 138, 'Height': 22}}
        before = {'apps': [{'pid': 1, 'bundle': 'com.example.user'}],
                  'windows': [{'id': 1, 'pid': 1, 'layer': 0, 'on_screen': True, 'bounds': {'Width': 800, 'Height': 600}}, toast]}
        running = {'apps': before['apps'] + [{'pid': 42, 'bundle': 'com.apple.TextEdit'}], 'windows': before['windows']}
        states = [running, running]
        def call(op, *args):
            if op == 'state':
                return states.pop() if states else {'apps': before['apps'], 'windows': []}
            raise RuntimeError('Native quit: Task application is not running')
        with patch.object(native, 'call', side_effect=call):
            result = native.cleanup({'allowed_apps': ['com.apple.TextEdit']}, before, settle_seconds=0)
        self.assertEqual(result['actions'], [{'pid': 42, 'error': 'Native quit: Task application is not running'}])
        self.assertEqual(result['remaining_launched_apps'], [])
        # The user's real window counts; their toast vanishing on its own does not.
        self.assertEqual(result['missing_original_window_ids'], [1])
        self.assertFalse(result['ok'])

    def test_browser_setup_requires_new_owned_window(self):
        before = {'windows': [{'id': 1}]}
        after = {'frontmost': 'com.apple.Safari', 'apps': [{'bundle': 'com.apple.Safari', 'pid': 5}],
                 'windows': [{'id': 1, 'pid': 5, 'layer': 0, 'on_screen': True}, {'id': 2, 'pid': 5, 'layer': 0, 'on_screen': True},
                             {'id': 4, 'pid': 5, 'layer': 0, 'on_screen': False}]}
        # A new off-screen helper window does not make the visible task window ambiguous.
        self.assertEqual(browser_window(before, after, 'com.apple.Safari', 2), 2)
        # Nor does a visible status bubble, which is not a standard AX window.
        after['windows'].append({'id': 5, 'pid': 5, 'layer': 0, 'on_screen': True})
        with self.assertRaises(RuntimeError):
            browser_window(before, after, 'com.apple.Safari', 2)
        self.assertEqual(browser_window(before, after, 'com.apple.Safari', 2, standard=[1, 2]), 2)
        after['windows'].pop()
        for claimed in (1, 3):
            with self.assertRaises(RuntimeError):
                browser_window(before, after, 'com.apple.Safari', claimed)
        after['frontmost'] = 'com.example.terminal'
        with self.assertRaises(RuntimeError):
            browser_window(before, after, 'com.apple.Safari', 2)

    def test_browser_setup_waits_for_transient_popups_to_close(self):
        before = {'windows': [{'id': 1}]}
        base = {'frontmost': 'com.google.Chrome', 'apps': [{'bundle': 'com.google.Chrome', 'pid': 5}]}
        task = {'id': 2, 'pid': 5, 'layer': 0, 'on_screen': True}
        popup = {'id': 3, 'pid': 5, 'layer': 0, 'on_screen': True}
        states = [{**base, 'windows': [task]}, {**base, 'windows': [task, popup]}]
        native = SimpleNamespace(call=lambda op: states.pop())
        with patch('run.time.sleep'):
            self.assertEqual(settled_browser_window(native, before, 'com.google.Chrome', 2)[0], 2)
        native = SimpleNamespace(call=lambda op: {**base, 'windows': [task, popup]})
        with self.assertRaises(RuntimeError):
            settled_browser_window(native, before, 'com.google.Chrome', 2, timeout=0)

    def test_restore_checks_actual_pid_and_pointer_tolerance(self):
        initial = {'front_pid': 1, 'pointer': [100.5, 200.5]}
        self.assertTrue(restoration_check(initial, {'front_pid': 1, 'pointer': [100, 200]})['ok'])
        self.assertFalse(restoration_check(initial, {'front_pid': 2, 'pointer': [100, 200]})['ok'])
        self.assertFalse(restoration_check(initial, {'front_pid': 1, 'pointer': [110, 200]})['ok'])

    def test_dry_preflight_records_lock_without_requiring_desktop(self):
        native = object.__new__(Native)
        state = {'frontmost': 'com.apple.loginwindow', 'windows': [], 'displays': []}
        with patch.object(native, 'call', return_value=state), patch('preflight.subprocess.run') as run:
            run.return_value.stdout = '"CGSSessionScreenIsLocked" = Yes'
            recorded = native.preflight(live=False)
            self.assertFalse(recorded['gui_ready'])
            with self.assertRaises(RuntimeError):
                native.preflight(live=True)

    def test_locked_and_loginwindow_stop(self):
        for lock, front in [('"CGSSessionScreenIsLocked" = Yes', 'com.apple.finder'), ('', 'com.apple.loginwindow')]:
            with self.assertRaises(RuntimeError):
                check_state({'frontmost': front}, lock)
        check_state({'frontmost': 'com.example.terminal'}, 'CGSSessionScreenIsLocked absent')

    def test_layer1000_fullscreen_on_any_display(self):
        s = {'displays': [{'X': 0, 'Y': 0, 'Width': 1000, 'Height': 800}],
             'windows': [{'id': 1, 'layer': 1000, 'on_screen': True,
                          'bounds': {'X': 0, 'Y': 0, 'Width': 1000, 'Height': 800}},
                         {'id': 2, 'layer': 1000, 'on_screen': True,
                          'bounds': {'X': 0, 'Y': 0, 'Width': 10, 'Height': 10}}]}
        self.assertEqual([w['id'] for w in overlays(s)], [1])


class SessionTests(unittest.TestCase):
    def test_exact_fresh_session_not_latest(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            old = root / 'old.jsonl'
            old.write_text(json.dumps({'type': 'session.meta', 'title': 'prompt', 'provider': 'mock', 'session_id': 'old'}) + '\n' + json.dumps({'type': 'conversation.message', 'message': {'role': 'user', 'content': 'prompt'}}) + '\n')
            new = root / 'new.jsonl'
            new.write_text(json.dumps({'type': 'session.meta', 'title': 'prompt', 'provider': 'mock', 'session_id': 'new'}) + '\n' + json.dumps({'type': 'conversation.message', 'message': {'role': 'user', 'content': 'prompt'}}) + '\n')
            self.assertEqual(bind_session(root, {old}, 'prompt', 'mock'), new)
            extra = root / 'extra.jsonl'
            extra.write_text(new.read_text())
            with self.assertRaises(RuntimeError):
                bind_session(root, {old}, 'prompt', 'mock')
            self.assertIsNone(bind_session(root, {old, new, extra}, 'prompt', 'mock'))

    def test_long_prompt_binds_full_user_message_not_truncated_title(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); path = root / 's.jsonl'
            prompt = 'test ' * 30
            path.write_text(json.dumps({'type': 'session.meta', 'title': 'test', 'provider': 'mock', 'session_id': 'new'}) + '\n')
            self.assertIsNone(bind_session(root, set(), prompt, 'mock'))
            with path.open('a') as f:
                f.write(json.dumps({'type': 'conversation.message', 'message': {'role': 'user', 'content': prompt}}) + '\n')
            self.assertEqual(bind_session(root, set(), prompt, 'mock'), path)
            self.assertIsNone(bind_session(root, set(), 'test', 'mock'))

    def test_pending_permission_uses_committed_interaction_identity(self):
        interaction = {'interaction_id': 'id1', 'request': {'PermissionRequest': {'tool_call_id': 'call1'}}}
        records = [{'type': 'runtime.surface_commit.prepared', 'commit_id': 'commit1', 'batch': {'events': [
            {'event': {'Interaction': {'Requested': {'interaction': interaction}}}}]}}]
        self.assertEqual(pending_approvals(records), [])
        records.append({'type': 'runtime.surface_commit.committed', 'commit_id': 'commit1'})
        self.assertEqual(pending_approvals(records)[0]['id'], 'id1')
        records.extend([{'type': 'runtime.surface_commit.prepared', 'commit_id': 'commit2', 'batch': {'events': [
            {'event': {'Interaction': {'Resolved': {'interaction_id': 'id1'}}}}]}},
            {'type': 'runtime.surface_commit.committed', 'commit_id': 'commit2'}])
        self.assertEqual(pending_approvals(records), [])


class RunnerLifecycleTests(unittest.TestCase):
    def test_failed_stop_cleanup_is_retried_only_after_pid_exit(self):
        class Native:
            called = False
            def cleanup(self, task, before):
                self.called = True
                return {'ok': True}
        with tempfile.TemporaryDirectory() as d:
            evidence = Path(d); native = Native()
            with patch('run.os.kill', side_effect=ProcessLookupError):
                result = retry_task_cleanup({}, {}, evidence, native, 123)
            self.assertTrue(native.called)
            self.assertTrue(result['ok'])
            self.assertTrue((evidence / 'cleanup-recovery.json').exists())
            native.called = False
            with patch('run.os.kill', return_value=None):
                result = retry_task_cleanup({}, {}, evidence, native, 123)
            self.assertFalse(native.called)
            self.assertFalse(result['ok'])

    def test_startup_failure_keeps_evidence_and_waits_for_exit(self):
        order = []
        class FakeTUI:
            submitted = None
            approvals = []
            def __init__(self, *args): pass
            def start(self):
                order.append('start')
                raise RuntimeError('startup failed')
            def stop(self):
                order.append('exited')
                return {'exited': True, 'records': []}
        class Native:
            def preflight(self, live): return {'windows': [], 'apps': []}
            def cleanup(self, task, before):
                order.append('cleanup')
                return {'ok': True}
        class Services:
            port = 1234
            def health(self): return {'http': 200}
        with tempfile.TemporaryDirectory() as d:
            base = Path(d); (base / 'out').mkdir(); (base / 'fixtures').mkdir()
            task = {'id': 'M7', 'kind': 'multi', 'allowed_apps': ['com.apple.TextEdit'], 'prompt': 'prompt', 'verifier': 'draft'}
            with patch('run.TUI', FakeTUI), patch('run.collect', return_value={'passed': False}):
                row = run_task(task, SimpleNamespace(dry_run=False), base, base / 'report', None, Services(), Native())
            self.assertTrue(row['harness_error'])
            self.assertEqual(order, ['start', 'exited', 'cleanup'])
            self.assertTrue((base / 'report/evidence/M7/metrics.json').is_file())

    def test_verifier_failure_still_cleans_and_retains_started_task(self):
        class FakeTUI:
            submitted = 0
            approvals = []
            def __init__(self, *args): pass
            def start(self): pass
            def run(self, *args): return 'completed'
            def stop(self): return {'exited': True, 'records': []}
        class Native:
            def preflight(self, live): return {'windows': [], 'apps': []}
            def cleanup(self, task, before): return {'ok': True}
        class Services:
            port = 1234
            def health(self): return {}
        with tempfile.TemporaryDirectory() as d:
            base = Path(d); (base / 'out').mkdir(); (base / 'fixtures').mkdir()
            task = {'id': 'M7', 'kind': 'multi', 'allowed_apps': ['com.apple.TextEdit'], 'prompt': 'prompt', 'verifier': 'draft'}
            with patch('run.TUI', FakeTUI), patch('run.collect', side_effect=RuntimeError('AX failed')):
                row = run_task(task, SimpleNamespace(dry_run=False), base, base / 'report', None, Services(), Native())
            self.assertTrue(row['harness_error'])
            self.assertTrue(row['cleanup']['ok'])

    def test_calculator_task_fails_when_the_reset_left_a_stale_display(self):
        class FakeTUI:
            submitted = 0
            approvals = []
            def __init__(self, *args): pass
            def start(self): pass
            def run(self, *args): return 'completed'
            def stop(self): return {'exited': True, 'records': []}
        class Native:
            def __init__(self, display): self.display = display
            def call(self, op, timeout=20): return {'pressed': [], 'display': self.display}
            def preflight(self, live): return {'windows': [], 'apps': []}
            def cleanup(self, task, before): return {'ok': True}
        class Services:
            port = 1234
            def health(self): return {}
        task = {'id': 'S1', 'kind': 'single', 'allowed_apps': ['com.apple.calculator'], 'prompt': 'prompt', 'verifier': 'calculator'}
        for display, passed in ((['\u200e7,006,652'], False), (['\u200e0'], True)):
            with tempfile.TemporaryDirectory() as d:
                base = Path(d); (base / 'out').mkdir(); (base / 'fixtures').mkdir()
                with patch('run.TUI', FakeTUI), patch('run.collect', return_value={'passed': True}):
                    row = run_task(task, SimpleNamespace(dry_run=False), base, base / 'report', None, Services(), Native(display))
                self.assertEqual(row['passed'], passed, display)


class FormTests(unittest.TestCase):
    def test_server_health_form_and_post(self):
        with tempfile.TemporaryDirectory() as d:
            base = Path(d)
            (base / 'fixtures').mkdir(); (base / 'out').mkdir()
            (base / 'fixtures/form.html').write_text('<form>Name</form>')
            handler = type('BoundHandler', (FormHandler,), {'base': base})
            server = ThreadingHTTPServer(('127.0.0.1', 0), handler)
            thread = Thread(target=server.serve_forever); thread.start()
            try:
                origin = f'http://127.0.0.1:{server.server_port}'
                with urlopen(origin + '/health') as r:
                    self.assertEqual(r.read(), b'orca-cu-eval')
                with urlopen(Request(origin + '/form.html', data=b'name=Orca&agree=yes')) as r:
                    self.assertIn(b'Thanks, Orca', r.read())
                self.assertEqual((base / 'out/form-posts.log').read_text(), 'name=Orca&agree=yes\n')
            finally:
                server.shutdown(); server.server_close(); thread.join()
