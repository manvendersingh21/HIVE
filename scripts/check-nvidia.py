#!/usr/bin/env python3
"""Exercise NVIDIA through disposable CLI/web deployments; --live uses separate reasoning and embedding keys.
Build hive and hive-web first. No worker CLIs or existing database are used.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import sys
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
MODEL = "nvidia/nemotron-3-ultra-550b-a55b"
EMBED = "nvidia/nemotron-3-embed-1b"
TASK = "Run exactly printf 'hive-nvidia-ok\\n' locally, and do nothing else."
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--live", action="store_true")
parser.add_argument("--probe", choices=["classification", "planning", "embeddings", "integration"], help="Run just one API probe")
parser.add_argument("--key-file", type=Path, help="Read NVIDIA_API_KEY_FLASH and NVIDIA_API_KEY_EMBEDDING from a dotenv file")
args = parser.parse_args()
env = dict(os.environ)
FLASH_KEY = "NVIDIA_API_KEY_FLASH"
EMBEDDING_KEY = "NVIDIA_API_KEY_EMBEDDING"
if args.key_file:
    for line in args.key_file.read_text().splitlines():
        key, separator, value = line.partition("=")
        if separator and key.strip() in (FLASH_KEY, EMBEDDING_KEY):
            env[key.strip()] = value.strip().strip("\"'")
for key in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"):
    env.pop(key, None)
required_keys = [EMBEDDING_KEY] if args.probe == "embeddings" else [FLASH_KEY] if args.probe in ("classification", "planning") else [FLASH_KEY, EMBEDDING_KEY]
if args.live:
    for key in required_keys:
        if not env.get(key, "").strip():
            parser.error(f"--live requires {key}")
else:
    env[FLASH_KEY] = "mock-flash-key"
    env[EMBEDDING_KEY] = "mock-embedding-key"
    env["NVIDIA_API_KEY"] = "obsolete-key-must-not-be-used"
records = []


def timed(name, fn):
    start = time.monotonic()
    try:
        result = fn()
    except Exception as error:
        print(f"{name}: FAIL ({time.monotonic() - start:.2f}s): {type(error).__name__}: {error}", flush=True)
        raise
    print(f"{name}: PASS ({time.monotonic() - start:.2f}s)", flush=True)
    return result


class Mock(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        expected_key = "mock-embedding-key" if self.path.endswith("embeddings") else "mock-flash-key"
        assert self.headers["Authorization"] == "Bearer " + expected_key
        records.append((self.path, body))
        if self.path.endswith("embeddings"):
            result = {"data": [{"embedding": [1.0, 0.5, 0.25]}]}
        else:
            prompt = body["messages"][0]["content"]
            if prompt.startswith("Classify"):
                text = "SIMPLE"
            elif "You extract a knowledge graph" in prompt:
                text = json.dumps({"entities": [{"name": "smoke", "kind": "tool", "description": "test"}], "relations": []})
            else:
                text = json.dumps({"summary": "Print smoke marker", "subtasks": [{"description": "Print marker", "requires_remote": False, "commands": ["printf 'hive-nvidia-ok\\n'"], "expected_behavior": "Print marker", "required_capabilities": []}]})
            result = {"model": MODEL, "choices": [{"finish_reason": "stop", "message": {"content": text, "reasoning_content": "must remain separate"}}]}
        data = json.dumps(result).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


mock = None
if args.live:
    endpoint = "https://integrate.api.nvidia.com/v1"
else:
    mock = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    endpoint = f"http://127.0.0.1:{mock.server_port}/v1"


def api(path, body):
    request = urllib.request.Request(endpoint + path, json.dumps(body).encode(), {
        "Authorization": "Bearer " + env[EMBEDDING_KEY if path == "/embeddings" else FLASH_KEY], "Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=120) as response:
        return json.load(response)


def complete(prompt):
    result = api("/chat/completions", {"model": MODEL, "messages": [{"role": "user", "content": prompt}], "temperature": 1, "top_p": 0.95, "max_tokens": 16384, "stream": False, "chat_template_kwargs": {"enable_thinking": True}})
    assert result["choices"][0]["finish_reason"] == "stop", result["choices"][0]["finish_reason"]
    content = result["choices"][0]["message"]["content"]
    assert content.strip()
    return content


try:
    if args.probe in (None, "classification"):
        label = timed("classification", lambda: complete("Classify echo hi as SIMPLE, MEDIUM, COMPLEX, or CODE_HEAVY. Return only the word."))
        assert label.strip() == "SIMPLE", label
    if args.probe in (None, "planning"):
        plan = timed("planning", lambda: complete('Return only a JSON plan with summary and subtasks for echo hi. Each subtask has description, requires_remote=false, commands, and expected_behavior.'))
        assert json.loads(plan[plan.index("{"):plan.rindex("}")+1])["subtasks"]
    if args.probe in (None, "embeddings"):
        for mode in ("passage", "query"):
            result = timed(f"embedding {mode}", lambda: api("/embeddings", {"model": EMBED, "input": ["smoke test"], "input_type": mode, "encoding_format": "float"}))
            print(f"  dimensions: {len(result['data'][0]['embedding'])}", flush=True)
    if args.probe and args.probe != "integration":
        sys.exit(0)
    with tempfile.TemporaryDirectory(prefix="hive-nvidia-") as tmp:
        root = Path(tmp)
        (root / "config").mkdir()
        (root / "config/workers.toml").write_text("workers = []\n")
        config = f'''[finetune]
auto_collect = false
[master]
listen_addr = "127.0.0.1:0"
[llm]
single_provider = "nvidia"
[llm.nvidia]
model = "{MODEL}"
base_url = "{endpoint}"
[llm.local]
base_url = "http://127.0.0.1:1"
[memory]
embedding_provider = "nvidia"
embedding_model = "{EMBED}"
[database]
path = "{root / 'memory.db'}"
[skills]
directory = "{root / 'skills'}"
[web]
listen_addr = "127.0.0.1:0"
'''
        (root / "config/hive.toml").write_text(config)
        cli = [str(ROOT / "target/debug/hive"), "--project-root", tmp]

        def run_cli(*argv):
            p = subprocess.run(cli + list(argv), env=env, cwd=tmp, text=True, capture_output=True, timeout=420)
            if p.returncode:
                raise RuntimeError(p.stderr[-2000:])
            return p.stdout

        output = timed("CLI harmless task and memory indexing", lambda: run_cli("task", "--local", "--deny-flagged", "--project", "smoke", "-d", TASK))
        assert MODEL in output and "[ok] $" in output and "\n    hive-nvidia-ok" in output, output
        timed("memory reindex", lambda: run_cli("memory", "reindex"))
        timed("memory query", lambda: run_cli("search", "hive-nvidia-ok", "--project", "smoke"))
        db = sqlite3.connect(root / "memory.db")
        rows = db.execute("SELECT provider, model, dim FROM rag_chunks").fetchall()
        assert rows and all(p == "nvidia" and m == EMBED and d > 0 for p, m, d in rows), rows
        db.close()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env.update(HIVE_CONFIG_ROOT=tmp, HIVE_WEB_ADDR=f"127.0.0.1:{port}", HIVE_WEB_PASSWORD="smoke-password", HIVE_WEB_STATIC=str(ROOT / "hive-web/static"), RUST_LOG="warn")
        with (root / "web.log").open("w+") as log:
            web = subprocess.Popen([str(ROOT / "target/debug/hive-web")], cwd=tmp, env=env, stdout=log, stderr=log)
            try:
                for _ in range(100):
                    try:
                        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=420)
                        connection.request("GET", "/api/health")
                        connection.getresponse().read()
                        break
                    except OSError:
                        time.sleep(0.1)
                else:
                    raise RuntimeError("web startup failed")
                connection.request("POST", "/login", "password=smoke-password", {"Content-Type": "application/x-www-form-urlencoded"})
                response = connection.getresponse()
                cookie = response.getheader("Set-Cookie").split(";", 1)[0]
                response.read()

                def web_chat():
                    connection.request("POST", "/api/chat", json.dumps({"message": TASK}), {"Cookie": cookie, "Content-Type": "application/json"})
                    response = connection.getresponse()
                    payload = response.read().decode()
                    assert response.status == 200, payload
                    reply = json.loads(payload)
                    assert reply["run"]["provider"] == "nvidia", reply
                    assert reply["run"]["model"] == MODEL, reply
                    assert not reply["result"]["awaiting_approval"], reply
                    assert any(outcome["status"] == "executed" and "hive-nvidia-ok" in outcome["output"] for outcome in reply["result"]["outcomes"]), reply
                timed("authenticated web chat harmless task", web_chat)
            finally:
                web.terminate()
                web.wait(timeout=10)
    if not args.live:
        assert all(body.get("chat_template_kwargs") == {"enable_thinking": True} for path, body in records if path.endswith("completions"))
        print(f"Mock requests checked: {len(records)}")
finally:
    if mock:
        mock.shutdown()
