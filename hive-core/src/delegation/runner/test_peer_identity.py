"""Peer-command identity binding.

Regression tests for the audit of main at 852f946: the runner auto-approved
`runner.py peer --run-id <id>` without checking that <id> was the calling run,
so any co-located run could forge peer, question and agreement messages into
another run's journal. The audit's reproduction (peer_spoof_repro.py) is
`test_peer_spoof_repro_forged_agreement_is_refused_and_writes_nothing`.
"""
import asyncio
import contextlib
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import runner

RUNNER = str(Path(runner.__file__).resolve())
VICTIM = '11111111-1111-4111-8111-111111111111'
ATTACKER = '22222222-2222-4222-8222-222222222222'
VICTIM_PEER = '33333333-3333-4333-8333-333333333333'


def dump(root):
    """Every row of every table in a journal, for write-nothing comparisons."""
    import sqlite3
    db = sqlite3.connect(str(Path(root)/'journal.db'))
    try:
        tables = [r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
        return {t: db.execute('SELECT * FROM "'+t+'" ORDER BY rowid').fetchall() for t in tables}
    finally:
        db.close()


class PeerIdentityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        self.base = self.home/'.hive'/'runs'
        self.workspace = self.home/'hive-workspaces'/'w'
        self.workspace.mkdir(parents=True)
        self.journals = []

    def tearDown(self):
        for journal in self.journals:
            journal.db.close()
        self.temp.cleanup()

    def journal(self, ident, peers):
        journal = runner.Journal(self.base/ident)
        journal.quiet = True
        journal.set('assignment', dict(id=ident, agent='codex', workspace=str(self.workspace),
                                       peers=[dict(id=p) for p in peers]))
        journal.set('state', 'working')
        self.journals.append(journal)
        return journal

    def run_peer(self, run_id, to, kind, body, credential):
        """Run the exact peer command an agent is told to use, as a subprocess."""
        env = {k: v for k, v in os.environ.items() if not k.startswith('HIVE_')}
        env['HOME'] = str(self.home)
        if credential is not None:
            env[runner.RUN_CREDENTIAL_ENV] = credential
        return subprocess.run([sys.executable, RUNNER, 'peer', '--run-id', run_id, '--to', to, '--kind', kind, '--body', body],
                              env=env, capture_output=True, text=True, timeout=30)

    def peer_events(self, journal):
        return [json.loads(r[0]) for r in journal.db.execute("SELECT payload FROM events WHERE kind='peer' ORDER BY seq")]

    def test_peer_spoof_repro_forged_agreement_is_refused_and_writes_nothing(self):
        # Two co-located runs. The attacker's agent holds only its own
        # credential and runs the peer command from its prompt with the
        # victim's run ID, forging a real agreement to the victim's peer.
        victim = self.journal(VICTIM, [VICTIM_PEER])
        victim.issue_credential()
        attacker = self.journal(ATTACKER, [])
        attacker_credential = attacker.issue_credential()
        before = dump(self.base/VICTIM)
        forged = self.run_peer(VICTIM, VICTIM_PEER, 'agreement', 'AGREED: ship the attacker interface', attacker_credential)
        self.assertNotEqual(forged.returncode, 0)
        self.assertIn('Peer command refused', forged.stderr)
        self.assertEqual(dump(self.base/VICTIM), before)
        self.assertEqual(self.peer_events(victim), [])
        # The attacker's own journal is untouched as well.
        self.assertEqual(self.peer_events(attacker), [])
        # Without any credential, or with a guessed one, the result is the same.
        for credential in (None, '', 'guess', runner.hashlib.sha256(b'x').hexdigest()):
            result = self.run_peer(VICTIM, VICTIM_PEER, 'question', 'forged', credential)
            self.assertNotEqual(result.returncode, 0)
        self.assertEqual(dump(self.base/VICTIM), before)

    def test_forged_peer_command_for_unknown_run_creates_no_journal(self):
        attacker = self.journal(ATTACKER, [])
        credential = attacker.issue_credential()
        missing = '44444444-4444-4444-8444-444444444444'
        result = self.run_peer(missing, VICTIM_PEER, 'agreement', 'forged', credential)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.base/missing).exists())

    def test_journal_from_before_credentials_refuses_peer_commands(self):
        # A run whose runner never issued a credential cannot be spoken for.
        legacy = self.journal(VICTIM, [VICTIM_PEER])
        legacy.db.execute('DROP TABLE credentials')
        legacy.db.commit()
        before = dump(self.base/VICTIM)
        result = self.run_peer(VICTIM, VICTIM_PEER, 'agreement', 'forged', 'anything')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(dump(self.base/VICTIM), before)

    def test_legitimate_peer_command_still_works(self):
        victim = self.journal(VICTIM, [VICTIM_PEER])
        credential = victim.issue_credential()
        result = self.run_peer(VICTIM, VICTIM_PEER, 'question', 'which port?', credential)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.peer_events(victim), [dict(to=VICTIM_PEER, kind='question', text='which port?')])
        self.assertEqual(victim.get('state'), 'waiting-for-peer')
        # The run's own credential still cannot leave the task's peer list.
        outside = self.run_peer(VICTIM, ATTACKER, 'answer', 'hello', credential)
        self.assertNotEqual(outside.returncode, 0)
        self.assertEqual(len(self.peer_events(victim)), 1)
        self.assertNotIn(credential, result.stdout+result.stderr+outside.stdout+outside.stderr)

    def test_reissued_credential_revokes_the_previous_one(self):
        victim = self.journal(VICTIM, [VICTIM_PEER])
        old = victim.issue_credential()
        new = victim.issue_credential()
        self.assertNotEqual(old, new)
        self.assertNotEqual(self.run_peer(VICTIM, VICTIM_PEER, 'answer', 'a', old).returncode, 0)
        self.assertEqual(self.run_peer(VICTIM, VICTIM_PEER, 'answer', 'b', new).returncode, 0)
        self.assertEqual([e['text'] for e in self.peer_events(victim)], ['b'])

    def peer_command(self, run_id):
        return shlex.join([sys.executable, RUNNER, 'peer', '--run-id', run_id, '--to', VICTIM_PEER,
                           '--kind', 'agreement', '--body', 'agreed'])

    def test_auto_approval_only_accepts_the_calling_runs_peer_command(self):
        ws = str(self.workspace)
        own, forged = self.peer_command(ATTACKER), self.peer_command(VICTIM)
        self.assertIsNone(runner.policy('Bash', dict(command=own), ws, ATTACKER))
        self.assertIsNotNone(runner.policy('Bash', dict(command=forged), ws, ATTACKER))
        # Unknown caller, wrappers, compounds and argument smuggling never pass.
        self.assertIsNotNone(runner.policy('Bash', dict(command=own), ws))
        self.assertIsNotNone(runner.policy('Bash', dict(command='bash -c '+shlex.quote(forged)), ws, ATTACKER))
        self.assertIsNotNone(runner.policy('Bash', dict(command='true && '+forged), ws, ATTACKER))
        self.assertIsNotNone(runner.policy('Bash', dict(command='timeout 5 '+forged), ws, ATTACKER))
        self.assertIsNotNone(runner.policy('Bash', dict(command=own+' --run-id '+VICTIM), ws, ATTACKER))
        self.assertIsNotNone(runner.policy('Bash', dict(command=shlex.join([sys.executable, RUNNER, 'peer', '--run-id', ATTACKER,
            '--to', VICTIM_PEER, '--run-id', VICTIM, '--body', 'x'])), ws, ATTACKER))
        self.assertIsNone(runner.policy('Bash', dict(command='bash -c '+shlex.quote(own)), ws, ATTACKER))

    def test_runner_permission_holds_a_forged_peer_command_for_review(self):
        attacker = self.journal(ATTACKER, [])
        async def exercise():
            self.assertTrue(await runner.permission(attacker, 'Bash', dict(command=self.peer_command(ATTACKER)), str(self.workspace)))
            pending = asyncio.create_task(runner.permission(attacker, 'Bash', dict(command=self.peer_command(VICTIM)), str(self.workspace)))
            await asyncio.sleep(.05)
            self.assertFalse(pending.done())
            approval = attacker.snapshot()['approvals'][0]
            self.assertEqual(approval['reason'], 'Peer command does not speak for the calling run')
            pending.cancel()
        asyncio.run(exercise())

    def test_agy_hook_denies_a_forged_peer_command(self):
        attacker = self.journal(ATTACKER, [])
        attacker.set('assignment', dict(attacker.get('assignment'), agent='agy'))
        def hook(command):
            request = json.dumps(dict(toolCall=dict(name='run_command', args=dict(CommandLine=command))))
            out = io.StringIO()
            with patch.object(runner, 'BASE', self.base), patch.object(sys, 'argv', ['runner.py', 'hook', '--run-id', ATTACKER]), \
                    patch.object(sys, 'stdin', io.StringIO(request)), contextlib.redirect_stdout(out):
                runner.main()
            return json.loads(out.getvalue().strip().splitlines()[-1])
        self.assertEqual(hook(self.peer_command(ATTACKER))['decision'], 'allow')
        self.assertEqual(hook(self.peer_command(VICTIM))['decision'], 'deny')

    def test_web_assessment_binds_to_the_requesting_run(self):
        def assess(run_id, command):
            action = json.dumps(dict(tool='Bash', arguments=dict(command=command), workspace=str(self.workspace)))
            argv = ['runner.py', 'assess'] + (['--run-id', run_id] if run_id else [])
            out = io.StringIO()
            with patch.object(sys, 'argv', argv), patch.object(sys, 'stdin', io.StringIO(action)), contextlib.redirect_stdout(out):
                runner.main()
            return json.loads(out.getvalue())['reason']
        self.assertIsNone(assess(ATTACKER, self.peer_command(ATTACKER)))
        self.assertIsNotNone(assess(ATTACKER, self.peer_command(VICTIM)))
        self.assertIsNotNone(assess(None, self.peer_command(ATTACKER)))

    def test_credential_reaches_only_the_agent_environment(self):
        journal = runner.Journal(self.base/VICTIM)
        journal.quiet = True
        self.journals.append(journal)
        seen = {}
        class Adapter:
            async def connect(inner, assignment, j):
                seen['env'] = runner.agent_environment(inner)
                seen['credential'] = inner.credential
            async def turn(inner, prompt):
                seen['prompt'] = prompt
                # An agent that prints its environment must not leak it.
                j = journal
                j.emit('native', dict(type='tool_result', output='HIVE_RUN_CREDENTIAL='+inner.credential))
                j.set('actual_model', 'model '+inner.credential)
                raise RuntimeError('stop after first turn: '+inner.credential)
        assignment = dict(id=VICTIM, agent='codex', workspace=str(self.workspace), peers=[dict(id=VICTIM_PEER)])
        out = io.StringIO()
        journal.quiet = False
        with patch.object(runner, 'Codex', Adapter), contextlib.redirect_stdout(out):
            asyncio.run(runner.run(assignment, journal))
        credential = seen['credential']
        self.assertGreaterEqual(len(credential), 40)
        self.assertEqual(seen['env'][runner.RUN_CREDENTIAL_ENV], credential)
        self.assertNotIn(runner.RUN_CREDENTIAL_ENV, os.environ)
        self.assertNotIn(credential, seen['prompt'])
        self.assertIn('peer --run-id '+VICTIM, seen['prompt'])
        # Not in any journal table (only its hash), events, snapshot or stdout.
        everything = json.dumps(dump(self.base/VICTIM), default=str)
        self.assertNotIn(credential, everything)
        self.assertIn(runner.hashlib.sha256(credential.encode()).hexdigest(), everything)
        snapshot = runner.encode(journal.snapshot())
        self.assertNotIn(credential, snapshot)
        self.assertNotIn(runner.hashlib.sha256(credential.encode()).hexdigest(), snapshot)
        self.assertNotIn(credential, out.getvalue())
        self.assertIn('[redacted]', everything)
        recovery = runner.recovery_snapshot(journal, owner_lock=object())
        self.assertNotIn(credential, runner.encode(recovery))
        # The issued credential is the one peer commands must present.
        self.assertTrue(runner.peer_authorized(self.base/VICTIM, credential))
        self.assertFalse(runner.peer_authorized(self.base/VICTIM, credential[:-1]))

    def test_native_processes_receive_the_credential_but_probes_do_not(self):
        script = 'import json,os,sys; print(json.dumps(dict(v=os.environ.get(%r))), flush=True); sys.stdin.readline()' % runner.RUN_CREDENTIAL_ENV
        async def observe(credential):
            process = runner.JsonProcess()
            if credential:
                process.credential = credential
            await process.start([sys.executable, '-c', script], str(self.workspace), quiet=True)
            try:
                return (await asyncio.wait_for(process.notifications.get(), 20))['v']
            finally:
                await process.close()
        self.assertEqual(asyncio.run(observe('per-run-value')), 'per-run-value')
        self.assertIsNone(asyncio.run(observe(None)))
        self.assertIsNone(runner.agent_environment(object()))
        opencode = type('OpenCode', (), {'credential': 'c'})()
        self.assertEqual(runner.agent_environment(opencode, dict(A='1')), dict(A='1', HIVE_RUN_CREDENTIAL='c'))

    def test_native_hive_peer_tool_path_still_works(self):
        journal = self.journal(VICTIM, [VICTIM_PEER])
        journal.issue_credential()
        requests = [dict(jsonrpc='2.0', id=1, method='tools/call',
                         params=dict(name='peer', arguments=dict(to=VICTIM_PEER, kind='agreement', text='port 8080')))]
        out = io.StringIO()
        with patch.object(sys, 'stdin', io.StringIO('\n'.join(map(json.dumps, requests))+'\n')), contextlib.redirect_stdout(out):
            runner.mcp_peer(journal)
        self.assertFalse(json.loads(out.getvalue())['result']['isError'])
        self.assertEqual(self.peer_events(journal), [dict(to=VICTIM_PEER, kind='agreement', text='port 8080')])
        # The Claude SDK bridge's native peer event is still journaled.
        claude = runner.Claude()
        claude.j, claude.a = journal, journal.get('assignment')
        async def turn():
            claude.notifications = asyncio.Queue()
            async def send(value):
                pass
            claude.send = send
            await claude.notifications.put(dict(type='peer', to=VICTIM_PEER, kind='answer', text='ok'))
            await claude.notifications.put(dict(type='result', is_error=False))
            await claude.turn('prompt')
        asyncio.run(turn())
        self.assertEqual(self.peer_events(journal)[-1], dict(to=VICTIM_PEER, kind='answer', text='ok'))
        self.assertIsNone(runner.policy('mcp__hive__peer', dict(to=VICTIM_PEER, kind='answer', text='ok'), str(self.workspace), VICTIM))


if __name__ == '__main__':
    unittest.main()
