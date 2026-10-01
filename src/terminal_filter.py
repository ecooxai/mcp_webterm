"""Filter a captured terminal snapshot, never the live PTY input stream.

The supplied Bash program has the terminal user's permissions (not a sandbox).
Its stdin is the full *retained* text. Bound time, captured output and child life.
"""
from __future__ import annotations
import codecs
import json
import os
import selectors
import signal
import subprocess
import sys
import tempfile
import time

TIMEOUT = 5.0
OUTPUT_CHARS = 262144
STDERR_CHARS = 2000
IO_LIMIT = 16 * 1024 * 1024

class Capture:
    def __init__(self, limit):
        self.limit = limit
        self.head = ''
        self.tail = ''
        self.total = 0
        self.decoder = codecs.getincrementaldecoder('utf-8')('replace')
    def append(self, raw, final=False):
        text = self.decoder.decode(raw, final=final)
        head = self.limit // 4
        self.head = (self.head + text)[:head]
        self.tail = (self.tail + text)[-(self.limit-head):]
        self.total += len(text)
    def value(self):
        tail = self.limit - self.limit//4
        if self.total <= tail: return self.tail
        if self.total <= self.limit: return self.head[:self.total-tail] + self.tail
        return self.head + self.tail


def run(command: str, text: str, cwd: str, timeout: float = TIMEOUT, max_command_bytes: int = 4096, env_overrides: dict | None = None) -> dict:
    if not isinstance(command, str) or not command.strip() or '\0' in command or len(command.encode()) > max_command_bytes:
        raise ValueError('filter_cmd must be nonempty Bash text of at most 4096 UTF-8 bytes')
    out, err = Capture(OUTPUT_CHARS), Capture(STDERR_CHARS)
    began = time.monotonic()
    timed_out = limited = False
    env = dict(os.environ)
    for key in ('BASH_ENV', 'ENV', 'WEBTERM_PASSWORD', 'WEBTERM_AUTH_TOKEN', 'AUTH_TOKEN'):
        env.pop(key, None)
    if env_overrides:
        env.update(env_overrides)
    # A private anonymous file also handles filters which never consume stdin.
    with tempfile.TemporaryFile() as source, selectors.DefaultSelector() as selector:
        source.write(text.encode('utf-8')); source.seek(0)
        child = subprocess.Popen(['/bin/bash', '--noprofile', '--norc', '-o', 'pipefail', '-c', command],
                                 stdin=source, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 cwd=cwd, env=env, start_new_session=True)
        for pipe, capture in ((child.stdout,out),(child.stderr,err)):
            os.set_blocking(pipe.fileno(), False); selector.register(pipe, selectors.EVENT_READ, capture)
        total_bytes = 0
        try:
            while selector.get_map():
                remaining = timeout - (time.monotonic()-began)
                if remaining <= 0:
                    timed_out = True; break
                for key, _ in selector.select(min(.1, remaining)):
                    raw = os.read(key.fd, 65536)
                    if not raw:
                        key.data.append(b'', final=True); selector.unregister(key.fileobj); continue
                    key.data.append(raw); total_bytes += len(raw)
                    if total_bytes > IO_LIMIT:
                        limited = True; break
                if limited: break
            if not timed_out and not limited:
                try: child.wait(timeout=max(.01, timeout-(time.monotonic()-began)))
                except subprocess.TimeoutExpired: timed_out = True
        finally:
            # Also reap descendants retaining pipe handles or backgrounded by Bash.
            try: os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError: pass
            child.wait(timeout=2)
            child.stdout.close(); child.stderr.close()
    code = 124 if timed_out else (137 if limited else child.returncode)
    if code < 0: code = 128-code
    result = {'output':out.value(),'output_chars':out.total,
              'filter_exit_code':code,'filter_input_chars':len(text)}
    if err.total: result['filter_stderr'] = err.value()
    if err.total > STDERR_CHARS: result['filter_stderr_truncated'] = True
    if timed_out: result['filter_timed_out'] = True
    if limited: result['filter_output_limit_hit'] = True
    if out.total > OUTPUT_CHARS or limited: result['retention_limited'] = True
    return result

if __name__ == '__main__':
    request = json.loads(sys.stdin.buffer.read(2*1024*1024+1))
    json.dump(run(request['command'], request['text'], request['cwd']), sys.stdout, ensure_ascii=True)
