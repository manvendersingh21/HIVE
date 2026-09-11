#!/usr/bin/env python3
"""Hive remote runner v1. Control plane is SSH + a private SQLite journal.

Never restart a missing process automatically: an interrupted tool may have had
side effects. The persistent process owns the native conversation and approvals.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import time
import uuid

VERSION = 1
AGENTS = ('claude', 'codex', 'opencode', 'agy')
BASE = Path.home() / '.hive' / 'runs'


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'))


def executable(name):
    found = shutil.which(name)
    if found:
        return found
    for prefix in (Path.home()/'.local/bin', Path.home()/'.opencode/bin', Path('/opt/homebrew/bin'), Path('/usr/local/bin')):
        path = prefix/name
        if path.is_file() and os.access(path, os.X_OK):
            return str(path)
    return None


def capture(args, timeout=8, include_stderr=False):
    try:
        p = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
        return p.returncode, (p.stdout + ('\n' + p.stderr if include_stderr else '')).strip()
    except (OSError, subprocess.TimeoutExpired):
        return -1, ''


def probe():
    records = []
    for name in AGENTS:
        path = executable(name)
        record = dict(agent=name, executable=path, version=None, authentication='unknown',
                      models=[], runtime_requirements=['python3', 'tmux'], controls=[],
                      runtime_ready=False, verified_at=int(time.time()), invocation=None)
        if path:
            record['version'] = capture([path, '--version'])[1][:200]
            if name in ('claude', 'codex'):
                code, output = capture([path, 'auth', 'status'] if name == 'claude' else [path, 'login', 'status'])
                if name == 'claude':
                    try:
                        record['authentication'] = 'authenticated' if json.loads(output).get('loggedIn') else 'login-required'
                    except ValueError:
                        pass
                else:
                    # Codex writes login status on stderr; never retain raw auth output.
                    try:
                        p = subprocess.run([path, 'login', 'status'], capture_output=True, text=True, timeout=8)
                        record['authentication'] = 'authenticated' if p.returncode == 0 else 'login-required'
                    except (OSError, subprocess.TimeoutExpired):
                        pass
            if name == 'claude':
                record['runtime_requirements'] += ['Python >=3.10 + claude-agent-sdk OR Node.js + @anthropic-ai/claude-agent-sdk']
                record['runtime_ready'] = bool(executable('node') and (Path(__file__).parent/'node_modules/@anthropic-ai/claude-agent-sdk').is_dir())
                sdk_python = Path(__file__).parent/'.sdk/bin/python'
                record['runtime_ready'] = record['runtime_ready'] or sdk_python.exists()
                record['controls'] = ['pre-tool-hook', 'permission-callback', 'persistent-stream']
                if record['runtime_ready']:
                    code, models = capture(([str(sdk_python), str(Path(__file__).with_name('claude_python.py'))] if sdk_python.exists() else [executable('node'), str(Path(__file__).with_name('claude.mjs'))])+['--models',path], timeout=15)
                    if code == 0:
                        try:
                            record['models'] = json.loads(models)
                        except ValueError:
                            pass
            elif name == 'codex':
                record['runtime_ready'] = capture([path, 'app-server', '--help'])[0] == 0
                record['controls'] = ['native-approval', 'workspace-sandbox', 'persistent-thread']
                if record['runtime_ready']:
                    try:
                        record['models'] = asyncio.run(discover_codex(path))
                    except Exception:
                        pass
            elif name == 'agy':
                record['controls'] = ['pre-tool-hook', 'persistent-stream']
                # The hook capability is verified independently before rollout.
                record['runtime_ready'] = '--input-format' in capture([path, '--help'], include_stderr=True)[1]
            else:
                record['controls'] = ['native-approval', 'persistent-session']
                record['runtime_ready'] = capture([path, 'serve', '--help'])[0] == 0
            record['runtime_ready'] = record['runtime_ready'] and bool(executable('tmux'))
        records.append(record)
    return records


class Journal:
    def __init__(self, root):
        self.quiet = False
        self.root = Path(root)
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(self.root, 0o700)
        self.db = sqlite3.connect(str(self.root/'journal.db'), timeout=30)
        self.db.row_factory = sqlite3.Row
        self.db.executescript('''
        PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE, kind TEXT, payload TEXT);
        CREATE TABLE IF NOT EXISTS inbox (id TEXT PRIMARY KEY, payload TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'queued');
        CREATE TABLE IF NOT EXISTS approvals (id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, action TEXT NOT NULL, reason TEXT NOT NULL, decision TEXT, consumed INTEGER NOT NULL DEFAULT 0);
        ''')
        self.db.commit()

    def set(self, key, value):
        with self.db:
            self.db.execute('INSERT INTO metadata VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value', (key, encode(value)))

    def get(self, key, default=None):
        row = self.db.execute('SELECT value FROM metadata WHERE key=?', (key,)).fetchone()
        return json.loads(row[0]) if row else default

    def emit(self, kind, payload, event_id=None):
        with self.db:
            self.db.execute('INSERT OR IGNORE INTO events(id,kind,payload) VALUES (?,?,?)', (event_id or str(uuid.uuid4()), kind, encode(payload)))
        if not self.quiet:
            print(encode(dict(kind=kind, payload=payload)), flush=True)

    def state(self, state):
        self.set('state', state)
        self.emit('state', dict(state=state))

    def enqueue(self, message):
        with self.db:
            row = self.db.execute('SELECT payload FROM inbox WHERE id=?', (message['id'],)).fetchone()
            if row and row[0] != encode(message):
                raise ValueError('Message ID reused with different content')
            self.db.execute('INSERT OR IGNORE INTO inbox(id,payload) VALUES (?,?)', (message['id'], encode(message)))

    def snapshot(self, after=0):
        pid = self.get('pid')
        if pid:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                self.set('state', 'disconnected')
        return dict(metadata={r['key']: json.loads(r['value']) for r in self.db.execute('SELECT * FROM metadata')},
                    events=[dict(seq=r['seq'], id=r['id'], kind=r['kind'], payload=json.loads(r['payload'])) for r in self.db.execute('SELECT * FROM events WHERE seq>? ORDER BY seq LIMIT 300', (after,))],
                    approvals=[dict(r) for r in self.db.execute('SELECT * FROM approvals WHERE consumed=0')])

    def pending(self, action, reason):
        fingerprint = hashlib.sha256(encode(action).encode()).hexdigest()
        row = self.db.execute('SELECT id FROM approvals WHERE fingerprint=? AND consumed=0', (fingerprint,)).fetchone()
        if row:
            return row[0]
        ident = str(uuid.uuid4())
        with self.db:
            self.db.execute('INSERT INTO approvals(id,fingerprint,action,reason) VALUES (?,?,?,?)', (ident, fingerprint, encode(action), reason))
        self.state('awaiting-approval')
        self.emit('approval', dict(id=ident, action=action, reason=reason, fingerprint=fingerprint))
        return ident

    def decide(self, ident, fingerprint, decision):
        if decision not in ('continue', 'stop'):
            raise ValueError('Decision must be continue or stop')
        with self.db:
            row = self.db.execute('SELECT * FROM approvals WHERE id=?', (ident,)).fetchone()
            if not row or row['fingerprint'] != fingerprint:
                raise ValueError('Approval does not match the exact pending action')
            if row['decision']:
                if row['decision'] != decision:
                    raise ValueError('Approval already decided')
                return
            self.db.execute('UPDATE approvals SET decision=? WHERE id=?', (decision, ident))

    def consume(self, ident):
        with self.db:
            row = self.db.execute('SELECT decision FROM approvals WHERE id=? AND consumed=0', (ident,)).fetchone()
            if row and row[0]:
                self.db.execute('UPDATE approvals SET consumed=1 WHERE id=? AND consumed=0', (ident,))
                return row[0]
        return None


def shell_parts(command):
    """Parse a deliberately small shell subset. Unsupported syntax asks."""
    if re.search(r'[`$\r\n]', command):
        raise ValueError('Shell expansion or control syntax requires review')
    words = shlex.split(command)
    if len(words) == 3 and words[0] in ('bash','sh','/bin/bash','/usr/bin/bash','/bin/sh','/usr/bin/sh') and words[1] in ('-c','-lc'):
        return shell_parts(words[2])
    lexer = shlex.shlex(command, posix=True, punctuation_chars=';&|<>()')
    lexer.whitespace_split = True
    lexer.commenters = ''
    tokens = list(lexer)
    parts, current = [], []
    i = 0
    while i < len(tokens):
        token = tokens[i]
        if token in (';', '&&', '||', '|'):
            if not current:
                raise ValueError('Empty shell segment')
            parts.append(current)
            current = []
        elif token in ('>', '>>', '>&'):
            if i+1 >= len(tokens):
                raise ValueError('Invalid redirection')
            target = tokens[i+1]
            if not (target == '/dev/null' or token == '>&' and target in ('1','2')):
                raise ValueError('File redirection requires review')
            if current and current[-1] in ('1','2'):
                current.pop()
            i += 1
        elif token in ('&', '<', '<<', '<<<', '(', ')'):
            raise ValueError('Background jobs and input redirection require review')
        else:
            current.append(token)
        i += 1
    if current:
        parts.append(current)
    return parts


# Unknown tools/commands ask. Shell metacharacters and interpreters never pass
# through the routine-command allowlist. Native sandboxes remain enabled.
def policy(tool, args, workspace):
    if tool in ('ListAgents', 'TodoWrite', 'ToolSearch', 'mcp__hive__peer'):
        return None
    root = Path(workspace).resolve()
    def inside(path):
        candidate = Path(path).expanduser()
        if not candidate.is_absolute():
            candidate = root/candidate
        try:
            rel = candidate.resolve().relative_to(root)
            return not any(p in ('.agents', '.claude', '.codex', '.git') for p in rel.parts)
        except ValueError:
            return False
    for key in ('file_path', 'path', 'cwd', 'Cwd', 'TargetFile'):
        if args.get(key) and not inside(args[key]):
            return 'Action accesses a path outside the assignment or protected controls'
    if tool == 'item/fileChange/requestApproval' and isinstance(args.get('changes'), list) and args['changes'] and not args.get('grantRoot'):
        for change in args['changes']:
            if not inside(change.get('path', '/')):
                return 'File change is outside the assignment or changes protected controls'
            if change.get('kind', {}).get('type') == 'delete' or sum(line.startswith('-') and not line.startswith('---') for line in change.get('diff', '').splitlines()) > 50:
                return 'File deletion or bulk removal requires review'
        return None
    command = args.get('command', args.get('CommandLine'))
    if command is not None:
        if not isinstance(command, str):
            return 'Command is not a string'
        try:
            parts = shell_parts(command)
        except ValueError as error:
            return str(error)
        if len(parts) != 1:
            for words in parts:
                reason = policy(tool, dict(command=shlex.join(words)), workspace)
                if reason:
                    return reason
            return None if parts else 'Empty command'
        words = parts[0]
        if not words:
            return 'Empty command'
        # Native Codex exposes an explicit shell wrapper. Inspect its literal
        # body using the same deterministic parser, never an eval or execution.
        if len(words) == 3 and words[0] in ('/usr/bin/bash', '/bin/bash', '/bin/sh', '/bin/zsh') and words[1] in ('-c', '-lc'):
            return policy(tool, dict(command=words[2]), workspace)
        if words[0] == 'nl' and len(words) == 3 and words[1] == '-ba' and inside(words[2]):
            return None
        # The task-scoped peer command validates both run IDs itself.
        if len(words) == 11 and words[0] in ('python3', sys.executable) and Path(words[1]).resolve() == Path(__file__).resolve() and words[2] == 'peer':
            flags = dict(zip(words[3::2], words[4::2]))
            if set(flags) == {'--run-id', '--to', '--kind', '--body'} and flags['--kind'] in ('question','answer','agreement','deployment'):
                return None
        # Routine test/build and isolated dependencies are explicitly authorized.
        # Shell composition, interpreter snippets and arbitrary install targets
        # still require review; native containment stays enabled.
        paths_safe = all(not w.startswith(('/', '~')) and '..' not in Path(w).parts for w in words[1:])
        if paths_safe:
            if words in (['python3','-m','venv','.venv'], ['python3','-m','unittest'], ['python3','-m','unittest','discover'], ['cargo','test'], ['cargo','check'], ['npm','test'], ['npm','run','build']):
                return None
            if words[0] in ('python3', '.venv/bin/python', '.venv/bin/python3') and len(words) == 2 and Path(words[1]).name.startswith('test_') and words[1].endswith('.py') and inside(words[1]):
                return None
            if words[:2] == ['.venv/bin/pip','install'] and len(words) == 4 and words[2] == '-r' and inside(words[3]):
                return None
            flags = {'pwd':set(), 'ls':{'-l','-a','-la','-al'}, 'rg':{'-n','-l','--files','-i'}, 'cat':set(), 'head':set(), 'tail':set(), 'wc':{'-l','-c'}, 'sha256sum':set(), 'shasum':set()}
            if words[0] in flags and all((w in flags[words[0]] if w.startswith('-') else inside(w)) for w in words[1:]):
                return None
        if words[:3] == ['python3','-m','py_compile'] and len(words)>3 and all(inside(p) for p in words[3:]):
            return None
        if words[0] == 'sort' and len(words)==1:
            return None
        if words[0] == 'timeout' and len(words)>2 and words[1].isdigit() and int(words[1])<=30:
            return policy(tool, dict(command=shlex.join(words[2:])), workspace)
        if words[0] == 'find' and words[1:] in (['.','-maxdepth','2','-type','f','-print'], ['.','-type','f','-print']):
            return None
        if words[:2]==['command','-v'] and len(words)==3 and re.fullmatch(r'[A-Za-z0-9_.-]+',words[2]):
            return None
        if words[0]=='rg':
            args_iter=iter(words[1:]); positional=[]; invalid=False
            for word in args_iter:
                if word=='-g':
                    next(args_iter, None)
                elif word.startswith('-') and word not in ('-n','-l','-i','--files','--hidden'):
                    invalid=True
                elif not word.startswith('-'):
                    positional.append(word)
            files=positional if '--files' in words else positional[1:]
            if not invalid and all(inside(p) or Path(p).resolve()==Path(__file__).resolve() for p in files):
                return None
        if words[0] == 'ping' and len(words)==4 and words[1]=='-c' and words[2] in ('1','2','3'):
            import ipaddress
            try:
                address=ipaddress.ip_address(words[3])
                if address.is_private or address in ipaddress.ip_network('100.64.0.0/10'):
                    return None
            except ValueError:
                pass
        if words[0] in ('echo', 'true'):
            return None
        if words[0] == 'printf' and len(words)>1 and not words[1].startswith('-'):
            return None
        if words[0] in ('which', 'uname') and all(re.fullmatch(r'[-A-Za-z0-9_]+', w) for w in words[1:]):
            return None
        if words in (['python3','--version'], ['node','--version'], ['tailscale','ip'], ['tailscale','ip','-4'], ['tailscale','status'], ['tailscale','status','--json'], ['launchctl','list'], ['brew','services','list'], ['systemctl','is-active','tailscaled'], ['ifconfig']):
            return None
        if words[0] == 'head' and len(words)==2 and re.fullmatch(r'-[0-9]+', words[1]):
            return None
        if words[0] == 'head' and len(words)==3 and words[1] in ('-c','-n') and words[2].isdigit():
            return None
        if words[0] == 'grep' and len(words) in (2,3) and not words[-1].startswith('-') and (len(words)==2 or words[1] in ('-i','-n')):
            return None
        if words[0] == 'sed' and len(words) in (3,4) and words[1]=='-n' and re.fullmatch(r'[0-9]+(,[0-9]+)?p', words[2]) and (len(words)==3 or inside(words[3]) or Path(words[3]).resolve()==Path(__file__).resolve()):
            return None
        if words[:2] in (['git','status'], ['git','diff'], ['git','log']) and all(w in ('--short','--stat','--oneline') for w in words[2:]):
            return None
        if words[:2] in (['git', 'status'], ['git', 'diff'], ['git', 'log']):
            if len(words) == 2:
                return None
        return 'Command may execute code, delete data, change permissions, or access resources beyond this workspace'
    if tool in ('Read', 'Glob', 'Grep', 'Write', 'Edit', 'MultiEdit', 'read_file', 'view_file', 'write_to_file', 'replace_file_content') and any(args.get(k) for k in ('file_path', 'path', 'TargetFile')):
        return None
    return 'Tool or requested permissions need explicit review'


async def permission(journal, tool, args, workspace):
    reason = policy(tool, args, workspace)
    if reason is None:
        return True
    ident = journal.pending(dict(tool=tool, arguments=args, workspace=workspace), reason)
    while True:
        decision = journal.consume(ident)
        if decision:
            journal.emit('approval-consumed', dict(id=ident, decision=decision))
            journal.state('working')
            return decision == 'continue'
        await asyncio.sleep(0.5)


class JsonProcess:
    async def start(self, args, cwd, quiet=False):
        self.proc = await asyncio.create_subprocess_exec(*args, cwd=cwd, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL if quiet else sys.stderr, limit=4*1024*1024)
        self.serial = 0
        self.waiters = {}
        self.notifications = asyncio.Queue()
        self.reader = asyncio.create_task(self.read())

    async def read(self):
        try:
            while True:
                raw = await self.proc.stdout.readline()
                if not raw:
                    raise RuntimeError('Native agent disconnected')
                try:
                    message = json.loads(raw)
                except ValueError:
                    continue
                if 'id' in message and 'method' not in message and message['id'] in self.waiters:
                    self.waiters.pop(message['id']).set_result(message)
                else:
                    await self.notifications.put(message)
        except Exception as error:
            for future in self.waiters.values():
                if not future.done():
                    future.set_exception(error)
            self.waiters.clear()
            await self.notifications.put(dict(disconnected=str(error)))

    async def send(self, value):
        self.proc.stdin.write((encode(value)+'\n').encode())
        await self.proc.stdin.drain()

    async def rpc(self, method, params):
        self.serial += 1
        ident = self.serial
        future = asyncio.get_running_loop().create_future()
        self.waiters[ident] = future
        await self.send(dict(id=ident, method=method, params=params))
        reply = await asyncio.wait_for(future, 40)
        if 'error' in reply:
            raise RuntimeError(encode(reply['error']))
        return reply['result']

    async def close(self):
        if self.proc.returncode is None:
            self.proc.terminate()
            try:
                await asyncio.wait_for(self.proc.wait(), 5)
            except asyncio.TimeoutError:
                self.proc.kill()
                await self.proc.wait()
        self.reader.cancel()


async def discover_codex(path):
    process = JsonProcess()
    try:
        await process.start([path, 'app-server'], str(Path.home()), quiet=True)
        await process.rpc('initialize', dict(clientInfo=dict(name='hive-inventory', version='1.0.0')))
        await process.send(dict(method='initialized', params={}))
        result = await process.rpc('model/list', dict(limit=100))
        return [model['model'] for model in result.get('data', [])]
    finally:
        if hasattr(process, 'proc'):
            await process.close()


class Codex(JsonProcess):
    @staticmethod
    def validate_hive_tool(journal, name, args):
        """Validate the same narrow contract at approval and execution time."""
        if not isinstance(args, dict):
            raise ValueError('Tool arguments must be an object')
        assignment = journal.get('assignment', {})
        if name == 'peer':
            if (set(args) != {'to', 'kind', 'text'} or
                    args.get('to') not in [p['id'] for p in assignment.get('peers', [])] or
                    args.get('kind') not in ('question', 'answer', 'agreement', 'deployment') or
                    not isinstance(args.get('text'), str) or not 0 < len(args['text']) <= 16000):
                raise ValueError('Invalid task peer message')
            return None
        if name != 'service':
            raise ValueError('Unsupported Hive tool')
        if args == {'operation': 'status'}:
            return None
        argv = args.get('argv')
        if (set(args) != {'operation', 'argv'} or args.get('operation') != 'start' or
                not isinstance(argv, list) or not 3 <= len(argv) <= 25 or
                any(not isinstance(arg, str) or not arg or len(arg) > 4096 or any(c in arg for c in '\x00\n\r') for arg in argv)):
            raise ValueError('Service requires start with an argv array, or status without argv')
        root = Path(assignment['workspace']).resolve()
        def contained(value):
            path = Path(value)
            path = (path if path.is_absolute() else root/path).resolve()
            try:
                relative = path.relative_to(root)
            except ValueError as error:
                raise ValueError('Service path leaves the assigned workspace') from error
            if any(part in ('.agents', '.claude', '.codex', '.git', '.hive') for part in relative.parts):
                raise ValueError('Service cannot access protected controls')
            return path
        interpreter = Path(argv[0])
        system_python = executable('python3')
        if argv[0] == 'python3' and system_python:
            interpreter = Path(system_python)
        allowed = {str(Path(p).resolve()) for p in (sys.executable, system_python, '/usr/bin/python3') if p}
        local_python = root/'.venv/bin/python'
        local_python3 = root/'.venv/bin/python3'
        lexical_interpreter = interpreter if interpreter.is_absolute() else root/interpreter
        if str(interpreter.resolve()) not in allowed and lexical_interpreter not in (local_python, local_python3):
            raise ValueError('Service requires python3 or the workspace .venv Python')
        if not lexical_interpreter.is_file() or not os.access(lexical_interpreter, os.X_OK):
            raise ValueError('Service Python interpreter is unavailable')
        script = contained(argv[1])
        if script.suffix != '.py' or not script.is_file():
            raise ValueError('Service entry point must be an existing workspace Python script')
        normalized = [str(lexical_interpreter), str(script)]
        values, serve_seen, i = {}, False, 2
        path_flags = ('--secret-file', '--token-file', '--root', '--directory')
        while i < len(argv):
            flag = argv[i]
            if flag == 'serve' and not serve_seen:
                serve_seen = True
                normalized.append(flag)
                i += 1
                continue
            if flag not in ('--bind', '--host', '--port')+path_flags or flag in values or i+1 >= len(argv):
                raise ValueError('Unsupported or repeated service argument')
            value = argv[i+1]
            values[flag] = value
            if flag in path_flags:
                value = str(contained(value))
            normalized.extend([flag, value])
            i += 2
        bind_flags = [flag for flag in ('--bind', '--host') if flag in values]
        if not serve_seen or len(bind_flags) != 1 or values[bind_flags[0]] != '127.0.0.1':
            raise ValueError('Service must explicitly bind only 127.0.0.1')
        if not values.get('--port', '').isdigit() or not 1024 < int(values['--port']) <= 65535:
            raise ValueError('Service requires an unprivileged explicit port')
        token_flags = [flag for flag in ('--secret-file', '--token-file') if flag in values]
        if len(token_flags) != 1:
            raise ValueError('Service requires a workspace authentication token file')
        if not contained(values[token_flags[0]]).is_file():
            raise ValueError('Service authentication token file is unavailable')
        if len([flag for flag in ('--root', '--directory') if flag in values]) != 1:
            raise ValueError('Service requires one explicit workspace storage directory')
        return normalized

    @staticmethod
    def hive_elicitation(journal, params, native):
        metadata = params.get('_meta', {})
        schema = params.get('requestedSchema')
        if (params.get('serverName') != 'hive' or params.get('threadId') != native or
                params.get('mode') != 'form' or not isinstance(metadata, dict) or
                metadata.get('codex_approval_kind') != 'mcp_tool_call' or
                not isinstance(schema, dict) or schema.get('type') != 'object' or
                schema.get('properties') != {} or schema.get('required') not in (None, [])):
            return False
        name = next((name for name in ('peer', 'service') if params.get('message') ==
                     'Allow the hive MCP server to run tool "'+name+'"?'), None)
        try:
            Codex.validate_hive_tool(journal, name, metadata.get('tool_params'))
            return True
        except (ValueError, KeyError, TypeError, OSError):
            return False

    @staticmethod
    def service(journal, args):
        import fcntl
        argv = Codex.validate_hive_tool(journal, 'service', args)
        assignment = journal.get('assignment')
        name = 'hive-service-'+str(uuid.UUID(assignment['id']))
        with open(journal.root/'service.lock', 'a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            receipt = journal.get('service_receipt')
            if receipt:
                if argv is not None and receipt['argv'] != argv:
                    raise ValueError('Service command changed; review the existing service before replacement')
                code, output = capture([executable('tmux'), 'list-panes', '-t', '='+name,
                    '-F', '#{pane_id}\t#{pane_pid}\t#{pane_current_path}\t#{pane_start_command}\t#{pane_dead}'])
                panes = output.splitlines()
                if code != 0 or len(panes) != 1:
                    return dict(state='disconnected', receipt=receipt, reason='Owned service pane is missing; no automatic relaunch')
                pane = panes[0].split('\t')
                if (len(pane) != 5 or pane[0] != receipt.get('pane_id') or pane[1] != str(receipt.get('pid')) or
                        pane[2] != assignment['workspace'] or shlex.split(pane[3]) != receipt['argv']):
                    raise ValueError('Service ownership or command changed; reconciliation required')
                digest = hashlib.sha256(Path(receipt['argv'][1]).read_bytes()).hexdigest()
                return dict(state='completed' if pane[4] == '1' else 'running', receipt=receipt,
                            script_changed=digest != receipt['script_sha256'])
            if argv is None:
                return dict(state='not-started')
            # No receipt means an existing session is unrelated/uncertain. Never
            # replace it, even if its name happens to match this assignment.
            if capture([executable('tmux'), 'has-session', '-t', '='+name])[0] == 0:
                raise ValueError('Service session already exists without an ownership receipt')
            receipt = dict(argv=argv, workspace=assignment['workspace'], tmux_name=name,
                fingerprint=hashlib.sha256(encode(argv).encode()).hexdigest(),
                script_sha256=hashlib.sha256(Path(argv[1]).read_bytes()).hexdigest(), state='launching')
            journal.set('service_receipt', receipt)
            # tmux executes multiple command arguments directly; no shell text
            # from the worker is evaluated. Checkpoint precedes the side effect.
            process = subprocess.run([executable('tmux'), 'new-session', '-d', '-P', '-F', '#{pane_id}\t#{pane_pid}',
                '-s', name, '-c', assignment['workspace'], *argv], capture_output=True, text=True, timeout=15, check=True)
            pane = process.stdout.strip().split('\t')
            if len(pane) != 2 or not pane[1].isdigit():
                raise ValueError('Service launch receipt is uncertain; inspect the owned tmux session')
            receipt.update(pane_id=pane[0], pid=int(pane[1]), state='started')
            journal.set('service_receipt', receipt)
            journal.emit('service', receipt)
            return dict(state='started', receipt=receipt)

    async def connect(self, assignment, journal):
        self.a, self.j = assignment, journal
        self.items = {}
        await self.start([executable('codex'), 'app-server'], assignment['workspace'])
        await self.rpc('initialize', dict(clientInfo=dict(name='hive', version='1.0.0'), capabilities=dict(experimentalApi=True)))
        await self.send(dict(method='initialized', params={}))
        models = await self.rpc('model/list', dict(limit=100))
        available = [m['model'] for m in models.get('data', [])]
        journal.set('available_models', available)
        requested = assignment.get('model')
        if requested and requested not in available:
            raise RuntimeError('Unavailable Codex model: '+requested)
        params = dict(cwd=assignment['workspace'], approvalPolicy='untrusted', sandbox='workspace-write')
        params['config'] = {'mcp_servers.hive': {'command': sys.executable, 'args': [str(Path(__file__).resolve()), 'mcp', '--run-id', assignment['id']]}}
        if requested:
            params['model'] = requested
        native = journal.get('native_conversation_id')
        if native:
            params['threadId'] = native
        result = await self.rpc('thread/resume' if native else 'thread/start', params)
        self.native = result['thread']['id']
        journal.set('native_conversation_id', self.native)
        journal.set('actual_model', result.get('model'))

    async def turn(self, prompt):
        await self.rpc('turn/start', dict(threadId=self.native, input=[dict(type='text', text=prompt)]))
        while True:
            event = await self.notifications.get()
            if 'disconnected' in event:
                raise RuntimeError(event['disconnected'])
            method, params = event.get('method', ''), event.get('params', {})
            if method in ('item/started', 'item/completed') and params.get('item', {}).get('id'):
                self.items[params['item']['id']] = params['item']
            if 'id' in event and method:
                if method == 'mcpServer/elicitation/request':
                    allowed = self.hive_elicitation(self.j, params, self.native)
                    await self.send(dict(id=event['id'], result=dict(action='accept' if allowed else 'decline', content={} if allowed else None)))
                elif method == 'item/tool/call' and params.get('tool') == 'hive_peer':
                    args=params.get('arguments', {})
                    if isinstance(args,str): args=json.loads(args)
                    valid=isinstance(args,dict) and args.get('to') in [p['id'] for p in self.j.get('assignment').get('peers', [])] and args.get('kind') in ('question','answer','agreement','deployment') and isinstance(args.get('text'),str) and 0<len(args['text'])<=16000
                    if valid:
                        self.j.emit('peer',args)
                        if args['kind']=='question': self.j.state('waiting-for-peer')
                    await self.send(dict(id=event['id'],result=dict(success=valid,contentItems=[dict(type='inputText',text='Peer message durably queued. End this turn briefly if you need the reply.' if valid else 'Invalid task peer message')])) )
                elif method in ('item/commandExecution/requestApproval', 'item/fileChange/requestApproval'):
                    action = dict(params)
                    if method == 'item/fileChange/requestApproval':
                        action['changes'] = self.items.get(params.get('itemId'), {}).get('changes')
                    allowed = await permission(self.j, method, action, self.a['workspace'])
                    await self.send(dict(id=event['id'], result=dict(decision='accept' if allowed else ('decline' if 'decline' in params.get('availableDecisions', ['decline']) else 'cancel'))))
                else:
                    await self.send(dict(id=event['id'], error=dict(code=-32601, message='Unsupported control; action denied')))
            if method:
                self.j.emit('native', event)
            if method == 'turn/completed':
                if params.get('turn', {}).get('status') != 'completed':
                    raise RuntimeError(encode(params.get('turn', {})))
                return


class Claude(JsonProcess):
    async def connect(self, assignment, journal):
        self.a, self.j = assignment, journal
        sdk_python=Path(__file__).parent/'.sdk/bin/python'
        command=[str(sdk_python), str(Path(__file__).with_name('claude_python.py'))] if sdk_python.exists() else [executable('node'), str(Path(__file__).with_name('claude.mjs'))]
        await self.start(command, assignment['workspace'])
        await self.send(dict(type='configure', assignment=assignment, resume=journal.get('native_conversation_id')))

    async def turn(self, prompt):
        await self.send(dict(type='prompt', text=prompt))
        while True:
            event = await self.notifications.get()
            if 'disconnected' in event:
                raise RuntimeError(event['disconnected'])
            if event.get('type') == 'permission':
                allowed = await permission(self.j, event['tool'], event['input'], self.a['workspace'])
                await self.send(dict(type='decision', id=event['request_id'], allowed=allowed))
            elif event.get('type') == 'peer':
                if event.get('to') not in [p['id'] for p in self.j.get('assignment').get('peers', [])]:
                    raise RuntimeError('Native peer tool attempted to cross task boundary')
                self.j.emit('peer', dict(to=event['to'],kind=event['kind'],text=event['text']))
                if event['kind']=='question': self.j.state('waiting-for-peer')
            elif event.get('type') == 'models':
                self.j.set('available_models', event['models'])
            elif event.get('type') == 'error':
                raise RuntimeError(event['message'])
            else:
                self.j.emit('native', event)
                if event.get('session_id'):
                    self.j.set('native_conversation_id', event['session_id'])
                if event.get('model'):
                    self.j.set('actual_model', event['model'])
                if event.get('type') == 'result':
                    if event.get('is_error'):
                        raise RuntimeError(encode(event))
                    return


class Agy(JsonProcess):
    async def connect(self, assignment, journal):
        # Hook configuration must be isolated to this fresh workspace.
        self.a, self.j = assignment, journal
        directory = Path(assignment['workspace'])/'.agents'
        directory.mkdir(exist_ok=True)
        hooks = directory/'hooks.json'
        command = shlex.join([sys.executable, str(Path(__file__).resolve()), 'hook', '--run-id', assignment['id']])
        expected_hooks = {'hive': {'PreToolUse': [dict(matcher='*', hooks=[dict(type='command', command=command, timeout=10)])]}}
        if hooks.exists():
            try:
                existing_hooks = json.loads(hooks.read_text())
            except (ValueError, OSError) as error:
                raise RuntimeError('Existing AGY hooks require review before integration') from error
            if existing_hooks != expected_hooks:
                raise RuntimeError('Existing AGY hooks require review before integration')
        else:
            hooks.write_text(encode(expected_hooks))
        args = [executable('agy'), '--input-format', 'stream-json', '--output-format', 'stream-json']
        if assignment.get('model'):
            args += ['--model', assignment['model']]
        if journal.get('native_conversation_id'):
            args += ['--conversation', journal.get('native_conversation_id')]
        await self.start(args, assignment['workspace'])

    async def turn(self, prompt):
        await self.send(dict(event='user', message=dict(content=prompt)))
        while True:
            event = await self.notifications.get()
            if 'disconnected' in event:
                raise RuntimeError(event['disconnected'])
            self.j.emit('native', event)
            payload = event.get(event.get('event'), {})
            conversation_id = event.get('conversation_id') or payload.get('conversation_id')
            if conversation_id:
                self.j.set('native_conversation_id', conversation_id)
            if event.get('event') == 'init' and payload.get('model'):
                self.j.set('actual_model', payload['model'])
            if event.get('event') == 'result':
                if payload.get('status') != 'SUCCESS':
                    raise RuntimeError('AGY '+str(payload.get('status', 'missing result status'))+': '+payload.get('error', 'Turn did not complete successfully'))
                return


class OpenCode:
    async def http(self, method, path, body=None):
        import urllib.request
        def request():
            req = urllib.request.Request(self.url+path, data=None if body is None else encode(body).encode(),
                                         headers={'Authorization': self.auth, 'Content-Type': 'application/json'}, method=method)
            with urllib.request.urlopen(req, timeout=300) as response:
                data = response.read()
                return json.loads(data) if data else None
        return await asyncio.to_thread(request)

    async def connect(self, assignment, journal):
        import base64
        import socket
        self.a, self.j = assignment, journal
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        password = uuid.uuid4().hex + uuid.uuid4().hex
        self.url = 'http://127.0.0.1:'+str(port)
        self.auth = 'Basic '+base64.b64encode(('opencode:'+password).encode()).decode()
        env = dict(os.environ, OPENCODE_SERVER_PASSWORD=password,
                   OPENCODE_CONFIG_CONTENT=encode({'permission': {'*': 'ask'}, 'agent': {'build': {'permission': {'*': 'ask'}}}}))
        self.proc = await asyncio.create_subprocess_exec(executable('opencode'), 'serve', '--pure', '--hostname', '127.0.0.1', '--port', str(port),
                    cwd=assignment['workspace'], env=env, stdout=asyncio.subprocess.DEVNULL, stderr=sys.stderr)
        for attempt in range(40):
            try:
                doc = await self.http('GET', '/doc')
                break
            except Exception:
                await asyncio.sleep(.25)
        else:
            raise RuntimeError('OpenCode local server did not become ready')
        required_paths = ('/permission/{requestID}/reply', '/session/{sessionID}/prompt_async',
                          '/session/status', '/session/{sessionID}/message/{messageID}')
        if any(path not in doc['paths'] for path in required_paths):
            raise RuntimeError('Installed OpenCode session or permission API is unsupported')
        providers = await self.http('GET', '/provider')
        available = [provider['id']+'/'+model for provider in providers.get('all', [])
                     if provider['id'] in providers.get('connected', []) for model in provider.get('models', {})]
        journal.set('available_models', available)
        if not available:
            raise RuntimeError('OpenCode has no connected model provider; log in on this device')
        model = assignment.get('model')
        if model and model not in available:
            raise RuntimeError('Unavailable OpenCode model: '+model)
        if not model:
            model = next((provider+'/'+model for provider, model in providers.get('default', {}).items() if provider+'/'+model in available), available[0])
        self.model = dict(zip(('providerID', 'modelID'), model.split('/', 1)))
        self.native = journal.get('native_conversation_id')
        if not self.native:
            session = await self.http('POST', '/session', {'title': assignment['objective'][:100], 'permission': [{'permission': '*', 'pattern': '*', 'action': 'ask'}]})
            self.native = session['id']
            journal.set('native_conversation_id', self.native)
        journal.set('actual_model', model)

    async def turn(self, prompt):
        if self.j.get('opencode_pending_message'):
            raise RuntimeError('Prior OpenCode prompt is uncertain; reconcile its message before submitting another')
        # Match OpenCode's ascending IDs: low 48 bits of milliseconds*4096
        # plus a per-timestamp counter, followed by fourteen random characters.
        timestamp = (int(time.time()*1000)*0x1000 + 1) & ((1 << 48)-1)
        message_id = 'msg_'+format(timestamp, '012x')+uuid.uuid4().hex[:14]
        self.j.set('opencode_pending_message', message_id)
        # Checkpoint before sending. Even a lost HTTP response must not resubmit
        # this prompt: the server may already be executing it.
        await self.http('POST', '/session/'+self.native+'/prompt_async',
                        dict(messageID=message_id, model=self.model, parts=[dict(type='text', text=prompt)]))
        seen = set()
        while True:
            for request in await self.http('GET', '/permission'):
                if request.get('sessionID') != self.native:
                    continue
                reference = request.get('tool', {})
                if not reference.get('messageID') or not reference.get('callID'):
                    await self.http('POST', '/permission/'+request['id']+'/reply', {'reply': 'reject'})
                    raise RuntimeError('OpenCode permission lacks an exact tool reference; action denied')
                message = await self.http('GET', '/session/'+self.native+'/message/'+reference['messageID'])
                matches = [part for part in message.get('parts', []) if part.get('type') == 'tool'
                           and part.get('callID') == reference['callID']
                           and part.get('messageID') == reference['messageID']
                           and part.get('sessionID') == self.native]
                if len(matches) != 1 or not isinstance(matches[0].get('state', {}).get('input'), dict):
                    await self.http('POST', '/permission/'+request['id']+'/reply', {'reply': 'reject'})
                    raise RuntimeError('OpenCode permission tool input is unavailable; action denied')
                part = matches[0]
                tool = part['tool']
                arguments = dict(part['state']['input'], _native_permission=request)
                if request['permission'] in ('bash', 'edit', 'read', 'glob', 'grep'):
                    tool = {'bash': 'Bash', 'edit': 'Edit', 'write': 'Write', 'read': 'Read', 'glob': 'Glob', 'grep': 'Grep'}.get(tool, tool)
                    if 'filePath' in arguments:
                        arguments['file_path'] = arguments['filePath']
                    if 'workdir' in arguments:
                        arguments['cwd'] = arguments['workdir']
                else:
                    # Broader grants such as external_directory remain explicit
                    # even when the associated shell command looks harmless.
                    tool = 'opencode-permission/'+request['permission']
                    arguments = dict(request=request, tool_input=arguments)
                allowed = await permission(self.j, tool, arguments, self.a['workspace'])
                await self.http('POST', '/permission/'+request['id']+'/reply', {'reply': 'once' if allowed else 'reject'})
            messages = await self.http('GET', '/session/'+self.native+'/message?limit=100')
            terminal = []
            for message in messages:
                digest = hashlib.sha256(encode(message).encode()).hexdigest()
                if digest not in seen:
                    seen.add(digest)
                    self.j.emit('native', message)
                info = message.get('info', {})
                if info.get('parentID') != message_id or info.get('role') != 'assistant':
                    continue
                if info.get('error'):
                    raise RuntimeError(encode(info['error']))
                if info.get('providerID') and info.get('modelID'):
                    self.j.set('actual_model', info['providerID']+'/'+info['modelID'])
                if info.get('time', {}).get('completed') and info.get('finish') not in (None, 'tool-calls', 'unknown'):
                    terminal.append(message)
            statuses = await self.http('GET', '/session/status')
            if terminal and statuses.get(self.native, {}).get('type', 'idle') == 'idle':
                self.j.set('opencode_pending_message', None)
                return
            await asyncio.sleep(.5)

    async def close(self):
        if self.proc.returncode is None:
            self.proc.terminate()
            await self.proc.wait()


def recovery_owner_lock(journal):
    import fcntl
    lock = open(journal.root/'owner.lock', 'a')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        return None
    return lock


def recovery_processes(journal):
    """A failed runner may retain an idle pane; native descendants must be gone."""
    code, output = capture(['ps', '-eo', 'pid=,ppid='])
    if code != 0:
        raise ValueError('Cannot verify native process termination')
    parents = {int(pid): int(parent) for pid, parent in (line.split() for line in output.splitlines())}
    owner = journal.get('pid')
    descendants = set()
    frontier = {owner}
    while frontier:
        frontier = {pid for pid, parent in parents.items() if parent in frontier and pid not in descendants}
        descendants.update(frontier)
    return dict(pid=owner, retained_pane_process=owner in parents, native_descendants=sorted(descendants))


def recovery_pane(journal):
    assignment = journal.get('assignment', {})
    ident = str(uuid.UUID(assignment['id']))
    tmux = executable('tmux')
    if not tmux:
        raise ValueError('tmux is unavailable')
    code, output = capture([tmux, 'list-panes', '-s', '-t', '=hive-agent-'+ident, '-F', '#{pane_id} #{pane_pid}'])
    panes = output.splitlines()
    if code != 0 or len(panes) != 1:
        raise ValueError('Recovery requires the original session with exactly one pane')
    pane, pid = panes[0].split()
    if not re.fullmatch(r'%\d+', pane) or int(pid) != journal.get('pid'):
        raise ValueError('Original pane ownership cannot be verified')
    return dict(id=pane, pid=int(pid), tmux_name='hive-agent-'+ident)


def recovery_snapshot(journal, owner_lock=None):
    acquired = recovery_owner_lock(journal) if owner_lock is None else None
    owner_available = owner_lock is not None or acquired is not None
    blockers = []
    try:
        if not owner_available:
            blockers.append('A live runner owns this conversation')
        metadata = {r['key']: json.loads(r['value']) for r in journal.db.execute('SELECT * FROM metadata')}
        if not metadata.get('native_conversation_id'):
            blockers.append('Native conversation identity is missing; automatic recreation is forbidden')
        if metadata.get('state') not in ('failed', 'disconnected', 'needs-setup'):
            blockers.append('Run is not in a recoverable failure state')
        if metadata.get('resume_authorization'):
            blockers.append('A prior resume is uncertain; inspect the original pane before further recovery')
        if metadata.get('opencode_pending_message'):
            blockers.append('An OpenCode native request is unresolved; inspect native message/status before recovery')
        approvals = [dict(r) for r in journal.db.execute('SELECT * FROM approvals WHERE consumed=0 ORDER BY id')]
        if approvals:
            blockers.append('Unconsumed approvals must be resolved before recovery')
        inbox = [dict(id=r['id'], state=r['state'], payload=json.loads(r['payload'])) for r in journal.db.execute('SELECT * FROM inbox ORDER BY rowid')]
        if not any(m['id'] == 'initial' for m in inbox):
            blockers.append('Initial delivery checkpoint is missing')
        events = [dict(seq=r['seq'], id=r['id'], kind=r['kind'], payload=json.loads(r['payload'])) for r in journal.db.execute('SELECT * FROM (SELECT * FROM events ORDER BY seq DESC LIMIT 100) ORDER BY seq')]
        processes, pane = None, None
        for probe, key in ((recovery_processes, 'processes'), (recovery_pane, 'pane')):
            try:
                value = probe(journal)
                if key == 'processes':
                    processes = value
                    if value['native_descendants']:
                        blockers.append('Native child processes are still alive')
                else:
                    pane = value
            except (ValueError, OSError) as error:
                blockers.append(str(error))
        snapshot = dict(version=1, metadata=metadata, events=events, inbox=inbox, approvals=approvals,
                        processes=processes, pane=pane, owner_available=owner_available)
        return dict(snapshot=snapshot, fingerprint=hashlib.sha256(encode(snapshot).encode()).hexdigest(),
                    blockers=blockers, can_resume=not blockers,
                    acknowledge_ids=[m['id'] for m in inbox if m['state'] == 'delivering'])
    finally:
        if acquired is not None:
            acquired.close()


def reconcile(journal, request):
    """Explicitly acknowledge uncertain deliveries and resume only the same native ID."""
    reason, evidence = request.get('reason'), request.get('evidence')
    if not all(isinstance(s, str) and 1 <= len(s.strip()) <= 16000 for s in (reason, evidence)):
        raise ValueError('Recovery reason and inspected side-effect evidence are required')
    acknowledgments = request.get('acknowledge_ids')
    if not isinstance(acknowledgments, list) or not all(isinstance(s, str) for s in acknowledgments):
        raise ValueError('Explicit uncertain message acknowledgments required')
    lock = recovery_owner_lock(journal)
    if lock is None:
        raise ValueError('A live runner owns this conversation')
    try:
        journal.db.execute('BEGIN IMMEDIATE')
        inspection = recovery_snapshot(journal, owner_lock=lock)
        if request.get('fingerprint') != inspection['fingerprint']:
            raise ValueError('Recovery snapshot changed; inspect it again')
        if not inspection['can_resume']:
            raise ValueError('; '.join(inspection['blockers']))
        if sorted(acknowledgments) != sorted(inspection['acknowledge_ids']):
            raise ValueError('Acknowledge every uncertain message exactly once; none will be replayed')
        native = journal.get('native_conversation_id')
        recovery_id = str(uuid.uuid4())
        authorization = dict(id=recovery_id, native_conversation_id=native, fingerprint=inspection['fingerprint'])
        journal.db.execute("UPDATE inbox SET state='acknowledged' WHERE state='delivering'")
        journal.db.execute("INSERT INTO metadata VALUES ('resume_authorization',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (encode(authorization),))
        journal.db.execute("UPDATE metadata SET value=? WHERE key='state'", (encode('launching'),))
        message = dict(id='recovery-'+recovery_id, text='Hive explicitly reconciled an interruption in this same conversation. Do not replay interrupted actions. Inspect current workspace and native history before continuing. Reconciliation reason: '+reason+'\nVerified side-effect evidence: '+evidence)
        journal.db.execute('INSERT INTO inbox(id,payload) VALUES (?,?)', (message['id'], encode(message)))
        audit = dict(id=recovery_id, native_conversation_id=native, fingerprint=inspection['fingerprint'], reason=reason, evidence=evidence, acknowledged_ids=acknowledgments)
        journal.db.execute('INSERT INTO events(id,kind,payload) VALUES (?,?,?)', (recovery_id, 'reconciliation', encode(audit)))
        journal.db.commit()
        pane = inspection['snapshot']['pane']
        assignment = journal.get('assignment')
    except Exception:
        journal.db.rollback()
        raise
    finally:
        lock.close()
    # The durable authorization is one-use. A lost SSH reply cannot authorize a
    # second launch, and an uncertain tmux failure is deliberately not retried.
    command = shlex.join([sys.executable, str(Path(__file__).resolve()), 'run', '--run-id', assignment['id']])
    subprocess.run([executable('tmux'), 'respawn-pane', '-k', '-t', pane['id'], command], check=True)
    return dict(resuming=True, recovery_id=recovery_id, native_conversation_id=native, tmux_name=pane['tmux_name'])


async def run(assignment, journal):
    # Holding a filesystem lock for the process lifetime prevents duplicate owners.
    import fcntl
    lock = open(journal.root/'owner.lock', 'w')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        return
    authorization = journal.get('resume_authorization')
    if authorization:
        if authorization.get('native_conversation_id') != journal.get('native_conversation_id') or journal.db.execute("SELECT 1 FROM inbox WHERE state='delivering'").fetchone() or journal.db.execute('SELECT 1 FROM approvals WHERE consumed=0').fetchone():
            journal.state('disconnected')
            journal.emit('reconcile-required', dict(reason='Resume identity or pending action changed'))
            lock.close()
            return
        journal.set('resume_authorization', None)
        journal.emit('resume-consumed', authorization)
    elif journal.get('started'):
        journal.state('disconnected')
        journal.emit('reconcile-required', dict(reason='Prior runner exited; inspect last tool before resuming'))
        lock.close()
        return
    journal.set('started', True)
    journal.set('assignment', assignment)
    journal.set('pid', os.getpid())
    journal.state('working')
    adapter = {'codex': Codex, 'claude': Claude, 'agy': Agy, 'opencode': OpenCode}[assignment['agent']]()
    try:
        await adapter.connect(assignment, journal)
        peers = assignment.get('peers', [])
        prompt = ('You are the real worker for this Hive assignment. Work only on your assigned device and workspace. '
                  'Implement, test, and verify the acceptance criteria. Keep services independent of this process. '
                  'Use the native hive peer tool when available, otherwise the peer command below, for questions, answers, interface agreements, and deployment results; '
                  'never impersonate peers or SSH into their machines. After a peer question, end this turn briefly if a reply is needed; Hive delivers it into this same session. Do not sleep or poll for peer replies. Include evidence and actual commands in your final response.\n'
                  + encode(assignment)+'\nPeer command: '+shlex.join([sys.executable, str(Path(__file__).resolve()), 'peer', '--run-id', assignment['id']])
                  +' --to PEER_RUN_ID --kind question|answer|agreement|deployment --body "message"\nPeers: '+encode(peers))
        if not journal.db.execute("SELECT 1 FROM inbox WHERE id='initial'").fetchone():
            journal.enqueue(dict(id='initial', text=prompt))
        recovery_message = 'recovery-'+authorization['id'] if authorization else None
        while True:
            row = journal.db.execute("SELECT * FROM inbox WHERE state='queued' ORDER BY CASE WHEN id=? THEN 0 ELSE 1 END, rowid LIMIT 1", (recovery_message,)).fetchone()
            if row is None:
                await asyncio.sleep(0.5)
                continue
            message = json.loads(row['payload'])
            # A crash after this checkpoint is uncertain, never replay automatically.
            with journal.db:
                journal.db.execute("UPDATE inbox SET state='delivering' WHERE id=?", (row['id'],))
            journal.state('working')
            await adapter.turn(message.get('text', encode(message)))
            with journal.db:
                journal.db.execute("UPDATE inbox SET state='acknowledged' WHERE id=?", (row['id'],))
            journal.emit('acknowledgment', dict(message_id=row['id']))
            journal.set('invocation', dict(verified_at=int(time.time()), model=journal.get('actual_model')))
            pending = journal.db.execute('SELECT 1 FROM approvals WHERE consumed=0 AND decision IS NULL').fetchone()
            journal.state('awaiting-approval' if pending else ('waiting-for-peer' if journal.get('state') == 'waiting-for-peer' else 'completed'))
    except Exception as error:
        journal.state('failed' if journal.get('native_conversation_id') else 'needs-setup')
        journal.emit('error', dict(message=str(error)))
    finally:
        if hasattr(adapter, 'proc'):
            await adapter.close()
        lock.close()


def mcp_peer(journal):
    """Narrow native MCP bridge for conversations created before dynamic tools."""
    journal.quiet = True
    for line in sys.stdin:
        request = json.loads(line)
        if 'id' not in request:
            continue
        method = request.get('method')
        if method == 'initialize':
            result = dict(protocolVersion=request.get('params', {}).get('protocolVersion', '2024-11-05'), capabilities={'tools': {}}, serverInfo={'name': 'hive-peer', 'version': '1.0.0'})
        elif method == 'tools/list':
            result = {'tools': [dict(name='peer', description='Send a durable task-scoped peer message. End the turn after a question to receive the reply.', inputSchema={'type':'object','required':['to','kind','text'],'additionalProperties':False,'properties':{'to':{'type':'string'},'kind':{'enum':['question','answer','agreement','deployment']},'text':{'type':'string'}}}),
                dict(name='service', description='Start or inspect this assignment\'s persistent private Python service in its own tmux session. Start argv: python3 WORKSPACE_SCRIPT.py [--secret-file WORKSPACE_TOKEN] serve --bind 127.0.0.1 --port UNPRIVILEGED_PORT --root WORKSPACE_DIRECTORY. Also accepts --token-file, --host, --directory. Existing service starts are idempotent; replacement and shutdown require separate review. Status requires only operation.', inputSchema={'type':'object','additionalProperties':False,'required':['operation'],'properties':{'operation':{'enum':['start','status']},'argv':{'type':'array','items':{'type':'string'},'minItems':3,'maxItems':25}}})]}
        elif method == 'tools/call':
            params = request.get('params', {})
            args = params.get('arguments', {})
            try:
                Codex.validate_hive_tool(journal, params.get('name'), args)
                if params['name'] == 'peer':
                    journal.emit('peer', args)
                    if args['kind'] == 'question': journal.state('waiting-for-peer')
                    output = 'Peer message durably queued'
                else:
                    output = encode(Codex.service(journal, args))
                result = {'content':[{'type':'text','text':output}], 'isError':False}
            except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
                result = {'content':[{'type':'text','text':str(error)}], 'isError':True}
        elif method == 'ping':
            result = {}
        else:
            print(encode({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32601,'message':'Unsupported method'}}), flush=True)
            continue
        print(encode({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument('operation', choices=['probe', 'assess', 'launch', 'run', 'snapshot', 'enqueue', 'decide', 'peer', 'hook', 'mcp', 'reconcile-inspect', 'reconcile'])
    parser.add_argument('--run-id')
    parser.add_argument('--after', type=int, default=0)
    parser.add_argument('--to')
    parser.add_argument('--kind', choices=['question', 'answer', 'agreement', 'deployment'])
    parser.add_argument('--body')
    args = parser.parse_args()
    if args.operation == 'assess':
        action = json.load(sys.stdin)
        print(encode({'reason': policy(action['tool'], action['arguments'], action['workspace'])}))
        return
    if args.operation == 'probe':
        print(encode(probe()))
        return
    ident = str(uuid.UUID(args.run_id))
    journal = Journal(BASE/ident)
    if args.operation == 'mcp':
        mcp_peer(journal)
        return
    journal.quiet = args.operation == 'hook'
    if args.operation == 'launch':
        assignment = json.load(sys.stdin)
        if assignment['id'] != ident or assignment['agent'] not in AGENTS:
            raise ValueError('Invalid assignment')
        workspace = Path(assignment['workspace']).expanduser()
        workspace.resolve().relative_to((Path.home()/'hive-workspaces').resolve())
        assignment['workspace'] = str(workspace.resolve())
        existing = journal.get('assignment')
        if existing:
            if existing != assignment:
                raise ValueError('Run ID reused with a different assignment')
            print(encode(dict(existing=True)))
            return
        if workspace.exists() and any(workspace.iterdir()):
            raise ValueError('Assignment requires a fresh empty workspace')
        workspace.mkdir(parents=True, exist_ok=True)
        journal.set('assignment', assignment)
        journal.state('launching')
        # Checkpoint first. SSH interruption never causes a second tmux launch.
        name = 'hive-agent-'+ident
        command = shlex.join([sys.executable, str(Path(__file__).resolve()), 'run', '--run-id', ident])
        subprocess.run([executable('tmux'), 'new-session', '-d', '-s', name, '-n', assignment['agent'], command], check=True)
        print(encode(dict(launched=True, tmux_name=name)))
    elif args.operation == 'run':
        asyncio.run(run(journal.get('assignment'), journal))
        # Retain a visible pane even if the native agent failed.
        while True:
            time.sleep(30)
    elif args.operation == 'snapshot':
        print(encode(journal.snapshot(args.after)))
    elif args.operation == 'reconcile-inspect':
        print(encode(recovery_snapshot(journal)))
    elif args.operation == 'reconcile':
        print(encode(reconcile(journal, json.load(sys.stdin))))
    elif args.operation == 'enqueue':
        journal.enqueue(json.load(sys.stdin))
    elif args.operation == 'decide':
        request = json.load(sys.stdin)
        journal.decide(request['id'], request['fingerprint'], request['decision'])
        if journal.get('assignment', {}).get('agent') == 'agy':
            row = journal.db.execute('SELECT action FROM approvals WHERE id=?', (request['id'],)).fetchone()
            journal.enqueue(dict(id='approval-'+request['id'], text='The user chose '+request['decision']+' for this exact action: '+row[0]+'. Continue the same task; the hook has a single-use grant only if approved.'))
    elif args.operation == 'peer':
        if not args.to or not args.kind or not args.body or len(args.body) > 16000:
            raise ValueError('Peer, kind and 1–16000 byte body required')
        assignment = journal.get('assignment')
        if args.to not in [p['id'] for p in assignment.get('peers', [])]:
            raise ValueError('Peer is not in this task')
        journal.emit('peer', dict(to=args.to, kind=args.kind, text=args.body))
        if args.kind == 'question':
            journal.state('waiting-for-peer')
    elif args.operation == 'hook':
        request = json.load(sys.stdin)
        call = request.get('toolCall', {})
        assignment = journal.get('assignment')
        action = dict(tool=call.get('name'), arguments=call.get('args', {}), workspace=assignment['workspace'])
        reason = policy(action['tool'], action['arguments'], action['workspace'])
        if not reason:
            print(encode(dict(decision='allow')))
            return
        # AGY cannot wait for stream permission messages. Deny before execution;
        # a matching one-use grant may allow exactly one later invocation.
        ident = journal.pending(action, reason)
        decision = journal.consume(ident)
        print(encode(dict(decision='allow' if decision == 'continue' else 'deny', reason=reason)))


if __name__ == '__main__':
    main()
