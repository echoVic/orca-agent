"""Independent verifiers. Screenshots supplement AX/files, never model claims."""
import json
from pathlib import Path
import re
import subprocess
import unicodedata
from urllib.parse import parse_qs

from fixtures import HEADING, BUTTON, DRAFT


def visible(text):
    """Drop format characters such as the LRM marks Calculator wraps around values."""
    return ''.join(ch for ch in text if unicodedata.category(ch) != 'Cf')


def ax_values(ax):
    if isinstance(ax, str):
        return [visible(ax)]
    if isinstance(ax, list):
        return [v for item in ax for v in ax_values(item)]
    if isinstance(ax, dict):
        return [visible(str(v)) for k, v in ax.items() if k in ('AXTitle', 'AXValue', 'AXDescription')] + ax_values(ax.get('elements', []))
    return []


def contains(data, value):
    # OCR of the task window only reports which expected strings it saw.
    return any(value in s for s in ax_values(data.get('ax', []))) or value in data.get('ocr_found', [])


def number(data, expected):
    return any(re.sub(r'[,，\s]', '', s) == str(expected) for s in ax_values(data.get('ax', [])))


def calculator(d): return number(d, 7006652)
def heading(d): return contains(d, HEADING)
def last_file(d): return contains(d, 'file-120.txt')
def textedit(d): return contains(d, 'Hello from Orca baseline')
def pdf(d): return d.get('pdf_pages') == 2 and any(re.search(r'1\s*[/／]\s*2|(?:2\s*页|2 pages)', v) for v in ax_values(d.get('ax', [])))
def button(d): return contains(d, BUTTON)
def arithmetic(d): return number(d, 368)
def chip(d): return bool(d.get('chip')) and contains(d, d['chip'])
def counter(d): return bool(d.get('counter')) and number(d, 3)
def draft(d): return d.get('draft') == DRAFT.replace('teh', 'the', 1)


def appearance(d):
    mode = 'auto' if d.get('appearance_auto') in ('1', 'true', 'YES') else ('dark' if d.get('appearance_style') == 'Dark' else 'light')
    labels = {'auto': ('自动', 'Auto'), 'dark': ('深色', 'Dark'), 'light': ('浅色', 'Light')}[mode]
    nodes = d.get('ax', [])
    return any(n.get('role') in ('AXRadioButton', 'AXCheckBox', 'AXButton')
               and (str(n.get('AXValue')) == '1' or n.get('AXSelected') is True)
               and any(label in value for label in labels for value in ax_values(n)) for n in nodes)


def sidebar(d):
    if any(d.get('actions', {}).get(a, 0) for a in ('click', 'type', 'key', 'scroll')):
        return False
    # Native collection is buttons-only; preserve the independent list in evidence.
    values = set(ax_values(d.get('ax', [])))
    return len(values) >= 2 and any(v in values for v in ('首页', 'Home', '助手', 'Assistants'))


def todo(d):
    text = d.get('todo', '')
    return not text.lstrip().startswith(('{\\rtf', '<?xml', '<html')) and len([s for s in text.splitlines() if s.strip()]) == 3


def form(d):
    valid = any(parse_qs(p).get('name') == ['Orca'] and parse_qs(p).get('agree') == ['yes'] for p in d.get('posts', []))
    return valid and contains(d, 'Thanks, Orca')


def rename(d):
    return 'reports-2026' in d.get('out_dirs', []) and 'reports' not in d.get('out_entries', [])


VERIFIERS = {f.__name__: f for f in (calculator, heading, last_file, appearance, textedit, pdf, sidebar, button, todo, form, rename, arithmetic, chip, counter, draft)}


def evaluate(name, data):
    passed = bool(VERIFIERS[name](data))
    if 'answer' in data:
        passed &= answer_matches(name, data)
    return {'passed': passed, 'verifier': name, 'evidence': data}


def answer_matches(name, data):
    answer = visible(data['answer'])
    expected = {'heading': HEADING, 'button': BUTTON, 'last_file': 'file-120.txt', 'chip': data.get('chip', ''), 'form': 'Thanks, Orca'}
    if name in expected:
        return bool(expected[name]) and expected[name] in answer
    if name in ('calculator', 'arithmetic', 'counter'):
        n = {'calculator': '7006652', 'arithmetic': '368', 'counter': '3'}[name]
        return bool(re.search(r'(?<!\d)' + n + r'(?!\d)', re.sub(r'[,，]', '', answer)))
    if name == 'pdf':
        return bool(re.search(r'2\s*(?:页|pages)|(?:两|二)页', answer, re.I))
    if name == 'appearance':
        words = ('自动', 'Auto') if data.get('appearance_auto') in ('1', 'true', 'YES') else (('深色', 'Dark') if data.get('appearance_style') == 'Dark' else ('浅色', 'Light'))
        return any(w.lower() in answer.lower() for w in words)
    if name == 'sidebar':
        names = set(ax_values(data.get('ax', []))) & {'首页', '智能体', '助手', '话题', '知识库', '文件', '翻译', '绘画', '设置', 'Home', 'Agents', 'Assistants', 'Topics', 'Knowledge', 'Files', 'Settings'}
        return len(names) >= 2 and all(n in answer for n in names)
    return True


def wrote_outside_gui(metrics, path):
    """Whether a non-GUI call wrote `path`; read-only checks such as `cat` or `file` are fine."""
    name = re.escape(Path(path).name)
    writes = [rf'>>?\s*["\']?[^\s"\'|;&]*{name}', rf'\btee\b[^|;&]*{name}', rf'\b(cp|mv|install|ln|touch|truncate)\b[^|;&]*{name}',
              rf'\bsed\s+-[a-zA-Z]*i[^|;&]*{name}', rf'\bperl\s+-[a-zA-Z]*i[^|;&]*{name}',
              rf'(write_text|write_bytes)', rf'open\([^)]*{name}[^)]*["\'][wax+]']
    for call in metrics['calls']:
        if Path(path).name not in json.dumps(call['arguments'], ensure_ascii=False):
            continue
        # File tools write directly; other tools (permission requests, reads) only name the file.
        if call['name'] in ('write_file', 'edit'):
            return True
        if call['name'] == 'bash' and any(re.search(w, call['arguments'].get('command', '')) for w in writes):
            return True
    return False


def gui_coverage(task, metrics, data):
    app = task['allowed_apps'][0]
    calls = [c for c in metrics['calls'] if c['name'] == 'computer' and c.get('status') == 'completed'
             and (c.get('target') or '').split('#')[0] == app
             and (c['arguments'].get('window') is None or c['arguments']['window'] in data['windows'])]
    actions = {c['arguments'].get('action') for c in calls}
    required = {'S5': {'type'}, 'S8': {'click'}, 'M1': {'type'}, 'M2': {'click', 'type'},
                'M3': {'type'}, 'M6': {'click'}, 'M7': {'type'}}.get(task['id'], {'observe'})
    if not required <= actions:
        return False
    # Calculator restores its last display; reading a stale result is not a computation.
    if task['id'] in ('S1', 'M4') and not actions & {'click', 'type', 'key'}:
        return False
    if task['id'] in ('M1', 'M7'):
        text = data.get('todo' if task['id'] == 'M1' else 'draft', '')
        if not text or not any(text.strip() == s.strip() for s in ax_values(data.get('ax', []))):
            return False
        # A direct file/shell mutation cannot substitute for the requested GUI edit.
        if wrote_outside_gui(metrics, 'todo.txt' if task['id'] == 'M1' else 'draft.txt'):
            return False
        if task['id'] == 'M1':
            # The saved lines must have been typed into the task window.
            typed = '\n'.join(c['arguments'].get('text', '') for c in calls if c['arguments'].get('action') == 'type')
            if not all(line.strip() in typed for line in text.splitlines() if line.strip()):
                return False
    if task['id'] == 'M6':
        return sum(c['arguments'].get('count', 1) for c in calls if c['arguments'].get('action') == 'click') >= 3
    return True


def command_value(args, missing_ok=False):
    r = subprocess.run(args, capture_output=True, text=True, timeout=10)
    if r.returncode and not missing_ok:
        raise RuntimeError(f'Independent check failed: {args[0]}')
    return r.stdout.strip() if r.returncode == 0 else ''


def collect(task, native, base, before, evidence, metrics):
    """Read only task app windows created after its snapshot (two read-only exceptions)."""
    app = task['allowed_apps'][0]
    state = native.call('state')
    original = {w['id'] for w in before['windows']}
    pids = {a['pid'] for a in state['apps'] if a['bundle'] == app}
    # Off-screen layer-0 windows are helpers without an AX window to read.
    windows = [w for w in state['windows'] if w['pid'] in pids and w['layer'] == 0 and w.get('on_screen')
               and (w['id'] not in original or task['id'] in ('S4', 'S7', 'M5', 'S1', 'M4'))]
    data = {'ax': [], 'windows': [w['id'] for w in windows], 'actions': metrics['actions'], 'answer': metrics['final_answer']}
    for w in windows:
        op = 'buttons' if task['id'] == 'S7' else ('settings' if app == 'com.apple.systempreferences' else 'ax')
        try:
            ax = native.call(op, app, str(w['id']))
        except RuntimeError as error:
            # Panels and popups are visible windows with no AX window of their
            # own; they hold no task evidence, so skip them rather than abort.
            if 'Expected exactly one matching AX window' not in str(error):
                raise
            data.setdefault('skipped_windows', []).append(w['id'])
            continue
        data['ax'].extend(ax)
        # No screenshots of pre-existing user windows. AX exceptions above are scoped.
        if w['id'] not in original and task['id'] != 'S7' and app != 'com.apple.systempreferences':
            shot = evidence / f'window-{w["id"]}.png'
            command_value(['screencapture', '-x', '-l', str(w['id']), str(shot)])
            if task['verifier'] in ('heading', 'button'):
                needle = HEADING if task['verifier'] == 'heading' else BUTTON
                data.setdefault('ocr_found', []).extend(native.call('ocr-find', str(shot), needle)['found'])
    paths = {'todo': 'out/todo.txt', 'draft': 'fixtures/draft.txt', 'counter': 'ws/counter.html'}
    for key, rel in paths.items():
        p = base / rel
        if p.is_file() and not p.is_symlink():
            try:
                data[key] = p.read_text(encoding='utf-8')
            except UnicodeDecodeError:
                data[key] = ''
    data['out_entries'] = sorted(p.name for p in (base / 'out').iterdir())
    data['out_dirs'] = sorted(p.name for p in (base / 'out').iterdir() if p.is_dir() and not p.is_symlink())
    log = base / 'out/form-posts.log'
    data['posts'] = log.read_text().splitlines() if log.exists() else []
    if task['verifier'] == 'pdf':
        data['pdf_pages'] = native.call('pdf-count', str(base / 'fixtures/two-pages.pdf'))['pages']
    if task['verifier'] == 'appearance':
        data['appearance_style'] = command_value(['defaults', 'read', '-g', 'AppleInterfaceStyle'], True)
        data['appearance_auto'] = command_value(['defaults', 'read', '-g', 'AppleInterfaceStyleSwitchesAutomatically'], True)
    if task['verifier'] == 'chip':
        data['chip'] = command_value(['sysctl', '-n', 'machdep.cpu.brand_string'])
    (evidence / 'verification-input.json').write_text(json.dumps(data, ensure_ascii=False, indent=2))
    result = evaluate(task['verifier'], data)
    # File-only substitutions (especially M7) cannot pass GUI acceptance.
    gui = gui_coverage(task, metrics, data)
    result['gui_evidence'] = gui
    result['passed'] &= gui
    return result
