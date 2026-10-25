"""A blocking, single-task TUI driver. No scheduler or detached monitor."""
import json
import os
from pathlib import Path
import signal
import sys
import time

from approvals import parse_panel, answer_panel, source_titles
from metrics import read_jsonl, semantic, terminal, arguments

HERE = Path(__file__).resolve().parent
MOCK_PROMPT = 'orca-cu-eval mock smoke: reply only; no tools'


def bind_session(root, previous, prompt, provider):
    matches = []
    for path in root.rglob('*.jsonl'):
        if path in previous:
            continue
        records = read_jsonl(path)
        if not records:
            continue
        meta = records[0]
        users = [r['message'].get('content') for r in records if r.get('type') == 'conversation.message'
                 and r['message'].get('role') == 'user']
        if meta.get('type') == 'session.meta' and users and users[0] == prompt and meta.get('provider') == provider:
            matches.append(path)
    if len(matches) > 1:
        raise RuntimeError('Ambiguous fresh session identity; refusing latest-file matching')
    return matches[0] if matches else None


def pending_approvals(records):
    events = semantic(records)
    legacy = {e['payload']['id']: e['payload'] for e in events if e['type'] == 'approval.requested'}
    resolved = {e['payload']['id'] for e in events if e['type'] == 'approval.resolved'}
    committed = {r['commit_id'] for r in records if r.get('type') == 'runtime.surface_commit.committed'}
    active, typed_calls = {}, set()
    for row in records:
        if row.get('type') != 'runtime.surface_commit.prepared' or row['commit_id'] not in committed:
            continue
        for envelope in row['batch']['events']:
            interaction = envelope.get('event', {}).get('Interaction', {})
            if 'Requested' in interaction:
                i = interaction['Requested']['interaction']
                request = i['request']
                if 'ToolApproval' in request:
                    req = request['ToolApproval']; tool = req['tool']
                    call = tool['tool_call_id']; typed_calls.add(call)
                    old = legacy.get('approval-' + call, {})
                    active[i['interaction_id']] = {'id': i['interaction_id'], 'call_id': call,
                        'tool': tool['name'], 'target': tool.get('target') or old.get('target'),
                        'arguments': arguments(tool.get('raw_arguments')), 'preview': req.get('preview', '')}
                elif 'PermissionRequest' in request:
                    req = request['PermissionRequest']
                    active[i['interaction_id']] = {'id': i['interaction_id'], 'call_id': req['tool_call_id'],
                        'tool': 'permissions', 'target': None, 'arguments': {}, 'preview': req.get('reason', '')}
            for name in ('Resolved', 'Expired', 'Cancelled'):
                if name in interaction:
                    active.pop(interaction[name]['interaction_id'], None)
    # Older transcripts without typed interactions remain readable.
    for ident, p in legacy.items():
        call = ident.removeprefix('approval-')
        if call not in typed_calls and ident not in resolved:
            active[ident] = {'id': ident, 'call_id': call, 'tool': p['tool'],
                             'target': p.get('target'), 'arguments': {}, 'preview': p.get('preview', '')}
    return list(active.values())


class TUI:
    def __init__(self, tmux, binary, base, evidence, provider):
        self.tmux, self.binary, self.base, self.evidence, self.provider = tmux, binary, base, evidence, provider
        self.pane, self.pid, self.session = None, None, None
        self.previous = set((base / 'home/sessions').rglob('*.jsonl'))
        self.approvals = []
        self.submitted = None
        self.titles = source_titles()

    def capture(self):
        return self.tmux.call('capture-pane', '-p', '-t', self.pane)

    def key(self, value, literal=False):
        args = ['send-keys', '-t', self.pane]
        if literal:
            args.append('-l')
        self.tmux.call(*args, value)

    def start(self):
        pid_file = self.evidence / 'process.json'
        self.pane = self.tmux.window('orca', [sys.executable, '-I', HERE / 'launch.py', self.binary, self.base, self.provider, pid_file])
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if pid_file.exists():
                self.pid = json.loads(pid_file.read_text())['pid']
            pane = self.capture()
            if not self.tmux.alive(self.pane):
                (self.evidence / 'startup-failure.txt').write_text(pane)
                raise RuntimeError('Orca exited during startup; see startup-failure.txt')
            if 'Message Orca' in pane:
                (self.evidence / 'ready.txt').write_text(pane)
                return
            if '╭ Welcome' in pane:
                # Trusted temp workspace is set up beforehand, no global trust changes.
                self.key('Enter')
            time.sleep(.15)
        raise RuntimeError('Orca TUI did not become ready')

    def run(self, task, prompt, health, task_windows=()):
        self.submitted = time.time()
        (self.evidence / 'submission.json').write_text(json.dumps({'time': self.submitted, 'prompt': prompt, 'provider': self.provider}))
        self.key(prompt, literal=True)
        self.key('Enter')
        deadline = time.monotonic() + (min(task['timeout'], 45) if self.provider == 'mock' else task['timeout'])
        answered, last, captures = set(), '', 0
        next_health = 0
        while time.monotonic() < deadline:
            if time.monotonic() >= next_health:
                health(); next_health = time.monotonic() + 2
            if self.session is None:
                self.session = bind_session(self.base / 'home/sessions', self.previous, prompt, self.provider)
                if self.session:
                    meta = read_jsonl(self.session)[0]
                    (self.evidence / 'session-identity.json').write_text(json.dumps({'path': str(self.session), 'session_id': meta['session_id'], 'pid': self.pid}))
            records = read_jsonl(self.session) if self.session else []
            pane = self.capture()
            if pane != last:
                captures += 1; last = pane
                (self.evidence / f'capture-{captures:04}.txt').write_text(pane)
            panel = parse_panel(pane, self.titles)
            pending = [p for p in pending_approvals(records) if p['id'] not in answered]
            if panel and pending:
                req = next((r for r in pending if (panel['tool'] == r['tool']
                    or r['tool'] == 'permissions' and panel['tool'] in ('permissions', r['call_id'], 'request_permissions'))), None)
                if req:
                    # Canonical semantic target repairs the known null surface target.
                    decision = answer_panel(panel, req, task['allowed_apps'], task_windows)
                    record = {'time': time.time(), **panel, **decision, 'interaction_id': req['id'], 'call_id': req['call_id']}
                    self.approvals.append(record)
                    (self.evidence / f'approval-{len(self.approvals):03}.txt').write_text(pane)
                    with (self.evidence / 'approvals.jsonl').open('a') as f:
                        f.write(json.dumps(record, ensure_ascii=False) + '\n')
                    answered.add(req['id']); self.key(decision['key'])
                    if decision['boundary_event']:
                        self.key('Escape')
                        return 'boundary_denied'
            end = terminal(records)
            if end:
                return end['status']
            if not self.tmux.alive(self.pane):
                return 'process_exited_without_terminal'
            time.sleep(.1)
        self.key('Escape')
        return 'timeout'

    def stop(self):
        """Wait for process exit before returning. A caller cannot advance early."""
        if self.pane is None:
            return {'exited': True, 'started': False}
        if self.tmux.alive(self.pane):
            self.key('C-c')
            time.sleep(.2)
            self.key('C-c')
        deadline = time.monotonic() + 5
        while self.tmux.alive(self.pane) and time.monotonic() < deadline:
            time.sleep(.1)
        forced = self.tmux.alive(self.pane)
        if forced and self.pid:
            os.kill(self.pid, signal.SIGTERM)
            deadline = time.monotonic() + 5
            while self.tmux.alive(self.pane) and time.monotonic() < deadline:
                time.sleep(.1)
        if self.tmux.alive(self.pane):
            raise RuntimeError('Previous Orca process did not exit; refusing to advance')
        if self.pid:
            try:
                os.kill(self.pid, 0)
            except ProcessLookupError:
                pass
            else:
                raise RuntimeError('Orca PID remains alive after tmux pane exit')
        (self.evidence / 'final-pane.txt').write_text(self.capture())
        if self.session:
            (self.evidence / 'session.jsonl').write_bytes(self.session.read_bytes())
        records = read_jsonl(self.session) if self.session else []
        self.tmux.call('kill-window', '-t', 'eval:orca')
        self.pane = None
        return {'exited': True, 'forced_sigterm': forced, 'records': records}
