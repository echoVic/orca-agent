"""Parse rendered approval boxes; never infer a grant from the model's text."""
from pathlib import Path
import re

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]


def source_titles(repo=REPO):
    # ui.rs names ordinary approvals; permission variants live on ApprovalDialog.
    ui = (repo / 'crates/orca-tui/src/ui.rs').read_text()
    types = (repo / 'crates/orca-tui/src/types.rs').read_text()
    if 'format!("Approve · {}", dialog.tool)' not in ui:
        raise ValueError('Approval title rendering changed; update the parser')
    block = types.split('pub fn title(&self)', 1)[1].split('\n    }', 1)[0]
    titles = set(re.findall(r'=>\s*"\s*([^"\n]+?)\s*"', block))
    if not titles:
        raise ValueError('No permission titles found in ApprovalDialog::title')
    return titles


def parse_panel(pane, titles=None):
    lines = pane.splitlines()
    # Only a complete, currently rendered box. Ignore transcript/scrollback text.
    starts = [i for i, line in enumerate(lines) if line.startswith('╭ ')]
    for start in reversed(starts):
        title = re.split(r'\s+─', lines[start].split('╭ ', 1)[1])[0].strip()
        end = next((i for i in range(start + 1, len(lines)) if re.match(r'^\s*╰', lines[i])), None)
        if end is None:
            continue
        body = '\n'.join(lines[start + 1:end])
        options = []
        for line in body.splitlines():
            m = re.match(r'^\s*│\s*[›>]?\s*(\d)\s{2,}(.+?)\s*│?\s*$', line)
            if m:
                label = re.split(r'\s{2,}', m[2].rstrip('│').strip())[0]
                options.append({'key': m[1], 'label': label})
        if not options or not any(o['label'] == 'Deny' for o in options):
            continue
        # Option+deny detection also recognizes future risk titles fail-safely.
        tool_match = re.search(r'(?:^|\n)\s*│\s*tool\s+(\S+)', body)
        tool = title.split('Approve · ', 1)[1] if title.startswith('Approve · ') else (tool_match[1] if tool_match else 'permissions')
        bundles = re.findall(r'(?<![\w./-])([A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+){2,}(?:#secure)?)(?![\w./-])', body)
        # App identity comes from preview or the standalone canonical target.
        target = next((b for b in bundles if b.startswith(('com.', 'org.', 'net.', 'dev.', 'io.'))), None)
        return {'title': title, 'tool': tool, 'target': target, 'options': options,
                'known_title': title.startswith('Approve · ') or title in (titles if titles is not None else source_titles())}
    return None


def decide(panel, allowed_apps, target=None):
    target = target or panel['target']
    options = {o['label']: o['key'] for o in panel['options']}
    boundary = False
    missing = False
    if panel['tool'] == 'computer':
        if not target or target.split('#', 1)[0] not in allowed_apps:
            chosen, boundary = 'Deny', True
        elif target.endswith('#secure') or 'Allow typing this once' in options:
            chosen = 'Allow typing this once' if 'Allow typing this once' in options else 'Allow once'
        elif 'Allow this app this session' in options:
            chosen = 'Allow this app this session'
        else:
            chosen, missing = 'Allow once', True
    else:
        chosen = 'Allow once'
    if chosen not in options:
        chosen, boundary = 'Deny', True
    return {'chosen': chosen, 'key': options[chosen], 'target': target,
            'boundary_event': boundary, 'session_option_missing': missing}


def answer_panel(panel, request, allowed_apps):
    decision = decide(panel, allowed_apps, request['target'])
    if panel['target'] and request['target'] and panel['target'].split('#')[0] != request['target'].split('#')[0]:
        decision.update(chosen='Deny', key=next(o['key'] for o in panel['options'] if o['label'] == 'Deny'), boundary_event=True)
    return decision


def detector_self_test():
    results = []
    for path in sorted((HERE / 'testdata').glob('*.txt')):
        p = parse_panel(path.read_text())
        if p is None:
            raise RuntimeError(f'Approval detector failed: {path.name}')
        results.append({'capture': path.name, **p})
    return results
