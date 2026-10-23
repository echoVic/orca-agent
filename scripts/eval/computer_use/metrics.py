"""Persisted session metrics. Cumulative usage snapshots are never summed."""
from collections import Counter
from datetime import datetime
import json


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
    for e in responses:
        for c in e['payload'].get('tool_calls', []):
            calls[c['id']] = {'id': c['id'], 'name': c['function_name'], 'arguments': arguments(c.get('arguments'))}
    for e in events:
        if e['type'] == 'tool.call.requested':
            p = e['payload']
            calls.setdefault(p['id'], {'id': p['id'], 'name': p['name'], 'arguments': arguments(p.get('raw_arguments'))})
            calls[p['id']]['target'] = p.get('target')
    results = {e['payload']['id']: e['payload'] for e in events if e['type'] == 'tool.call.completed'}
    computer = [c for c in calls.values() if c['name'] == 'computer']
    errors, observations = [], []
    foreground = 0
    for c in calls.values():
        r = results.get(c['id'], {})
        c['status'] = r.get('status')
        output = r.get('output') or ''
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
