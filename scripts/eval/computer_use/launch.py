"""Exec wrapper: credentials never enter tmux commands, files or evidence."""
import json
import os
from pathlib import Path
import sys


def environment(base, provider):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('ORCA_', 'DEEPSEEK_', 'OPENAI_', 'ANTHROPIC_')) and k != 'NODE_OPTIONS'}
    env.update(ORCA_HOME=str(base / 'home'), TERM='xterm-256color')
    if provider == 'mock':
        env['DEEPSEEK_API_KEY'] = 'mock-no-network'
    elif provider == 'deepseek':
        try:
            key = json.loads((Path.home() / '.orca/auth.json').read_text())['DEEPSEEK_API_KEY']
            if not isinstance(key, str) or not key:
                raise ValueError()
        except (OSError, ValueError, KeyError):
            raise RuntimeError('Cannot read DEEPSEEK_API_KEY from read-only ~/.orca/auth.json') from None
        env['DEEPSEEK_API_KEY'] = key
    else:
        raise ValueError('Unsupported harness provider')
    return env


def main():
    binary, base, provider, pid_file = sys.argv[1:]
    base = Path(base)
    env = environment(base, provider)
    Path(pid_file).write_text(json.dumps({'pid': os.getpid(), 'provider': provider}))
    os.chdir(base / 'ws')
    os.execve(binary, [binary, '--provider', provider, '--model', 'auto', '--mode', 'auto-edit', '--cwd', str(base / 'ws')], env)


if __name__ == '__main__':
    main()
