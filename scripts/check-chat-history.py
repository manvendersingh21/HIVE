#!/usr/bin/env python3
"""Check saved chats against a disposable web server and fake Ollama; no real model or workers."""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--browser', action='store_true', help='Also run Playwright checks; provide playwright via NODE_PATH')
args = parser.parse_args()
gate = threading.Event()
gate.set()
requests = []

class Model(BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        try: self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError): pass
    def do_GET(self): self.reply(200, {'models': []})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        prompt = body['messages'][0]['content']
        requests.append(prompt)
        if prompt.startswith('Classify'):
            assert 'format' not in body, 'classification must remain plain text'
        if prompt.startswith('You are a task planner'):
            schema = body['format']
            assert 'target_machine' in schema['properties']['subtasks']['items']['required']
            assert schema['type'] == 'object'

        if 'hold fixture' in prompt: gate.wait(15)
        if 'failure fixture' in prompt:
            self.reply(500, {'error': 'mock model unavailable'})
            return
        if prompt.startswith('Classify'):
            answer = 'SIMPLE'
        elif 'premature fixture' in prompt and 'Execution feedback' not in prompt:
            answer = json.dumps({'phase':'complete','targets':['local'],'summary':'Unverified claim','subtasks':[]})
        elif 'planning retry fixture' in prompt or 'repeat fixture' in prompt:
            if 'Successful functional verification recorded: true.' in prompt:
                answer=json.dumps({'phase':'complete','summary':'Retry verified','subtasks':[]})
            elif 'Execution feedback' not in prompt:
                answer=json.dumps({'phase':'work','targets':['local'],'summary':'Work once','subtasks':[{'description':'write once','target_machine':'local','commands':["printf 'once\\n' >> planning-once.txt"]}]})
            elif 'Your previous planning attempt failed' not in prompt:
                if 'repeat fixture' in prompt:
                    answer=json.dumps({'phase':'work','targets':['local'],'summary':'Repeat work','subtasks':[{'description':'write once','target_machine':'local','commands':["printf 'once\\n' >> planning-once.txt"]}]})
                else: answer='invalid JSON from model'
            else:
                answer=json.dumps({'phase':'verify','summary':'Verify preserved work','subtasks':[{'description':'verify once','target_machine':'local','commands':['cat planning-once.txt']}]})
        elif 'repair fixture' in prompt:
            if 'Successful functional verification recorded: true.' in prompt:
                plan = {'phase':'complete','summary':'Repair verified by running generated code','subtasks':[]}
            elif 'Execution feedback' not in prompt:
                plan = {'phase':'work','summary':'Try implementation','subtasks':[{'description':'first attempt','target_machine':'local','commands':["printf 'intentional failure\\n' >&2; exit 7", 'touch should-not-run']}]}
            elif 'Round 1.' in prompt:
                # Attempt a false completion after a failed command: Hive must reject it.
                plan = {'phase':'complete','summary':'Pretend success','subtasks':[]}
            elif 'Wrote ' not in prompt:
                assert 'exit_code: 7' in prompt, 'model never received actual failure'
                plan = {'phase':'work','summary':'Repair from observed failure','subtasks':[{'description':'write implementation','target_machine':'local','files':[{'path':str(root/'repair.py'),'content':'print(' + repr('repaired "quote" and unicode café') + ')\n'}], 'commands':[f'python3 -m py_compile {root}/repair.py']}]}
            else:
                plan = {'phase':'verify','summary':'Run generated implementation','subtasks':[{'description':'functional verification','target_machine':'local','commands':[f'python3 {root}/repair.py']}]}
            answer = json.dumps(plan)
        else:
            if 'approval fixture' in prompt:
                commands = ["printf 'once\\n' >> first.txt", 'rm -rf ./approval-target']
            else:
                commands = ["printf 'saved-chat-ok\\n'"]
            if 'Execution feedback for the SAME user request.' in prompt:
                if 'Successful functional verification recorded: true.' in prompt:
                    answer = json.dumps({'phase': 'complete', 'summary': 'Saved chat response verified', 'subtasks': []})
                else:
                    answer = json.dumps({'phase': 'verify', 'summary': 'Verify fixture', 'subtasks': [{'description': 'Verify fixture output', 'commands': ["printf 'saved-chat-ok\\n'"], 'target_machine': 'local'}]})
            else:
                answer = json.dumps({'phase': 'work', 'summary': 'Saved chat response', 'subtasks': [{'description': 'Run fixture', 'commands': commands, 'requires_remote': False, 'target_machine': 'local'}]})
        self.reply(200, {'message': {'content': answer}})

model = ThreadingHTTPServer(('127.0.0.1', 0), Model)
threading.Thread(target=model.serve_forever, daemon=True).start()

with tempfile.TemporaryDirectory(prefix='hive-chat-history-') as tmp:
    root = Path(tmp)
    (root/'config').mkdir()
    (root/'config/workers.toml').write_text('workers = []\n')
    config = (ROOT/'config/hive.toml').read_text().replace('http://localhost:11434', f'http://127.0.0.1:{model.server_port}').replace('path = "~/.hive/hive.db"', f'path = "{root / "history.db"}"').replace('directory = "~/.hive/skills"', f'directory = "{root / "skills"}"')
    (root/'config/hive.toml').write_text(config)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
    env = dict(os.environ, HIVE_CONFIG_ROOT=tmp, HIVE_WEB_ADDR=f'127.0.0.1:{port}', HIVE_WEB_PASSWORD='history-test-password', HIVE_WEB_STATIC=str(ROOT/'hive-web/static'), RUST_LOG='warn')
    for key in ('NVIDIA_API_KEY_FLASH','NVIDIA_API_KEY_EMBEDDING','OPENAI_API_KEY','GEMINI_API_KEY','ANTHROPIC_API_KEY'):
        env.pop(key, None)
    web = None
    cookie = None
    log = (root/'web.log').open('w+')

    def api(method, path, body=None, expected=200, auth=True):
        connection = http.client.HTTPConnection('127.0.0.1', port, timeout=20)
        headers = {'Content-Type': 'application/json'}
        if auth and cookie: headers['Cookie'] = cookie
        connection.request(method, path, None if body is None else json.dumps(body), headers)
        response = connection.getresponse(); raw = response.read().decode(); connection.close()
        assert response.status == expected, (method, path, response.status, raw)
        return json.loads(raw) if raw else None

    def start():
        global web, cookie
        web = subprocess.Popen([str(ROOT/'target/debug/hive-web')], cwd=tmp, env=env, stdout=log, stderr=log)
        for _ in range(100):
            try:
                c = http.client.HTTPConnection('127.0.0.1', port, timeout=1)
                c.request('GET','/api/health'); response=c.getresponse(); response.read(); c.close(); break
            except OSError: time.sleep(.1)
        else: raise AssertionError('web startup failed')
        c = http.client.HTTPConnection('127.0.0.1',port)
        c.request('POST','/login','password=history-test-password',{'Content-Type':'application/x-www-form-urlencoded'})
        response=c.getresponse(); assert response.status==303
        cookie=response.getheader('Set-Cookie').split(';')[0]; response.read(); c.close()

    def stop():
        global web
        if web:
            web.terminate(); web.wait(timeout=10); web=None

    def create(): return api('POST','/api/chats',{},201)['id']
    def send(chat, text, request_id=None, expected=200):
        return api('POST','/api/chat',{'message':text,'conversation_id':chat,'request_id':request_id or str(uuid.uuid4())},expected)
    def detail(chat): return api('GET','/api/chats/'+chat)
    def wait_status(chat, status):
        for _ in range(100):
            data=detail(chat)
            if data['messages'] and data['messages'][-1]['status']==status: return data
            time.sleep(.05)
        raise AssertionError(('status never reached',status,data))

    try:
        start()
        for method,path,body in [('GET','/api/chats',None),('GET','/api/chats/missing',None),('POST','/api/chats',{}),('POST','/api/chat',{'message':'test'})]:
            # Unauthorized routes return plain text; don't parse it as JSON.
            c=http.client.HTTPConnection('127.0.0.1',port)
            c.request(method,path,None if body is None else json.dumps(body),{'Content-Type':'application/json'})
            r=c.getresponse(); assert r.status==401; r.read(); c.close()
        chat=create(); request_id=str(uuid.uuid4())
        first=send(chat,'first fixture',request_id)
        assert first['conversation_id']==chat and first['run']['provider']=='local'
        assert first['workflow']['status']=='complete' and first['workflow']['verified']
        assert len(detail(chat)['messages'])==2
        count=len(requests)
        assert send(chat,'first fixture',request_id)==first
        assert len(requests)==count and len(detail(chat)['messages'])==2
        other=create()
        c=http.client.HTTPConnection('127.0.0.1',port)
        c.request('POST','/api/chat',json.dumps({'message':'first fixture','conversation_id':other,'request_id':request_id}),{'Content-Type':'application/json','Cookie':cookie})
        r=c.getresponse();assert r.status==409;r.read();c.close()
        start_at=len(requests)
        send(chat,'follow up fixture')
        assert any('first fixture' in p and 'saved-chat-ok' in p for p in requests[start_at:] if not p.startswith('Classify'))
        start_at=len(requests)
        send(other,'separate fixture')
        assert all('first fixture' not in p for p in requests[start_at:])
        assert any(c['id']==chat for c in api('GET','/api/chats?q=first%20fixture'))
        stop();start()
        assert len(detail(chat)['messages'])==4
        print('PASS: persistence, search, context isolation, idempotency, authentication, restart',flush=True)

        repair=create()
        fixed=send(repair,'repair fixture')
        assert fixed['workflow']['status']=='complete' and fixed['workflow']['verified'], fixed
        assert fixed['workflow']['round']==5, fixed
        assert fixed['result']['outcomes'][0]['status']=='failed'
        assert not (root/'should-not-run').exists(), 'Ran a dependent step after failure'
        assert (root/'repair.py').exists(), 'Structured file write never executed'
        assert any('repaired "quote" and unicode café' in o['output'] for o in fixed['result']['outcomes'])
        print('PASS: failure feedback, dependency stop, false-completion rejection, file writing, correction, and verification',flush=True)

        premature=send(create(),'premature fixture')
        assert premature['workflow']['round'] > 1
        assert premature['workflow']['verified']
        assert premature['result']['outcomes'], 'Accepted an initial completion claim without running verification'
        print('PASS: initial empty completion cannot bypass verification',flush=True)

        retried=send(create(),'planning retry fixture')
        assert retried['workflow']['verified'] and retried['workflow']['round']==3
        assert (root/'planning-once.txt').read_text()=='once\n'
        assert len(retried['result']['outcomes'])==2
        print('PASS: planning retry receives its error without replaying completed work',flush=True)

        (root/'planning-once.txt').unlink()
        repeated=send(create(),'repeat fixture')
        assert repeated['workflow']['verified']
        assert (root/'planning-once.txt').read_text()=='once\n', 'Repeated a successful round'
        assert any('No progress:' in p for p in requests), 'Model did not receive loop feedback'
        print('PASS: identical successful rounds are rejected before execution and corrected',flush=True)

        approval=create();(root/'approval-target').mkdir()
        reply=send(approval,'approval fixture')
        assert reply['result']['awaiting_approval']==[1], reply
        assert (root/'first.txt').read_text()=='once\n'
        stop();start()
        saved=detail(approval)['messages'][-1]['reply']
        approved=api('POST',f"/api/chat/{saved['run']['id']}/approve",{'approved':[1]})
        assert not approved['result']['awaiting_approval']
        assert not (root/'approval-target').exists()
        assert (root/'first.txt').read_text()=='once\n', 'Replayed a completed command!'
        assert len(detail(approval)['messages'])==2
        print('PASS: approval survives restart and never replays completed steps',flush=True)

        held=create();gate.clear()
        c=http.client.HTTPConnection('127.0.0.1',port)
        c.request('POST','/api/chat',json.dumps({'message':'hold fixture','conversation_id':held}),{'Content-Type':'application/json','Cookie':cookie})
        wait_status(held,'planning'); c.close(); gate.set()
        wait_status(held,'completed')
        print('PASS: browser disconnect does not lose the result',flush=True)

        interrupted=create();gate.clear()
        c=http.client.HTTPConnection('127.0.0.1',port)
        c.request('POST','/api/chat',json.dumps({'message':'hold fixture','conversation_id':interrupted}),{'Content-Type':'application/json','Cookie':cookie})
        wait_status(interrupted,'planning');stop();c.close();gate.set();start()
        assert detail(interrupted)['messages'][-1]['status']=='interrupted'
        print('PASS: interrupted work stays recorded without automatic execution',flush=True)

        failed=create()
        c=http.client.HTTPConnection('127.0.0.1',port)
        c.request('POST','/api/chat',json.dumps({'message':'failure fixture','conversation_id':failed}),{'Content-Type':'application/json','Cookie':cookie})
        r=c.getresponse();assert r.status==502;r.read();c.close()
        assert detail(failed)['messages'][-1]['status']=='failed'
        stop()
        (root/'config/hive.toml').write_text(config.replace(f'http://127.0.0.1:{model.server_port}','http://127.0.0.1:1'))
        start(); assert len(detail(chat)['messages'])==4
        assert any(c['id']==failed for c in api('GET','/api/chats?q=unavailable'))
        print('PASS: model failures are saved and history works with Ollama unavailable',flush=True)
        stop();(root/'config/hive.toml').write_text(config);start()
        if args.browser:
            browser_env=dict(os.environ,HIVE_TEST_BASE=f'http://127.0.0.1:{port}',HIVE_TEST_DIR=tmp)
            subprocess.run(['node',str(ROOT/'scripts/check-chat-history-browser.cjs')],env=browser_env,check=True)
        assert (root/'history.db').stat().st_mode & 0o777 == 0o600
        print('Saved chat integration passed.',flush=True)
    finally:
        stop(); gate.set(); log.close(); model.shutdown()
