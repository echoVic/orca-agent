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


def browser_window(before, after, app, claimed):
    old = {w['id'] for w in before['windows']}
    pids = {a['pid'] for a in after['apps'] if a['bundle'] == app}
    owned = [w['id'] for w in after['windows'] if w['id'] not in old and w['pid'] in pids and w['layer'] == 0]
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

    def call(self, *args):
        r = subprocess.run([str(self.binary), *map(str, args)], capture_output=True, text=True, timeout=20)
        if r.returncode:
            raise RuntimeError(f'Native {args[0]} failed: {r.stderr.strip()}')
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

    def cleanup(self, task, before):
        state = self.call('state')
        old_ids = {w['id'] for w in before['windows']}
        initial_pids = {a['pid'] for a in before['apps']}
        apps = {a['pid']: a['bundle'] for a in state['apps']}
        actions = []
        for w in state['windows']:
            app = apps.get(w['pid'])
            if app in task['allowed_apps'] and w['layer'] == 0 and w['id'] not in old_ids:
                actions.append(self.call('close', app, str(w['id']), 'discard' if app == 'com.apple.TextEdit' else 'keep'))
        for a in state['apps']:
            if a['bundle'] in task['allowed_apps'] and a['pid'] not in initial_pids:
                actions.append(self.call('quit', a['bundle'], str(a['pid'])))
        time.sleep(.2)
        final = self.call('state')
        remaining = [w['id'] for w in final['windows'] if w['id'] not in old_ids and w['layer'] == 0
                     and any(a['pid'] == w['pid'] and a['bundle'] in task['allowed_apps'] for a in final['apps'])]
        regular_pids = {a['pid'] for a in before['apps']}
        original_regular = {w['id'] for w in before['windows'] if w['layer'] == 0 and w['pid'] in regular_pids}
        missing = sorted(original_regular - {w['id'] for w in final['windows']})
        remaining_apps = [a['pid'] for a in final['apps'] if a['bundle'] in task['allowed_apps'] and a['pid'] not in initial_pids]
        return {'actions': actions, 'remaining_owned_windows': remaining,
                'remaining_launched_apps': remaining_apps,
                'missing_original_window_ids': missing, 'ok': not remaining and not missing and not remaining_apps}
