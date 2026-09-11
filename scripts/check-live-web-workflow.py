#!/usr/bin/env python3
"""Submit a real Ollama request to an isolated Hive web instance; never execute workload commands here.

Supply a request in a text file. The only mutations outside Hive are test config,
chat requests, and saved diagnostic artifacts. All requested machine work is done
by Hive's model and executor. Test sessions/workspaces are deliberately left for inspection.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('request',type=Path)
parser.add_argument('--timeout',type=int,default=1200)
args=parser.parse_args()
root=Path(tempfile.mkdtemp(prefix='hive-live-workflow-'))
(root/'config').mkdir()
config=(ROOT/'config/hive.toml').read_text().replace('path = "~/.hive/hive.db"',f'path = "{root}/test.db"').replace('directory = "~/.hive/skills"',f'directory = "{root}/skills"')
(root/'config/hive.toml').write_text(config)
(root/'config/workers.toml').write_text((ROOT/'config/workers.toml').read_text())
with socket.socket() as sock:
    sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
env=dict(os.environ,HIVE_CONFIG_ROOT=str(root),HIVE_WEB_ADDR=f'127.0.0.1:{port}',HIVE_WEB_PASSWORD='workflow-test-password',HIVE_MASTER_NAME='manus-mac-mini',HIVE_WEB_STATIC=str(ROOT/'hive-web/static'),RUST_LOG='hive_web=info,hive_core=info')
log=(root/'web.log').open('w+')
web=subprocess.Popen([str(ROOT/'target/debug/hive-web')],cwd=root,env=env,stdout=log,stderr=log)
cookie=''

def call(method,path,data=None):
    c=http.client.HTTPConnection('127.0.0.1',port,timeout=20)
    c.request(method,path,None if data is None else json.dumps(data),{'Content-Type':'application/json','Cookie':cookie})
    r=c.getresponse();raw=r.read();c.close()
    assert r.status in (200,201,202),(r.status,raw)
    return json.loads(raw)

try:
    for _ in range(150):
        try:
            c=http.client.HTTPConnection('127.0.0.1',port,timeout=1);c.request('GET','/api/health');r=c.getresponse();r.read();c.close();break
        except OSError:time.sleep(.1)
    c=http.client.HTTPConnection('127.0.0.1',port)
    c.request('POST','/login','password=workflow-test-password',{'Content-Type':'application/x-www-form-urlencoded'})
    r=c.getresponse();assert r.status==303;cookie=r.getheader('Set-Cookie').split(';')[0];r.read();c.close()
    receipt=call('POST','/api/chat',{'message':args.request.read_text(),'request_id':str(uuid.uuid4()),'background':True})
    chat=receipt['conversation_id']
    print(f'Hive web request accepted: {chat}; artifacts: {root}',flush=True)
    last=None;deadline=time.monotonic()+args.timeout
    while time.monotonic()<deadline:
        detail=call('GET','/api/chats/'+chat)
        message=detail['messages'][-1]
        reply=message.get('reply') or {}
        workflow=reply.get('workflow') or {}
        outputs=reply.get('result',{}).get('outcomes',[])
        state=(message['status'],workflow.get('round'),len(outputs),workflow.get('status'))
        if state!=last:
            print('Progress:',state,flush=True)
            if outputs:
                print('Latest:',json.dumps(outputs[-1],ensure_ascii=False)[:5000],flush=True)
            last=state
        (root/'result.json').write_text(json.dumps(detail,indent=2))
        if message['status'] not in ('planning','executing'):
            print('Final:',message['content'][:12000],flush=True)
            assert message['status']=='completed' and workflow.get('verified'), 'Hive did not verify completion; inspect saved artifacts'
            sessions=call('GET','/api/sessions')
            (root/'sessions.json').write_text(json.dumps(sessions,indent=2))
            print('PASS: Hive itself executed and verified the request through its web API',flush=True)
            break
        time.sleep(2)
    else:raise TimeoutError('Hive workflow did not finish before the test deadline')
finally:
    web.terminate();web.wait(timeout=10);log.close()
