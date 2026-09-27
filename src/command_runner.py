"""Run one MCP command in a real nested PTY; retain bounded output across HTTP restarts.

This helper runs as the terminal user, never as a privileged logging service.
It writes only its private per-command directory and does not interpret source.
"""
from __future__ import annotations
import codecs
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import sys
import termios
import time
import tty

HEAD = 65536
TAIL = 196608

class Output:
    def __init__(self):
        self.head = ''
        self.tail = ''
        self.total = 0
        self.escape = ''
        self.decoder = codecs.getincrementaldecoder('utf-8')('replace')

    def append(self, raw, final=False):
        clean = []
        for c in self.decoder.decode(raw, final=final):
            if self.escape:
                if self.escape == '\x1b':
                    self.escape = '\x1b[' if c == '[' else ('\x1b]' if c == ']' else '')
                elif self.escape == '\x1b[':
                    if '@' <= c <= '~': self.escape = ''
                elif self.escape == '\x1b]':
                    if c == '\x07': self.escape = ''
                    elif c == '\x1b': self.escape = 'osc-end'
                elif self.escape == 'osc-end':
                    self.escape = '' if c == '\\' else '\x1b]'
                continue
            if c == '\x1b': self.escape = c
            elif c != '\r' and (c >= ' ' or c in '\n\t'):
                clean.append(c)
        text = ''.join(clean)
        self.head = (self.head + text)[:HEAD]
        self.tail = (self.tail + text)[-TAIL:]
        self.total += len(text)

    def value(self):
        if self.total <= TAIL: return self.tail
        if self.total <= HEAD + TAIL: return self.head[:self.total-TAIL] + self.tail
        return self.head + self.tail


def save(folder, value):
    temp = folder / 'result.new'
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump(value, f, ensure_ascii=False)
    os.replace(temp, folder / 'result.json')


def run(folder):
    os.umask(0o077)
    spec = json.loads((folder / 'spec.json').read_text())
    source = folder / ('source.py' if spec['language'] == 'python' else 'source.sh')
    output = Output()
    started = time.time()
    child = None
    master = None
    old_tty = None
    exit_code = None
    def update(running=True):
        value = dict(language=spec['language'], running=running, exit_code=exit_code,
                     output=output.value(), output_chars=output.total,
                     retention_limited=output.total > HEAD + TAIL,
                     retained_limit_chars=HEAD + TAIL, started_at=started,
                     updated_at=time.time(), elapsed_s=round(time.time()-started, 3))
        if child: value['command_pid'] = child
        save(folder, value)
    try:
        child, master = pty.fork()
        if child == 0:
            if spec['language'] == 'python':
                # Match python -c import behavior: local project modules resolve
                # from the requested workspace, not this private spool folder.
                loader = ("import runpy,sys; path=sys.argv[1]; sys.argv=[path]; "
                          "sys.path[0]=''; runpy.run_path(path,run_name='__main__')")
                os.execvp('python3', ['python3', '-c', loader, str(source)])
            os.execvp('bash', ['bash', str(source)])
        def resize(*_):
            try:
                size = fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, b'\0'*8)
                fcntl.ioctl(master, termios.TIOCSWINSZ, size)
            except OSError: pass
        resize()
        signal.signal(signal.SIGWINCH, resize)
        def terminate(signum, _frame):
            try: os.killpg(child, signum)
            except ProcessLookupError: pass
        signal.signal(signal.SIGTERM, terminate)
        signal.signal(signal.SIGHUP, terminate)
        if os.isatty(0):
            old_tty = termios.tcgetattr(0)
            tty.setraw(0, termios.TCSANOW)
        update()
        last_write = time.monotonic()
        stdin = True
        status = None
        while True:
            readable, _, _ = select.select([master] + ([0] if stdin else []), [], [], .1)
            if master in readable:
                try: data = os.read(master, 65536)
                except OSError as exc:
                    if exc.errno != errno.EIO: raise
                    data = b''
                if not data: break
                output.append(data)
                view = memoryview(data)
                while view:
                    try: n = os.write(1, view)
                    except OSError: view = b''; break
                    view = view[n:]
            if 0 in readable:
                data = os.read(0, 4096)
                if data:
                    try: os.write(master, data)
                    except OSError: pass
                else: stdin = False
            if time.monotonic()-last_write >= .2:
                update(); last_write = time.monotonic()
            if status is None:
                pid, child_status = os.waitpid(child, os.WNOHANG)
                if pid: status = child_status
            if status is not None and not readable: break
        if status is None: _, status = os.waitpid(child, 0)
        exit_code = os.waitstatus_to_exitcode(status)
        if exit_code < 0: exit_code = 128-exit_code
        output.append(b'', final=True)
    except BaseException as exc:
        exit_code = 1
        output.append(('\nCommand runner failed: '+type(exc).__name__+'\n').encode())
        if child:
            try: os.killpg(child, signal.SIGTERM)
            except ProcessLookupError: pass
    finally:
        if old_tty is not None:
            try: termios.tcsetattr(0, termios.TCSADRAIN, old_tty)
            except OSError: pass
        if master is not None: os.close(master)
        source.unlink(missing_ok=True)
        update(False)
    return exit_code

if __name__ == '__main__':
    raise SystemExit(run(Path(sys.argv[1])))
