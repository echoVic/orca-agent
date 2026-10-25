#!/usr/bin/env python3
"""Run with python3 -I. Only --dry-run is GUI-free."""
import argparse
from datetime import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

# -I intentionally removes the script directory; add this known source directory.
HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from approvals import detector_self_test
from fixtures import BASE, archive_previous, prepare, reset_outputs
from launch import environment as child_environment
from metrics import extract, read_jsonl, user_window_touches
from preflight import Native, browser_window
from report import dump, write_report
from services import Tmux, Services
from tasks import REPO, load_tasks
from tui import TUI, MOCK_PROMPT
from verify import collect


def select_tasks(tasks, selection):
    if selection is None:
        return tasks
    ids = selection.split(',')
    if len(set(ids)) != len(ids) or set(ids) - {t['id'] for t in tasks}:
        raise ValueError('Use unique task IDs S1–S8,M1–M7, separated by commas')
    return [t for t in tasks if t['id'] in ids]


def settled_browser_window(native, before, app, claimed, timeout=3.0, standard=None):
    """Prove the claimed window once transient popups (Chrome's omnibox list) have closed."""
    deadline = time.monotonic() + timeout
    while True:
        after = native.call('state')
        try:
            return browser_window(before, after, app, claimed, standard), after
        except RuntimeError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(.25)


def interrupted(signum, frame):
    raise InterruptedError(f'Interrupted by signal {signum}')


def notify(text):
    try:
        r = subprocess.run(['osascript', '-e', f'display notification "{text}" with title "Orca acceptance"'],
                           capture_output=True, timeout=5, check=False)
        return {'command_completed': r.returncode == 0}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {'command_completed': False, 'error': str(error)}


def retry_task_cleanup(task, before, evidence, native, pid):
    if pid:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            pass
        else:
            result = {'ok': False, 'reason': 'Orca PID still alive after server shutdown'}
            dump(evidence / 'cleanup-recovery.json', result)
            return result
    result = native.cleanup(task, before)
    result['recovered_after_server_stop'] = True
    dump(evidence / 'cleanup-recovery.json', result)
    return result


def run_task(task, args, base, out, tmux, services, native):
    evidence = out / 'evidence' / task['id']; evidence.mkdir(parents=True)
    reset_outputs(base, evidence / 'previous-outputs')
    reset_failed = None
    if not args.dry_run and 'com.apple.calculator' in task['allowed_apps']:
        try:
            # Launch, clear and quit wait up to 8 s each.
            reset = native.call('calculator-reset', timeout=40)
        except RuntimeError as error:
            reset = {'error': str(error)}
        dump(evidence / 'calculator-reset.json', reset)
        if not any(re.sub(r'[\s\u200e\u200f]', '', value) == '0' for value in reset.get('display', [])):
            # A display left from an earlier run could otherwise answer the task.
            reset_failed = 'Calculator reset did not show a cleared display; GUI result cannot be trusted'
    before = native.preflight(live=not args.dry_run)
    dump(evidence / 'preflight.json', before)
    dump(evidence / 'service-health.json', services.health())
    prompt = MOCK_PROMPT if args.dry_run else task['prompt'].format(port=services.port)
    tui = TUI(tmux, REPO / 'target/release/orca', base, evidence, 'mock' if args.dry_run else 'deepseek')
    verification = {'passed': None, 'reason': 'dry run; GUI verification skipped'}
    reason, exit_proof, cleanup, errors = 'not_started', {}, {}, []
    # Existing windows the task provides: Orca asks about them, and the harness approves.
    task_windows = 'all' if task.get('existing_windows') == 'approve' else set()
    try:
        if not args.dry_run and task['allowed_apps'][0] in ('com.apple.Safari', 'com.google.Chrome'):
            app = task['allowed_apps'][0]
            if not any(a['bundle'] == app for a in before['apps']):
                subprocess.run(['open', '-b', app], check=True, capture_output=True, timeout=15)
                # Protect windows restored by the browser at app launch from AX/close.
                launched = native.call('state')
                before['windows'] = launched['windows']
                dump(evidence / 'browser-restored-state.json', launched)
            browser_before = native.call('state')
            created = native.call('new-browser', app)
            wid, browser_after = settled_browser_window(native, browser_before, app, created['window_id'],
                                                        standard=created.get('standard_window_ids'))
            dump(evidence / 'browser-window.json', {'window': wid, 'before': browser_before, 'after': browser_after})
            # The harness's window predates Orca's turn, so Orca asks before using it.
            task_windows = {wid}
        tui.start()
        def health():
            services.health()
            if not args.dry_run:
                native.preflight(live=True)
        reason = tui.run(task, prompt, health, task_windows)
    except (Exception, KeyboardInterrupt) as error:
        reason = 'harness_error'
        errors.append(f'{type(error).__name__}: {error}')
    finally:
        dump(evidence / 'cleanup-snapshot.json', before)
        # Exit proof is mandatory before verification/GUI cleanup/next task.
        try:
            exit_proof = tui.stop()
        except Exception as error:
            exit_proof = {'exited': False, 'error': str(error), 'pid': getattr(tui, 'pid', None)}
            errors.append(f'process stop: {error}')
        records = exit_proof.pop('records', [])
        metrics = extract(records, tui.approvals, tui.submitted, time.time())
        preexisting = {w['id'] for w in before['windows']}
        if isinstance(task_windows, set):
            # The harness's browser window also predates Orca's turn.
            preexisting.update(task_windows)
        metrics['user_window_touches'] = user_window_touches(
            metrics['calls'], preexisting, tui.approvals)
        metrics['window_panels'] = sum(bool(a.get('window_panel')) for a in tui.approvals)
        dump(evidence / 'metrics.json', metrics)
        try:
            if not args.dry_run and exit_proof.get('exited') and reason != 'harness_error':
                verification = collect(task, native, base, before, evidence, metrics)
                if reset_failed:
                    verification = {**verification, 'passed': False, 'reason': reset_failed}
        except Exception as error:
            errors.append(f'verification: {error}')
            verification = {'passed': False, 'error': str(error)}
        finally:
            try:
                cleanup = ({'ok': True, 'gui': 'untouched'} if args.dry_run else native.cleanup(task, before)) if exit_proof.get('exited') else {'ok': False, 'reason': 'process still alive; stop suite'}
            except Exception as error:
                cleanup = {'ok': False, 'error': str(error)}
                errors.append(f'GUI cleanup: {error}')
            cleanup['process'] = exit_proof
            dump(evidence / 'cleanup.json', cleanup)
            dump(evidence / 'verification.json', verification)
    smoke = reason in ('completed', 'success') and metrics['tool_calls'] == 0 and args.dry_run
    if args.dry_run and not smoke:
        errors.append(f'mock smoke failed: {reason}')
    if not cleanup['ok']:
        errors.append('GUI cleanup incomplete; refusing to advance')
    passed = bool(verification['passed']) and reason in ('completed', 'success')
    return {'id': task['id'], 'kind': task['kind'], 'passed': None if args.dry_run else passed,
            'reason': reason, 'metrics': metrics, 'cleanup': cleanup, 'smoke_passed': smoke,
            'overlay_count': len(before.get('overlays', [])),
            'harness_error': errors}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--tasks', help='Comma separated IDs; default all 15, one attempt each')
    parser.add_argument('--dry-run', action='store_true', help='Mock-only TUI smoke; no GUI changes or real API key access')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--baseline', type=Path, help='Previous aggregates.json')
    args = parser.parse_args(argv)
    tasks = select_tasks(load_tasks(), args.tasks)
    if args.baseline:
        json.loads(args.baseline.read_text())  # Validate before side effects.
    now = datetime.now().astimezone()
    run_id = now.strftime('%Y%m%dT%H%M%S') + f'-{os.getpid()}'
    out = (args.out or REPO / '.superpowers/sdd' / f'{now:%Y-%m-%d}-computer-use-eval' / run_id).resolve()
    if out.exists():
        raise ValueError('Output directory already exists; choose a new --out to preserve evidence')
    binary = REPO / 'target/release/orca'
    if not binary.is_file():
        raise RuntimeError('Build target/release/orca with env -u NODE_OPTIONS cargo build --release --locked --bin orca')
    BASE.mkdir(parents=True, exist_ok=True)
    rows = []
    env = {'dry_run': args.dry_run, 'started_at': now.isoformat(), 'selected_tasks': [t['id'] for t in tasks],
           'head': subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=REPO, capture_output=True, text=True, check=True).stdout.strip(),
           'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
           'tasks_sha256': hashlib.sha256((HERE / 'tasks.json').read_bytes()).hexdigest(),
           'harness_sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(HERE.iterdir()) if p.suffix in ('.py', '.swift', '.json', '.md')},
           'macos': subprocess.run(['sw_vers'], capture_output=True, text=True, check=True).stdout.strip(),
           'cleanup_errors': []}
    tmux, native, services, initial_file = Tmux(), None, None, None
    with (BASE / '.harness.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError('Another computer-use harness owns the fixtures') from None
        # Check server ownership before archiving any active state.
        if tmux.call('list-sessions', check=False):
            raise RuntimeError('Dedicated tmux server is in use; refusing to alter fixtures')
        out.mkdir(parents=True)
        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, interrupted)
        try:
            archive_previous(BASE, BASE / 'archive' / run_id)
            prepare(BASE)
            (BASE / 'state').mkdir()
            (BASE / 'home/config.toml').write_text('auto_memory = false\n')
            subprocess.run(['git', 'init', '-q', str(BASE / 'ws')], check=True, capture_output=True)
            subprocess.run([str(binary), 'trust', 'add', '--cwd', str(BASE / 'ws')],
                           env=child_environment(BASE, 'mock'), cwd=BASE / 'ws', check=True, capture_output=True, timeout=15)
            dump(out / 'approval-detector-self-test.json', detector_self_test())
            native = Native(BASE / 'state')
            initial = native.preflight(live=not args.dry_run)
            initial_file = out / 'initial-state.json'; dump(initial_file, initial)
            dump(out / 'pdf-verification.json', native.call('pdf-count', str(BASE / 'fixtures/two-pages.pdf')))
            tmux.start()
            services = Services(tmux, BASE); services.start()
            dump(out / 'services.json', services.health())
            if not args.dry_run:
                env['start_notification'] = notify('Computer-use acceptance started')
            for task in tasks:
                print(f'{task["id"]}: {"mock smoke" if args.dry_run else "live"}', flush=True)
                row = run_task(task, args, BASE, out, tmux, services, native)
                rows.append(row)
                dump(out / 'progress.json', [{'id': r['id'], 'reason': r['reason']} for r in rows])
                if row['harness_error']:
                    raise RuntimeError(f'{task["id"]}: ' + '; '.join(row['harness_error']))
        except (Exception, KeyboardInterrupt) as error:
            env['error'] = f'{type(error).__name__}: {error}'
        finally:
            # Once cancellation starts, a second signal must not interrupt cleanup.
            for sig in (signal.SIGTERM, signal.SIGINT):
                signal.signal(sig, signal.SIG_IGN)
            def recover_windows():
                if args.dry_run or native is None or tmux.owned:
                    return
                for row in rows:
                    if not row['cleanup']['ok']:
                        task = next(t for t in tasks if t['id'] == row['id'])
                        evidence = out / 'evidence' / row['id']
                        before = json.loads((evidence / 'cleanup-snapshot.json').read_text())
                        recovered = retry_task_cleanup(task, before, evidence, native, row['cleanup']['process'].get('pid'))
                        if not recovered['ok']:
                            env['cleanup_errors'].append(f'{row["id"]}: scoped GUI cleanup remains incomplete')
            for name, action in [('services', services.stop if services else None), ('tmux', tmux.stop), ('windows', recover_windows),
                                 ('restore', (lambda: native.restore(initial_file, args.dry_run)) if native and initial_file else None)]:
                if action:
                    try:
                        result = action()
                        if name == 'services':
                            dump(out / 'service-cleanup.json', result)
                        if name == 'restore':
                            dump(out / 'restoration.json', result)
                            if not args.dry_run and not result['ok']:
                                env['cleanup_errors'].append('Frontmost/pointer restoration did not match initial state')
                    except Exception as error:
                        env['cleanup_errors'].append(f'{name}: {error}')
            if not args.dry_run and initial_file:
                env['end_notification'] = notify('Computer-use acceptance ended')
            env['ended_at'] = datetime.now().astimezone().isoformat()
            write_report(out, rows, env, args.baseline)
    print(str(out / 'report.md'), flush=True)
    return 1 if env.get('error') or env['cleanup_errors'] or len(rows) != len(tasks) or any(r['passed'] is False for r in rows) else 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, RuntimeError) as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1)
