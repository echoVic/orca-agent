"""tmux-owned service lifetimes, independent of the invoking shell."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import html
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time
from urllib.parse import parse_qs
from urllib.request import urlopen

SOCKET = 'orca-cu-eval'


class Tmux:
    def __init__(self):
        self.owned = False

    def call(self, *args, check=True):
        r = subprocess.run(['tmux', '-L', SOCKET, '-f', '/dev/null', *map(str, args)],
                           capture_output=True, text=True, timeout=10)
        if check and r.returncode:
            raise RuntimeError(f'tmux {args[0]} failed: {r.stderr.strip()}')
        return r.stdout.strip()

    def start(self):
        if self.call('list-sessions', check=False):
            raise RuntimeError(f'tmux -L {SOCKET} is already in use; leave it intact')
        self.call('new-session', '-d', '-s', 'eval', '-n', 'control', '-x', '220', '-y', '60', 'exec cat')
        self.owned = True
        self.call('set-option', '-g', 'remain-on-exit', 'on')
        self.call('set-option', '-g', 'history-limit', '5000')

    def window(self, name, argv):
        self.call('new-window', '-d', '-t', 'eval:', '-n', name, 'exec ' + shlex.join(map(str, argv)))
        return f'eval:{name}.0'

    def alive(self, pane):
        return self.call('display-message', '-p', '-t', pane, '#{pane_dead}', check=False) == '0'

    def stop(self):
        if self.owned:
            self.call('kill-server')
            self.owned = False


class Services:
    def __init__(self, tmux, base):
        self.tmux, self.base, self.port = tmux, base, None
        self.panes = []
        self.pids = []

    def start(self):
        port_file = self.base / 'state/port.json'
        self.panes.append(self.tmux.window('http', [sys.executable, '-I', Path(__file__).resolve(), '--serve', self.base, port_file]))
        self.panes.append(self.tmux.window('awake', ['caffeinate', '-dimsu']))
        self.pids = [int(self.tmux.call('display-message', '-p', '-t', p, '#{pane_pid}')) for p in self.panes]
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if not all(self.tmux.alive(p) for p in self.panes):
                raise RuntimeError('A service exited during startup')
            if port_file.exists():
                self.port = json.loads(port_file.read_text())['port']
                self.health()
                return
            time.sleep(.1)
        raise RuntimeError('HTTP server did not become healthy')

    def health(self):
        if not all(self.tmux.alive(p) for p in self.panes):
            raise RuntimeError('HTTP/caffeinate tmux window exited')
        with urlopen(f'http://127.0.0.1:{self.port}/health', timeout=2) as r:
            if r.status != 200 or r.read() != b'orca-cu-eval':
                raise RuntimeError('HTTP health check failed')
        return {'http': 200, 'port': self.port, 'caffeinate': True, 'panes': self.panes, 'pids': self.pids}

    def stop(self):
        for pane in self.panes:
            self.tmux.call('kill-window', '-t', pane.rsplit('.', 1)[0], check=False)
        self.panes.clear()
        deadline = time.monotonic() + 5
        remaining = list(self.pids)
        while remaining and time.monotonic() < deadline:
            for pid in list(remaining):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    remaining.remove(pid)
            if remaining:
                time.sleep(.05)
        if remaining:
            raise RuntimeError(f'Service processes did not exit: {remaining}')
        return {'stopped_pids': self.pids, 'ok': True}


class FormHandler(BaseHTTPRequestHandler):
    base = None

    def log_message(self, *args):
        pass

    def respond(self, body, content_type='text/html; charset=utf-8', status=200):
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == '/health':
            self.respond(b'orca-cu-eval', 'text/plain')
        elif self.path in ('/page.html', '/form.html'):
            self.respond((self.base / 'fixtures' / self.path[1:]).read_bytes())
        else:
            self.respond(b'Not found', status=404)

    def do_POST(self):
        try:
            size = int(self.headers.get('Content-Length', '0'))
        except ValueError:
            size = -1
        if self.path != '/form.html' or not 0 <= size <= 4096:
            self.respond(b'Invalid form request', status=400)
            return
        body = self.rfile.read(size).decode('utf-8', errors='replace')
        with (self.base / 'out/form-posts.log').open('a') as f:
            f.write(body.replace('\n', '%0A').replace('\r', '%0D') + '\n')
        name = parse_qs(body).get('name', [''])[0]
        self.respond(('<!doctype html><meta charset="utf-8"><h1>Thanks, ' + html.escape(name) + '</h1>').encode())


def serve(base, port_file):
    FormHandler.base = base
    server = ThreadingHTTPServer(('127.0.0.1', 0), FormHandler)
    tmp = port_file.with_suffix('.tmp')
    tmp.write_text(json.dumps({'port': server.server_port, 'pid': os.getpid()}))
    tmp.replace(port_file)
    server.serve_forever()


if __name__ == '__main__':
    if len(sys.argv) != 4 or sys.argv[1] != '--serve':
        raise SystemExit('Internal form server; invoke run.py instead')
    serve(Path(sys.argv[2]), Path(sys.argv[3]))
