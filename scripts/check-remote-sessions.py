#!/usr/bin/env python3
"""Live regression: isolated Hive server, disposable tmux sessions on configured workers.

Checks all configured workers via existing SSH configuration. Requires Playwright
in NODE_PATH for the browser terminal check. Never touches existing sessions.
"""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
HOSTS = ['local', 'mac-air', 'archlinux-worker', 'cis-linux2', 'cis-a6000']
with tempfile.TemporaryDirectory(prefix='hive-remote-sessions-') as tmp:
    root = Path(tmp)
    (root / 'config').mkdir()
    config = (ROOT / 'config/hive.toml').read_text().replace(
        'path = "~/.hive/hive.db"', f'path = "{root / "test.db"}"')
    (root / 'config/hive.toml').write_text(config)
    (root / 'config/workers.toml').write_text((ROOT / 'config/workers.toml').read_text())
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    env = dict(os.environ, HIVE_CONFIG_ROOT=tmp, HIVE_WEB_ADDR=f'127.0.0.1:{port}',
               HIVE_WEB_PASSWORD='sessions-test-password', HIVE_WEB_STATIC=str(ROOT / 'hive-web/static'))
    name = 'hive-session-check-' + uuid.uuid4().hex[:12]
    created = []
    cookie = ''

    def request(method, path, body=None):
        conn = http.client.HTTPConnection('127.0.0.1', port, timeout=25)
        conn.request(method, path, None if body is None else json.dumps(body),
                     {'Cookie': cookie, 'Content-Type': 'application/json'})
        res = conn.getresponse()
        status, raw, headers = res.status, res.read(), dict(res.getheaders())
        conn.close()
        return status, raw, headers

    with (root / 'web.log').open('w+') as log:
        web = subprocess.Popen([str(ROOT / 'target/debug/hive-web')], env=env, cwd=tmp, stdout=log, stderr=log)
        try:
            for _ in range(100):
                try:
                    if request('GET', '/api/health')[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.1)
            else:
                log.seek(0)
                raise RuntimeError(log.read())
            conn = http.client.HTTPConnection('127.0.0.1', port)
            conn.request('POST', '/login', 'password=sessions-test-password',
                         {'Content-Type': 'application/x-www-form-urlencoded'})
            res = conn.getresponse()
            assert res.status == 303
            cookie = res.getheader('Set-Cookie').split(';')[0]
            res.read()
            conn.close()
            assert request('POST', '/api/sessions', {'name': name, 'host': 'unknown', 'kind': 'shell'})[0] == 404
            for host in HOSTS:
                status, raw, _ = request('POST', '/api/sessions', {'name': name, 'host': host, 'kind': 'shell'})
                assert status == 201, (host, status, raw)
                created.append(host)
            status, raw, headers = request('GET', '/api/sessions')
            assert status == 200
            assert 'x-hive-session-errors' not in headers, headers
            sessions = json.loads(raw)
            assert {s['host'] for s in sessions if s['name'] == name} == set(HOSTS)
            print('PASS: same-named sessions on all five machines', flush=True)
            subprocess.run(['node', str(ROOT / 'scripts/check-remote-sessions.cjs')], check=True,
                           env=dict(os.environ, HIVE_TEST_BASE=f'http://127.0.0.1:{port}', HIVE_TEST_SESSION=name))
            status, raw, _ = request('DELETE', f'/api/sessions/{name}?host=mac-air')
            assert status == 204, raw
            created.remove('mac-air')
            sessions = json.loads(request('GET', '/api/sessions')[1])
            assert {s['host'] for s in sessions if s['name'] == name} == set(HOSTS) - {'mac-air'}
            print('PASS: deleting the Air session preserves the same-named sessions on other machines', flush=True)
        finally:
            for host in created:
                status, raw, _ = request('DELETE', f'/api/sessions/{name}?host={host}')
                if status != 204:
                    print(f'Cleanup failed for {host}/{name}: {status} {raw!r}')
            web.terminate()
            web.wait(timeout=10)
