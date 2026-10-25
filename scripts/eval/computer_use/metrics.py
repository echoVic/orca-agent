"""Persisted session metrics. Cumulative usage snapshots are never summed."""
from collections import Counter
from datetime import datetime
import json
import re

from approvals import WINDOW


def read_jsonl(path):
    data = path.read_bytes()
    lines = data.splitlines()
    result = []
    for i, line in enumerate(lines):
        try:
            result.append(json.loads(line.decode('utf-8')))
        except UnicodeDecodeError as error:
            if i == len(lines) - 1 and not data.endswith(b'\n') and error.reason == 'unexpected end of data':
                break
            raise ValueError(f'Invalid UTF-8 record {i+1} in {path.name}') from None
        except json.JSONDecodeError:
            if i == len(lines) - 1 and not data.endswith(b'\n'):
                break
            raise ValueError(f'Corrupt JSONL record {i+1} in {path.name}') from None
    return result


def semantic(records):
    return [r['event'] for r in records if r.get('type') == 'event.semantic']


def terminal(records):
    # The dedicated record's timestamp precedes its semantic mirror slightly.
    for r in reversed(records):
        if r.get('type') == 'session.completed':
            return {'status': r['status'], 'at': r.get('completed_at'), 'source': 'session.completed'}
    for r in reversed(records):
        e = r.get('event', {})
        if r.get('type') == 'event.semantic' and e.get('type') == 'session.completed':
            p = e['payload']
            status = next(iter(p.get('terminal', {})), p.get('status'))
            return {'status': status, 'timestamp': e.get('timestamp_ms', 0) / 1000, 'source': 'semantic.terminal'}
    committed = {r['commit_id'] for r in records if r.get('type') == 'runtime.surface_commit.committed'}
    for r in reversed(records):
        if r.get('type') == 'runtime.surface_commit.prepared' and r['commit_id'] in committed:
            for envelope in reversed(r['batch']['events']):
                record = envelope.get('event', {}).get('Operation', {}).get('Terminal', {}).get('record')
                if record:
                    kind = next(iter(record['terminal']))
                    return {'status': 'completed' if kind == 'Succeeded' else kind.lower(), 'source': 'committed.operation.terminal'}
    return None


def arguments(value):
    if isinstance(value, dict):
        return value
    try:
        obj = json.loads(value or '{}')
        return obj if isinstance(obj, dict) else {}
    except (ValueError, TypeError):
        return {}


def extract(records, approvals, submitted, ended=None):
    events = semantic(records)
    responses = [e for e in events if e['type'] == 'model.response.completed']
    calls = {}
    for e in events:
        if e['type'] == 'model.response.completed':
            for c in e['payload'].get('tool_calls', []):
                calls.setdefault(c['id'], {'id': c['id'], 'name': c['function_name'], 'arguments': arguments(c.get('arguments'))})
        elif e['type'] == 'tool.call.requested':
            p = e['payload']
            calls.setdefault(p['id'], {'id': p['id'], 'name': p['name'], 'arguments': arguments(p.get('raw_arguments'))})
            calls[p['id']]['target'] = p.get('target')
    results = {e['payload']['id']: e['payload'] for e in events if e['type'] == 'tool.call.completed'}
    computer = [c for c in calls.values() if c['name'] == 'computer']
    errors, observations = [], []
    foreground = 0
    latest_observed = {}
    for c in calls.values():
        r = results.get(c['id'], {})
        c['status'] = r.get('status')
        output = r.get('output') or ''
        if c['name'] == 'computer':
            # App listings carry raw titles; only an observation header proves its window.
            seen = None
            if c['status'] == 'completed' and c['arguments'].get('action') not in ('list_apps', 'launch_app'):
                seen = re.search(r'(?m)^[^"\n]*"(?:[^"\\\n]|\\.)*" \[window (\d+)\]: ', output)
            c['requested_window'] = c['arguments'].get('window')
            c['observed_window'] = int(seen[1]) if seen else None
            bundle = (c.get('target') or '').split('#', 1)[0]
            c['inherited_window'] = None
            if (c['arguments'].get('action') in ('click', 'type', 'key', 'scroll')
                    and c['requested_window'] is None and c['observed_window'] is None):
                c['inherited_window'] = latest_observed.get(bundle)
            c['window'] = next((c[field] for field in ('observed_window', 'requested_window', 'inherited_window')
                                if c[field] is not None), None)
            if bundle and c['observed_window'] is not None:
                latest_observed[bundle] = c['observed_window']
        foreground += int('used the foreground' in output)
        if c['name'] == 'computer' and ('screenshot ' in output or c['arguments'].get('action') == 'observe'):
            observations.append(len(output))
        if r.get('error') or r.get('status') not in (None, 'completed'):
            errors.append({'id': c['id'], 'tool': c['name'], 'action': c['arguments'].get('action'),
                           'message': r.get('error') or output or r.get('status')})
    usage = next(({k: v for k, v in r.items() if k != 'type'} for r in reversed(records) if r.get('type') == 'session.usage'), {})
    end = terminal(records)
    if end:
        ended = datetime.fromisoformat(end['at'].replace('Z', '+00:00')).timestamp() if end.get('at') else end.get('timestamp', ended)
    return {'wall_seconds': max(0, ended - submitted) if ended is not None and submitted is not None else None,
            'model_turns': len(responses), 'model_turns_started': sum(e['type'] == 'turn.started' for e in events),
            'tool_calls': len(calls), 'computer_calls': len(computer),
            'actions': dict(Counter(c['arguments'].get('action', 'unknown') for c in computer)),
            'foreground_marks': foreground, 'tool_errors': errors, 'approvals': approvals,
            'approvals_shown': len(approvals), 'usage': usage,
            'largest_observation_chars': max(observations, default=0), 'terminal': end,
            'calls': list(calls.values()),
            'final_answer': next((e['payload'].get('assistant_content') or '' for e in reversed(responses)
                                  if not e['payload'].get('tool_calls')), ''),
            'routing': sorted({e['payload']['actual_model'] for e in events if e['type'] == 'model.routed'})}


def _call_windows(call):
    # Read older metrics too; new records derive `window` from these three IDs.
    return {call.get(field) for field in ('requested_window', 'observed_window', 'inherited_window', 'window')} - {None}


def user_window_touches(calls, preexisting, approvals):
    """Every pre-existing window used by a completed call without a covering grant."""
    order = {c['id']: index for index, c in enumerate(calls) if c.get('id')}
    grants = []
    for approval in approvals:
        match = WINDOW.search(approval.get('target') or '')
        answered = order.get(approval.get('call_id'))
        if match and answered is not None and approval.get('chosen') not in (None, 'Deny'):
            grants.append((int(match[1]), answered, approval['chosen'] == 'Allow this window this session'))
    touched = set()
    for index, call in enumerate(calls):
        if call['name'] != 'computer' or call.get('status') != 'completed':
            continue
        for window in _call_windows(call) & set(preexisting):
            if not any(granted == window and (answered == index or session and answered < index)
                       for granted, answered, session in grants):
                touched.add(window)
    return sorted(touched)


def unattributed_computer_calls(calls):
    """Completed window operations whose destination the transcript cannot establish."""
    return sum(c['name'] == 'computer' and c.get('status') == 'completed'
               and c.get('arguments', {}).get('action') not in ('list_apps', 'launch_app')
               and not _call_windows(c) for c in calls)
