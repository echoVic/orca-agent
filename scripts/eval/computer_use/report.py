"""Stable JSON aggregates plus human-readable evidence and comparisons."""
from datetime import datetime
import hashlib
import json
from pathlib import Path
import statistics


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def aggregate(rows, dry_run):
    metrics = [r['metrics'] for r in rows]
    times = [m['wall_seconds'] for m in metrics if m.get('wall_seconds') is not None]
    usage = {k: sum(m.get('usage', {}).get(k, 0) for m in metrics)
             for k in ('input_tokens', 'output_tokens', 'cache_tokens', 'estimated_cost_usd')}
    a = {'schema_version': 1, 'mode': 'dry-run' if dry_run else 'live',
         'valid_protocol_baseline': not dry_run and len(rows) == 15 and all(r.get('cleanup', {}).get('ok') for r in rows),
         'single_total': sum(r['kind'] == 'single' for r in rows),
         'multi_total': sum(r['kind'] == 'multi' for r in rows),
         'single_success': None if dry_run else sum(r['kind'] == 'single' and r['passed'] for r in rows),
         'multi_outcome_success': None if dry_run else sum(r['kind'] == 'multi' and r['passed'] for r in rows),
         'strict_gui_multi_success': None if dry_run else sum(r['kind'] == 'multi' and r['passed'] for r in rows),
         'mock_smoke_passed': sum(r.get('smoke_passed', False) for r in rows) if dry_run else None,
         'median_wall_s': statistics.median(times) if times else None, 'max_wall_s': max(times, default=None),
         'median_computer_calls': statistics.median([m['computer_calls'] for m in metrics]) if metrics else None,
         'approvals': sum(m['approvals_shown'] for m in metrics), 'primary_usage': usage,
         'median_approvals_per_task': statistics.median([m['approvals_shown'] for m in metrics]) if metrics else None,
         'mean_approvals_per_task': sum(m['approvals_shown'] for m in metrics) / len(metrics) if metrics else None,
         'total_computer_calls': sum(m['computer_calls'] for m in metrics),
         'usage_missing_tasks': [r['id'] for r in rows if not r['metrics'].get('usage')],
         'session_option_missing': sum(p.get('session_option_missing', False) for m in metrics for p in m['approvals']),
         'boundary_events': sum(p.get('boundary_event', False) for m in metrics for p in m['approvals']),
         'user_window_touches': sum(len(m.get('user_window_touches', [])) for m in metrics),
         'window_panels': sum(m.get('window_panels', 0) for m in metrics),
         'tasks': {r['id']: {'passed': r['passed'], 'wall_seconds': r['metrics']['wall_seconds'],
                    'computer_calls': r['metrics']['computer_calls'], 'approvals': r['metrics']['approvals_shown'],
                    'reason': r['reason'], 'user_window_touches': r['metrics'].get('user_window_touches', [])} for r in rows}}
    for kind, success_key, total_key in [('single', 'single_success', 'single_total'), ('multi', 'multi_outcome_success', 'multi_total')]:
        a[kind + '_success_rate'] = a[success_key] / a[total_key] if not dry_run and a[total_key] else None
    return a


def fmt(value):
    return '—' if value is None else f'{value:.4f}' if isinstance(value, float) else str(value)


def write_report(out, rows, environment, baseline=None):
    a = aggregate(rows, environment['dry_run'])
    if environment.get('error') or environment.get('cleanup_errors'):
        a['valid_protocol_baseline'] = False
    dump(out / 'aggregates.json', a)
    dump(out / 'environment.json', environment)
    lines = ['# Orca computer-use acceptance', '',
             f"Mode: **{a['mode']}**. Dry runs measure mock transport only; GUI outcomes are unverified.", '',
             f"HEAD: `{environment.get('head', 'unknown')}`. Binary SHA-256: `{environment.get('binary_sha256', 'unknown')}`.",
             f"Started: {environment.get('started_at', '')}; ended: {environment.get('ended_at', '')}.", '',
             '[Environment and cleanup](environment.json). [Aggregates](aggregates.json).', '',
             '| Task | Verified | Terminal/reason | Seconds | Turns | Tools | Computer/actions | Foreground | Errors | Approvals | Tokens in/out/cache | Est. USD | Largest observation |',
             '|---|---|---|---:|---:|---:|---|---:|---:|---:|---|---:|---:|']
    for r in rows:
        m = r['metrics']; u = m['usage']; tid = r['id']
        outcome = 'unverified (mock)' if environment['dry_run'] else ('pass' if r['passed'] else 'fail')
        tokens = '/'.join(fmt(u.get(k)) for k in ('input_tokens', 'output_tokens', 'cache_tokens'))
        actions = ', '.join(f'{k}:{v}' for k, v in sorted(m['actions'].items()))
        lines.append(f"| [{tid}](evidence/{tid}/metrics.json) | {outcome} | {r['reason']} | {fmt(m['wall_seconds'])} | {m['model_turns']} | {m['tool_calls']} | {m['computer_calls']} ({actions}) | {m['foreground_marks']} | {len(m['tool_errors'])} | {m['approvals_shown']} | {tokens} | {fmt(u.get('estimated_cost_usd'))} | {m['largest_observation_chars']} |")
    lines += ['', '## Aggregates', '', '| Metric | Value |', '|---|---|']
    for k, v in a.items():
        if k != 'tasks':
            lines.append(f'| {k} | {fmt(v)} |')
    if baseline:
        old = json.loads(Path(baseline).read_text())
        lines += ['', '## Previous comparison', '', f'Previous source: `{baseline}`.', '',
                  'Warning: previous baseline is invalid or incomplete; comparisons are descriptive.' if not old.get('valid_protocol_baseline') else 'Previous baseline reports a valid protocol run.',
                  'Selected task sets and modes must match before interpreting improvements.', '',
                  '| Metric | Previous | Current | Delta |', '|---|---:|---:|---:|']
        for k in ('single_success', 'single_total', 'multi_outcome_success', 'multi_total', 'median_wall_s', 'max_wall_s', 'median_computer_calls', 'approvals'):
            prev, cur = old.get(k), a[k]
            delta = cur - prev if isinstance(cur, (int, float)) and isinstance(prev, (int, float)) else None
            lines.append(f'| {k} | {fmt(prev)} | {fmt(cur)} | {fmt(delta)} |')
        for k in ('input_tokens', 'output_tokens', 'cache_tokens', 'estimated_cost_usd'):
            prev, cur = old.get('primary_usage', {}).get(k), a['primary_usage'][k]
            lines.append(f'| {k} | {fmt(prev)} | {fmt(cur)} | {fmt(cur-prev if prev is not None else None)} |')
        lines += ['', '| Task | Previous pass | Current pass | Wall delta | Computer delta | Approval delta |', '|---|---|---|---:|---:|---:|']
        for tid, cur in a['tasks'].items():
            prev = old.get('tasks', {}).get(tid, {})
            def delta(k):
                return cur[k] - prev[k] if isinstance(cur.get(k), (int, float)) and isinstance(prev.get(k), (int, float)) else None
            lines.append(f"| {tid} | {fmt(prev.get('passed'))} | {fmt(cur['passed'])} | {fmt(delta('wall_seconds'))} | {fmt(delta('computer_calls'))} | {fmt(delta('approvals'))} |")
        dump(out / 'baseline.json', old)
    lines += ['', '## Failures, boundaries and evidence', '']
    for r in rows:
        tid = r['id']
        lines.append(f"- {tid}: {r['reason']}; layer-1000 overlays: {r.get('overlay_count', 0)}; [verification](evidence/{tid}/verification.json), [process exit and cleanup](evidence/{tid}/cleanup.json).")
        if r['metrics'].get('user_window_touches'):
            lines.append(f"  - used windows open before the task without approval: {r['metrics']['user_window_touches']}")
        for error in r['metrics']['tool_errors']:
            message = str(error['message']).replace('\n', ' ')
            lines.append(f"  - {error['tool']}: {message}")
    if environment.get('error'):
        lines += ['', f"Harness error: {environment['error']}"]
    lines += ['', '## Cleanup', '', 'Services and TUI processes are stopped before another task starts or this runner returns.',
              'Per-task cleanup records and environment.json retain process-exit, window, frontmost and pointer checks.',
              'Usage is the last persisted cumulative session.usage snapshot per task; costs are estimates, not invoices.',
              'Raw session JSONL and captures are local evidence and may contain task-app content. Review before sharing.', '']
    (out / 'report.md').write_text('\n'.join(lines))
    manifest = {str(p.relative_to(out)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in sorted(out.rglob('*')) if p.is_file() and p.name != 'manifest.json'}
    dump(out / 'manifest.json', manifest)
    return a
