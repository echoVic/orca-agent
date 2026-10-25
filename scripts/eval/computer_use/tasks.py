"""Validate the fixed acceptance set before starting any services."""
import json
from pathlib import Path
import re

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]


def load_tasks(path=HERE / 'tasks.json'):
    from verify import VERIFIERS
    tasks = json.loads(Path(path).read_text())
    expected = [f'S{i}' for i in range(1, 9)] + [f'M{i}' for i in range(1, 8)]
    if not isinstance(tasks, list) or [t.get('id') for t in tasks] != expected:
        raise ValueError('Expected exactly S1–S8, M1–M7 in order')
    for t in tasks:
        if t.get('existing_windows', 'deny') not in ('approve', 'deny'):
            raise ValueError(f'Invalid existing_windows: {t.get("id")}')
        if (t.get('kind') != ('single' if t['id'][0] == 'S' else 'multi')
                or not isinstance(t.get('prompt'), str) or not t['prompt']
                or type(t.get('timeout')) is not int or not 0 < t['timeout'] <= 360
                or t.get('verifier') not in VERIFIERS
                or not isinstance(t.get('allowed_apps'), list) or not t['allowed_apps']
                or any(not isinstance(a, str) or not re.fullmatch(r'[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+', a) for a in t['allowed_apps'])):
            raise ValueError(f'Invalid task: {t.get("id")}')
        if re.findall(r'\{[^}]*\}', t['prompt']) != (['{port}'] if t['id'] == 'M2' else []):
            raise ValueError('Only M2 may contain the {port} placeholder')
    return tasks
