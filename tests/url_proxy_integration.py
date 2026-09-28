"""Isolated proxy regression tests; no Google traffic and no CAPTCHA automation.
Run the url_proxy_test_server example first. Set PROXY_TEST_PORT and TOKEN_FILE.
"""
import base64
import hashlib
import http.client
import json
import os
import secrets
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PORT = int(os.environ.get('PROXY_TEST_PORT', '11080'))
TOKEN = Path(os.environ.get('TOKEN_FILE', '/build/webterm/proxy-browser-fix/forward.token')).read_text().strip()
AUTH = 'Basic ' + base64.b64encode(('proxy:' + TOKEN).encode()).decode()
RESULTS = []
B_ORIGIN = ''

class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *_):
        pass
    def do_POST(self):
        self.do_GET()
    def do_GET(self):
        body = self.rfile.read(int(self.headers.get('content-length', '0')))
        path = self.path.split('?', 1)[0]
        locations = {
            '/redirect': '/next?x=%2F',
            '/nested/redirect': '../next?x=%2F',
            '/absolute': f'http://127.0.0.1:{self.server.server_port}/next?x=%2F',
            '/cross': B_ORIGIN + '/finish?x=a%2Fb',
            '/post-redirect': '/echo',
        }
        if path in locations:
            self.send_response(307 if path == '/post-redirect' else 302)
            self.send_header('Location', locations[path])
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        if path == '/socket':
            self.send_response(101)
            self.send_header('Connection', 'Upgrade')
            self.send_header('Upgrade', 'websocket')
            key = self.headers['Sec-WebSocket-Key']
            accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
            self.send_header('Sec-WebSocket-Accept', accept)
            self.end_headers()
            h = self.rfile.read(2)
            mask = self.rfile.read(4)
            payload = self.rfile.read(h[1] & 127)
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            reply = b'echo:' + payload
            self.wfile.write(bytes([0x81, len(reply)]) + reply)
            self.wfile.flush()
            self.close_connection = True
            return
        data = json.dumps({'path': self.path, 'body': body.decode(), 'method': self.command,
                           'cookie': self.headers.get('Cookie', ''), 'host': self.headers.get('Host')}).encode()
        self.send_response(200)
        if path == '/cookie':
            self.send_header('Set-Cookie', 'sid=abc; Domain=example.com; Path=/app; Secure; HttpOnly; SameSite=None')
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

def req(path, method='GET', body=None, headers=None):
    c = http.client.HTTPConnection('127.0.0.1', PORT, timeout=8)
    c.request(method, path, body=body, headers=headers or {})
    r = c.getresponse()
    answer = (r.status, dict(r.getheaders()), r.read())
    c.close()
    return answer

def target(url):
    assert req('/pmurl', 'PUT', json.dumps({'url': url}), {'Content-Type':'application/json'})[0] == 200

def active():
    return json.loads(req('/pmurl')[2])['url']

def check(name, condition):
    if not condition:
        raise AssertionError(name)
    RESULTS.append(name)

servers = [ThreadingHTTPServer(('127.0.0.1', 0), Handler) for _ in range(2)]
for server in servers:
    threading.Thread(target=server.serve_forever, daemon=True).start()
A_ORIGIN, B_ORIGIN = [f'http://127.0.0.1:{s.server_port}' for s in servers]
try:
    target(A_ORIGIN + '/base/index')
    status, headers, _ = req('/')
    check('root opens selected path without duplication', status == 307 and headers['location'] == '/base/index')
    check('asset paths resolve at origin root', json.loads(req('/asset.js')[2])['path'] == '/asset.js')
    status, headers, _ = req('/proxy/search?q=a%2Fb&x=%252F')
    check('legacy prefix removed without changing query', status == 307 and headers['location'] == '/search?q=a%2Fb&x=%252F')
    status, headers, _ = req('/proxy//evil.example/path')
    check('legacy cleanup cannot create an open redirect', status == 307 and headers['location'] == '/evil.example/path')
    for path in ['/redirect', '/nested/redirect', '/absolute']:
        status, headers, _ = req(path)
        check('redirect rewritten without prefix: ' + path, status == 302 and headers['location'] == '/next?x=%2F')
    check('same-origin redirects do not poison selected target', active() == A_ORIGIN + '/base/index')
    status, headers, _ = req('/cross', headers={'Sec-Fetch-Dest':'document'})
    check('cross-origin navigation retargets and keeps clean URL', status == 302 and headers['location'] == '/finish?x=a%2Fb' and active() == B_ORIGIN + '/finish?x=a%2Fb')
    check('redirect destination is fetched exactly', json.loads(req('/finish?x=a%2Fb')[2])['path'] == '/finish?x=a%2Fb')
    target(A_ORIGIN + '/')
    status, headers, _ = req('/cross', headers={'Sec-Fetch-Dest':'empty','Sec-Fetch-Mode':'cors'})
    check('XHR cross-origin redirect cannot change global target', active() == A_ORIGIN + '/' and headers['location'].startswith(B_ORIGIN))
    status, headers, _ = req('/post-redirect', 'POST', 'body=keep')
    response = json.loads(req(headers['location'], 'POST', 'body=keep')[2])
    check('307 preserves POST method and body', status == 307 and response['method'] == 'POST' and response['body'] == 'body=keep')
    cookie = req('/cookie')[1]['set-cookie']
    check('cookie security and original path are preserved', cookie == 'sid=abc; Path=/app; Secure; HttpOnly; SameSite=None')
    cookie = json.loads(req('/echo', headers={'Cookie':'webterm_session=private; colab_de_auth=private; app=ok'})[2])['cookie']
    check('control cookies are not leaked upstream', cookie == 'app=ok')
    s = socket.create_connection(('127.0.0.1', PORT), timeout=8)
    key = base64.b64encode(secrets.token_bytes(16)).decode()
    s.sendall((f'GET /socket HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n').encode())
    h = b''
    while not h.endswith(b'\r\n\r\n'):
        h += s.recv(1)
    check('WebSocket upgrade preserved', b' 101 ' in h)
    mask = secrets.token_bytes(4)
    payload = b'ping'
    s.sendall(bytes([0x81,0x80|len(payload)]) + mask + bytes(b ^ mask[i%4] for i,b in enumerate(payload)))
    fh = s.recv(2)
    reply = b''
    while len(reply) < (fh[1] & 127):
        reply += s.recv((fh[1] & 127) - len(reply))
    check('WebSocket bidirectional echo', reply == b'echo:ping')
    s.close()
    check('CONNECT requires authentication', req('example.com:443', 'CONNECT')[0] == 407)
    check('wrong proxy password rejected', req('example.com:443', 'CONNECT', headers={'Proxy-Authorization':'Basic cHJveHk6d3Jvbmc='})[0] == 407)
    for dest in ['127.0.0.1:443', '169.254.169.254:80', 'example.com:22']:
        check('restricted CONNECT destination blocked: '+dest, req(dest, 'CONNECT', headers={'Proxy-Authorization':AUTH})[0] == 403)
    check('absolute URLs cannot hit controller routes', req('http://127.0.0.1/mpxx', headers={'Proxy-Authorization':AUTH})[0] == 403)
    status, _, body = req('/mpxx/proxy.pac')
    check('PAC file contains no credentials', status == 200 and TOKEN.encode() not in body and b'PROXY' in body)
    target(A_ORIGIN + '/')
    status, _, body = req('/sorry/index?continue=https%3A%2F%2Fwww.google.com%2Fsearch%3Fq%3Dls')
    sorry = json.loads(body)
    check('verification path is passed to upstream instead of replaced', status == 200 and sorry['path'].startswith('/sorry/index?continue='))
    result = {'passed': len(RESULTS), 'failed': 0, 'checks': RESULTS}
    Path('/build/webterm/proxy-browser-fix/integration.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))
finally:
    target('https://www.google.com/')
    for server in servers:
        server.shutdown()
