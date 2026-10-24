"""Read-only preflight; GUI changes are confined to explicitly live runs."""
import json
import math
from pathlib import Path
import subprocess
import time


def check_state(state, lock_text):
    if any('CGSSessionScreenIsLocked' in l and ('Yes' in l or '= true' in l) for l in lock_text.splitlines()):
        raise RuntimeError('Screen is locked')
    if not state.get('frontmost') or 'loginwindow' in state['frontmost'].lower():
        raise RuntimeError('Frontmost application is loginwindow or unknown')


def overlays(state):
    def overlap(a, b):
        width = max(0, min(a['X'] + a['Width'], b['X'] + b['Width']) - max(a['X'], b['X']))
        height = max(0, min(a['Y'] + a['Height'], b['Y'] + b['Height']) - max(a['Y'], b['Y']))
        return width * height >= b['Width'] * b['Height'] * .9
    return [w for w in state.get('windows', []) if w['layer'] == 1000 and w.get('on_screen')
            and any(overlap(w['bounds'], d) for d in state.get('displays', []))]


def restoration_check(initial, actual):
    distance = math.dist(initial['pointer'], actual['pointer'])
    return {'ok': initial['front_pid'] == actual['front_pid'] and distance <= 1,
            'frontmost_restored': initial['front_pid'] == actual['front_pid'],
            'pointer_distance_points': distance}


def browser_window(before, after, app, claimed, standard=None):
    old = {w['id'] for w in before['windows']}
    pids = {a['pid'] for a in after['apps'] if a['bundle'] == app}
    # Every app owns off-screen layer-0 helper windows; only a visible one is the task window.
    # Visible status bubbles and popups are not standard AX windows.
    owned = [w['id'] for w in after['windows'] if w['id'] not in old and w['pid'] in pids
             and w['layer'] == 0 and w.get('on_screen') and (standard is None or w['id'] in standard)]
    if after['frontmost'] != app or owned != [claimed]:
        raise RuntimeError('Browser setup did not prove a single new task window; refusing prompt submission')
    return claimed


class Native:
    def __init__(self, state_dir):
        self.binary = state_dir / 'native'
        source = Path(__file__).with_name('native.swift')
        r = subprocess.run(['swiftc', str(source), '-o', str(self.binary)], capture_output=True, text=True, timeout=120)
        if r.returncode:
            raise RuntimeError('Native helper compilation failed:\n' + r.stderr)

    def call(self, *args, timeout=20):
        try:
            r = subprocess.run([str(self.binary), *map(str, args)], capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f'Native {args[0]} timed out after {timeout} s') from None
        if r.returncode:
            raise RuntimeError(f'Native {args[0]} failed: {(r.stdout + r.stderr).strip()}')
        data = json.loads(r.stdout)
        if isinstance(data, dict) and data.get('error'):
            raise RuntimeError(f'Native {args[0]}: {data["error"]}')
        return data

    def preflight(self, live=False):
        state = self.call('state')
        r = subprocess.run(['ioreg', '-n', 'Root', '-d', '1', '-l'], capture_output=True, text=True, timeout=10, check=True)
        lines = [l.strip() for l in r.stdout.splitlines() if 'CGSSessionScreenIsLocked' in l]
        state['lock_check'] = lines or ['CGSSessionScreenIsLocked absent (not Yes)']
        state['gui_ready'] = True
        try:
            check_state(state, '\n'.join(lines))
        except RuntimeError as error:
            state['gui_ready'] = False
            state['gui_unavailable'] = str(error)
            if live:
                raise
        state['overlays'] = overlays(state)
        if live and not (state['ax_trusted'] and state['screen_capture']):
            raise RuntimeError('AX/screen-capture permission absent; no TCC prompt will be requested')
        return state

    def restore(self, snapshot, dry_run=False):
        if dry_run:
            return {'skipped': 'dry run made no GUI changes', 'state': self.call('state')}
        actual = self.call('restore', snapshot)
        proof = restoration_check(json.loads(snapshot.read_text()), actual)
        return {**proof, 'state': actual}

    def cleanup(self, task, before, settle_seconds=10.0):
        old_ids = {w['id'] for w in before['windows']}
        initial_pids = {a['pid'] for a in before['apps']}
        allowed = set(task['allowed_apps'])

        def owned(state):
            # Off-screen layer-0 windows are app helpers with no AX window to close.
            apps = {a['pid']: a['bundle'] for a in state['apps']}
            return [(apps[w['pid']], w['id']) for w in state['windows'] if apps.get(w['pid']) in allowed
                    and w['layer'] == 0 and w.get('on_screen') and w['id'] not in old_ids]

        def launched(state):
            return [a for a in state['apps'] if a['bundle'] in allowed and a['pid'] not in initial_pids]

        actions, transient = [], set()

        def close(state):
            for app, wid in owned(state):
                # A save sheet can appear after the discard attempt; retry a few times.
                if wid in transient or sum(a.get('window') == wid for a in actions) >= 3:
                    continue
                try:
                    result = self.call('close', app, str(wid), 'discard' if app == 'com.apple.TextEdit' else 'keep')
                    if result.get('transient'):
                        transient.add(wid)
                    actions.append(result)
                except RuntimeError as error:
                    # Apps such as Calculator quit with their last window; the final state decides.
                    actions.append({'window': wid, 'error': str(error)})

        close(self.call('state'))
        time.sleep(min(.5, settle_seconds))
        for a in launched(self.call('state')):
            try:
                actions.append(self.call('quit', a['bundle'], str(a['pid'])))
            except RuntimeError as error:
                actions.append({'pid': a['pid'], 'error': str(error)})
        deadline = time.monotonic() + settle_seconds
        while True:
            final = self.call('state')
            remaining = [wid for _, wid in owned(final) if wid not in transient]
            remaining_apps = [a['pid'] for a in launched(final)]
            if not (remaining or remaining_apps) or time.monotonic() >= deadline:
                break
            close(final)  # Windows the task opened late, e.g. while quitting.
            time.sleep(.25)
        # Only visible document-sized originals count: background apps recreate helper
        # windows, and toasts or status bubbles (22 pt tall) vanish on their own.
        regular_pids = {a['pid'] for a in before['apps']}
        original_regular = {w['id'] for w in before['windows'] if w['layer'] == 0 and w.get('on_screen') and w['pid'] in regular_pids
                            and w.get('bounds', {}).get('Width', 0) >= 120 and w.get('bounds', {}).get('Height', 0) >= 60}
        missing = sorted(original_regular - {w['id'] for w in final['windows']})
        return {'actions': actions, 'remaining_owned_windows': remaining,
                'transient_windows': sorted(transient),
                'remaining_launched_apps': remaining_apps,
                'missing_original_window_ids': missing, 'ok': not remaining and not missing and not remaining_apps}
