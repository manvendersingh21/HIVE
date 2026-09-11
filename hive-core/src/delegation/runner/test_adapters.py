"""Native interface contracts; no providers or model processes are started.

Fixtures follow AGY headless/hooks documentation and OpenCode 1.18.29 /doc.
"""
import asyncio
import contextlib
import io
import json
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import AsyncMock, MagicMock, patch
import uuid

import runner


class AdapterContracts(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.workspace = self.root / 'workspace'
        self.workspace.mkdir()
        self.ident = str(uuid.uuid4())
        self.j = runner.Journal(self.root / self.ident)
        self.j.quiet = True
        self.assignment = dict(id=self.ident, agent='agy', workspace=str(self.workspace), objective='test')
        self.j.set('assignment', self.assignment)

    def tearDown(self):
        self.j.db.close()
        self.temp.cleanup()

    def agy(self):
        adapter = runner.Agy()
        adapter.j = self.j
        adapter.notifications = asyncio.Queue()
        adapter.send = AsyncMock()
        return adapter

    async def test_agy_initial_identity_survives_disconnection(self):
        adapter = self.agy()
        adapter.notifications.put_nowait(dict(event='init', conversation_id='native-id', init={'model': 'available-model'}))
        adapter.notifications.put_nowait(dict(disconnected='connection lost'))
        with self.assertRaisesRegex(RuntimeError, 'connection lost'):
            await adapter.turn('start')
        self.assertEqual(self.j.get('native_conversation_id'), 'native-id')
        self.assertEqual(self.j.get('actual_model'), 'available-model')

    async def test_agy_only_success_completes_a_turn(self):
        for status in ('ERROR', 'CANCELED', 'INTERRUPTED', 'INVALID', 'WAITING', 'RUNNING', None):
            with self.subTest(status=status):
                adapter = self.agy()
                adapter.notifications.put_nowait(dict(event='result', result=dict(conversation_id='native-id', status=status)))
                with self.assertRaises(RuntimeError):
                    await adapter.turn('start')

    async def test_agy_multiple_turns_share_stream_and_only_send_user_messages(self):
        adapter = self.agy()
        for prompt in ('first', 'follow-up'):
            adapter.notifications.put_nowait(dict(event='result', result=dict(conversation_id='native-id', status='SUCCESS')))
            await adapter.turn(prompt)
        self.assertEqual([call.args[0] for call in adapter.send.call_args_list], [
            dict(event='user', message=dict(content='first')),
            dict(event='user', message=dict(content='follow-up')),
        ])
        self.assertEqual(self.j.get('native_conversation_id'), 'native-id')

    async def test_agy_reconnect_preserves_own_hooks_and_rejects_changed_hooks(self):
        adapter = self.agy()
        adapter.start = AsyncMock()
        with patch.object(runner, 'executable', return_value='/bin/agy'):
            await adapter.connect(self.assignment, self.j)
            hook_file = self.workspace / '.agents/hooks.json'
            original = hook_file.read_bytes()
            self.j.set('native_conversation_id', 'native-id')
            await adapter.connect(self.assignment, self.j)
            self.assertEqual(hook_file.read_bytes(), original)
            args = adapter.start.call_args.args[0]
            self.assertEqual(args[-2:], ['--conversation', 'native-id'])
            self.assertNotIn('--dangerously-skip-permissions', args)
            hook_file.write_text('{}')
            with self.assertRaisesRegex(RuntimeError, 'hooks require review'):
                await adapter.connect(self.assignment, self.j)
            self.assertEqual(adapter.start.call_count, 2)

    async def test_agy_hook_denies_then_consumes_only_exact_one_use_grant(self):
        request = dict(toolCall=dict(name='run_command', args=dict(CommandLine='sudo true', Cwd=str(self.workspace))),
                       conversationId='native-id', modelName='model')
        def hook(payload):
            output = io.StringIO()
            with patch.object(runner, 'BASE', self.root), patch.object(runner.sys, 'argv', ['runner.py', 'hook', '--run-id', self.ident]), \
                    patch.object(runner.sys, 'stdin', io.StringIO(json.dumps(payload))), contextlib.redirect_stdout(output):
                runner.main()
            return json.loads(output.getvalue())
        self.assertEqual(hook(request)['decision'], 'deny')
        pending = self.j.snapshot()['approvals'][0]
        self.j.decide(pending['id'], pending['fingerprint'], 'continue')
        changed = dict(toolCall=dict(name='run_command', args=dict(CommandLine='sudo other', Cwd=str(self.workspace))))
        self.assertEqual(hook(changed)['decision'], 'deny')
        self.assertEqual(hook(request)['decision'], 'allow')
        self.assertEqual(hook(request)['decision'], 'deny')

    def opencode(self):
        adapter = runner.OpenCode()
        adapter.j, adapter.a = self.j, self.assignment
        adapter.native = 'ses_native'
        adapter.model = dict(providerID='provider', modelID='requested')
        return adapter

    async def test_opencode_connect_requires_connected_model_and_private_ask_config(self):
        adapter = self.opencode()
        paths = ('/permission/{requestID}/reply', '/session/{sessionID}/prompt_async',
                 '/session/status', '/session/{sessionID}/message/{messageID}')
        socket = MagicMock()
        socket.__enter__.return_value.getsockname.return_value = ('127.0.0.1', 12345)
        self.j.set('native_conversation_id', 'ses_existing')
        async def http(method, path, body=None):
            if path == '/doc':
                return dict(paths=dict.fromkeys(paths, {}))
            if path == '/provider':
                return dict(all=[dict(id='offline', models={'unavailable': {}}), dict(id='online', models={'usable': {}})],
                            connected=['online'], default={'offline': 'unavailable', 'online': 'usable'})
            self.fail('Reconnect must not create a new session: '+path)
        adapter.http = AsyncMock(side_effect=http)
        with patch('socket.socket', return_value=socket), patch.object(runner, 'executable', return_value='/bin/opencode'), \
                patch.object(runner.asyncio, 'create_subprocess_exec', new=AsyncMock()) as launch:
            await adapter.connect(self.assignment, self.j)
        self.assertEqual(adapter.native, 'ses_existing')
        self.assertEqual(adapter.model, dict(providerID='online', modelID='usable'))
        self.assertEqual(self.j.get('available_models'), ['online/usable'])
        self.assertEqual(launch.call_args.args[-4:], ('--hostname', '127.0.0.1', '--port', '12345'))
        env = launch.call_args.kwargs['env']
        config = json.loads(env['OPENCODE_CONFIG_CONTENT'])
        self.assertEqual(config['permission'], {'*': 'ask'})
        self.assertEqual(config['agent']['build']['permission'], {'*': 'ask'})
        self.assertTrue(env['OPENCODE_SERVER_PASSWORD'])

    async def test_opencode_unknown_requested_model_fails_before_session_creation(self):
        adapter = self.opencode()
        paths = ('/permission/{requestID}/reply', '/session/{sessionID}/prompt_async',
                 '/session/status', '/session/{sessionID}/message/{messageID}')
        socket = MagicMock()
        socket.__enter__.return_value.getsockname.return_value = ('127.0.0.1', 12345)
        adapter.http = AsyncMock(side_effect=[dict(paths=dict.fromkeys(paths, {})),
            dict(all=[dict(id='online', models={'usable': {}})], connected=['online'], default={})])
        with patch('socket.socket', return_value=socket), patch.object(runner, 'executable', return_value='/bin/opencode'), \
                patch.object(runner.asyncio, 'create_subprocess_exec', new=AsyncMock()):
            with self.assertRaisesRegex(RuntimeError, 'Unavailable OpenCode model'):
                await adapter.connect(dict(self.assignment, model='online/unavailable'), self.j)
        self.assertEqual(adapter.http.call_count, 2)

    async def test_opencode_does_not_finish_on_initial_idle_or_unrelated_result(self):
        adapter = self.opencode()
        message_id, polls = None, 0
        async def http(method, path, body=None):
            nonlocal message_id, polls
            if path.endswith('/prompt_async'):
                message_id = body['messageID']
                self.assertRegex(message_id, r'^msg_[0-9a-f]{12}[0-9A-Za-z]{14}$')
                self.assertEqual(self.j.get('opencode_pending_message'), message_id)
                return None
            if path == '/permission':
                return []
            if '?limit=' in path:
                polls += 1
                return [dict(info=dict(role='assistant', parentID='msg_other' if polls == 1 else message_id,
                    modelID='actual', providerID='provider', time={'completed': 1}, finish='stop'), parts=[])]
            if path == '/session/status':
                return {}
            self.fail(path)
        adapter.http = AsyncMock(side_effect=http)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            await adapter.turn('implement')
        self.assertEqual(polls, 2)
        self.assertIsNone(self.j.get('opencode_pending_message'))
        self.assertEqual(self.j.get('actual_model'), 'provider/actual')
        self.assertEqual(sum(call.args[0] == 'POST' for call in adapter.http.call_args_list), 1)

    async def test_opencode_lost_submission_response_is_never_resubmitted(self):
        adapter = self.opencode()
        adapter.http = AsyncMock(side_effect=TimeoutError('lost response'))
        with self.assertRaises(TimeoutError):
            await adapter.turn('implement')
        self.assertTrue(self.j.get('opencode_pending_message'))
        with self.assertRaisesRegex(RuntimeError, 'uncertain'):
            await adapter.turn('implement')
        self.assertEqual(adapter.http.call_count, 1)

    async def test_opencode_tool_round_and_busy_status_cannot_complete_turn(self):
        adapter = self.opencode()
        message_id, polls = None, 0
        async def http(method, path, body=None):
            nonlocal message_id, polls
            if path.endswith('/prompt_async'):
                message_id = body['messageID']
            elif path == '/permission':
                return []
            elif '?limit=' in path:
                polls += 1
                return [dict(info=dict(role='assistant', parentID=message_id, time={'completed': 1},
                    finish='tool-calls' if polls == 1 else 'stop'))]
            elif path == '/session/status':
                return {adapter.native: dict(type='busy' if polls == 2 else 'idle')}
            else:
                self.fail(path)
        adapter.http = AsyncMock(side_effect=http)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            await adapter.turn('implement')
        self.assertEqual(polls, 3)

    async def test_opencode_approval_uses_exact_input_and_once_or_reject(self):
        for allowed in (True, False):
            with self.subTest(allowed=allowed):
                adapter = self.opencode()
                self.j.set('opencode_pending_message', None)
                replies = []
                message_id = None
                request = dict(id='per_1', sessionID=adapter.native, permission='bash', patterns=['sudo *'],
                    metadata={}, always=['*'], tool=dict(messageID='msg_tool', callID='call_1'))
                part = dict(type='tool', tool='bash', callID='call_1', messageID='msg_tool', sessionID=adapter.native,
                    state=dict(status='running', input=dict(command='sudo exact-command')))
                async def http(method, path, body=None):
                    nonlocal message_id
                    if path.endswith('/prompt_async'):
                        message_id = body['messageID']
                    elif path == '/permission':
                        return [dict(request, id='per_other', sessionID='ses_other'), request]
                    elif path.endswith('/message/msg_tool'):
                        return dict(parts=[part])
                    elif path == '/permission/per_1/reply':
                        replies.append(body)
                    elif '?limit=' in path:
                        return [dict(info=dict(role='assistant', parentID=message_id, time={'completed': 1}, finish='stop'))]
                    elif path == '/session/status':
                        return {}
                    else:
                        self.fail(path)
                adapter.http = AsyncMock(side_effect=http)
                with patch.object(runner, 'permission', new=AsyncMock(return_value=allowed)) as permission:
                    await adapter.turn('implement')
                self.assertEqual(replies, [dict(reply='once' if allowed else 'reject')])
                self.assertEqual(permission.call_args.args[1], 'Bash')
                self.assertEqual(permission.call_args.args[2]['command'], 'sudo exact-command')
                self.assertEqual(permission.call_args.args[2]['_native_permission'], request)

    async def test_opencode_opaque_permission_is_rejected_without_user_grant(self):
        adapter = self.opencode()
        request = dict(id='per_opaque', sessionID=adapter.native, permission='bash')
        async def http(method, path, body=None):
            return [request] if path == '/permission' else None
        adapter.http = AsyncMock(side_effect=http)
        with patch.object(runner, 'permission', new=AsyncMock()) as permission:
            with self.assertRaisesRegex(RuntimeError, 'exact tool reference'):
                await adapter.turn('implement')
        permission.assert_not_called()
        self.assertEqual(adapter.http.call_args.args, ('POST', '/permission/per_opaque/reply', {'reply': 'reject'}))

    def elicitation(self, tool, args):
        return dict(serverName='hive', threadId='native-thread', mode='form',
                    message='Allow the hive MCP server to run tool "'+tool+'"?',
                    requestedSchema={'type': 'object', 'properties': {}},
                    _meta={'codex_approval_kind': 'mcp_tool_call', 'tool_params': args})

    def service_args(self):
        (self.workspace/'server.py').write_text('# agent-authored application fixture\n')
        (self.workspace/'secret.token').write_text('private-test-token')
        return dict(operation='start', argv=[sys.executable, 'server.py', '--secret-file', 'secret.token',
                    'serve', '--bind', '127.0.0.1', '--port', '8765', '--root', 'incoming'])

    async def test_codex_mcp_approval_accepts_only_exact_hive_peer_scope(self):
        self.j.set('assignment', dict(self.assignment, peers=[{'id': 'peer-id'}]))
        request = self.elicitation('peer', dict(to='peer-id', kind='agreement', text='protocol v1'))
        self.assertTrue(runner.Codex.hive_elicitation(self.j, request, 'native-thread'))
        for changed in (dict(serverName='other'), dict(threadId='other'), dict(mode='url'),
                        dict(message='Allow arbitrary tool peer?'), dict(requestedSchema={'type': 'object', 'properties': {'grant': {}}}),
                        dict(_meta={'codex_approval_kind': 'mcp_tool_call', 'tool_params': dict(to='other-task', kind='agreement', text='x')})):
            with self.subTest(changed=changed):
                self.assertFalse(runner.Codex.hive_elicitation(self.j, dict(request, **changed), 'native-thread'))
        self.assertEqual(self.j.db.execute("SELECT count(*) FROM events WHERE kind='peer'").fetchone()[0], 0,
                         'Approval must not execute the tool; MCP invokes it once afterward')
        adapter = runner.Codex()
        adapter.j, adapter.a, adapter.native, adapter.items = self.j, self.assignment, 'native-thread', {}
        adapter.rpc, adapter.send, adapter.notifications = AsyncMock(), AsyncMock(), asyncio.Queue()
        adapter.notifications.put_nowait(dict(id='approval-id', method='mcpServer/elicitation/request', params=request))
        adapter.notifications.put_nowait(dict(method='turn/completed', params={'turn': {'status': 'completed'}}))
        await adapter.turn('peer work')
        adapter.send.assert_awaited_once_with(dict(id='approval-id', result=dict(action='accept', content={})))

    async def test_codex_start_and_resume_keep_untrusted_and_same_mcp(self):
        for native in (None, 'native-existing'):
            with self.subTest(native=native):
                self.j.set('native_conversation_id', native)
                adapter = runner.Codex()
                adapter.start, adapter.send = AsyncMock(), AsyncMock()
                adapter.rpc = AsyncMock(side_effect=[{}, {'data': [{'model': 'available'}]},
                                                     {'thread': {'id': native or 'native-new'}, 'model': 'available'}])
                await adapter.connect(self.assignment, self.j)
                params = adapter.rpc.call_args.args[1]
                self.assertEqual(params['approvalPolicy'], 'untrusted')
                self.assertEqual(params['sandbox'], 'workspace-write')
                self.assertNotIn('dynamicTools', params)
                self.assertEqual(params['config']['mcp_servers.hive']['args'][-2:], ['--run-id', self.ident])

    async def test_service_validation_rejects_escape_interpreters_and_public_bind(self):
        args = self.service_args()
        argv = args['argv']
        self.assertTrue(runner.Codex.hive_elicitation(self.j, self.elicitation('service', args), 'native-thread'))
        (self.workspace/'escape.py').symlink_to(self.root/'outside.py')
        (self.root/'outside.py').write_text('outside')
        variants = []
        for index, value in ((0, '/bin/sh'), (1, '-c'), (1, 'escape.py'), (3, '/etc/passwd'),
                             (6, '0.0.0.0'), (8, '22'), (8, '65536'), (10, '../outside')):
            changed = list(argv)
            changed[index] = value
            variants.append(changed)
        variants += [argv+['--bind', '127.0.0.1'], argv+['--eval', 'malicious'], argv+[';touch /tmp/escape']]
        for changed in variants:
            with self.subTest(argv=changed):
                with self.assertRaises(ValueError):
                    runner.Codex.validate_hive_tool(self.j, 'service', dict(operation='start', argv=changed))
        with patch.object(runner.subprocess, 'run') as launch:
            with self.assertRaises(ValueError):
                runner.Codex.service(self.j, dict(operation='start', argv=variants[0]))
            launch.assert_not_called()

    async def test_service_start_is_idempotent_and_owns_exact_pane(self):
        args = self.service_args()
        with patch.object(runner, 'capture', return_value=(1, '')), \
                patch.object(runner.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '%3\t12345\n', '')) as launch:
            result = runner.Codex.service(self.j, args)
        self.assertEqual(result['state'], 'started')
        receipt = result['receipt']
        self.assertEqual(receipt['tmux_name'], 'hive-service-'+self.ident)
        self.assertEqual(launch.call_args.args[0][-len(receipt['argv']):], receipt['argv'])
        self.assertNotIn('shell', launch.call_args.kwargs)
        import shlex
        pane = '%3\t12345\t'+str(self.workspace)+'\t'+shlex.join(receipt['argv'])+'\t0'
        with patch.object(runner, 'capture', return_value=(0, pane)), patch.object(runner.subprocess, 'run') as launch:
            self.assertEqual(runner.Codex.service(self.j, args)['state'], 'running')
            self.assertEqual(runner.Codex.service(self.j, {'operation': 'status'})['state'], 'running')
            launch.assert_not_called()
        changed = dict(args, argv=list(args['argv']))
        changed['argv'][8] = '8766'
        with self.assertRaisesRegex(ValueError, 'command changed'):
            runner.Codex.service(self.j, changed)
        with patch.object(runner, 'capture', return_value=(0, pane.replace('12345', '99999'))):
            with self.assertRaisesRegex(ValueError, 'ownership'):
                runner.Codex.service(self.j, {'operation': 'status'})

    async def test_service_lost_launch_response_is_not_replayed(self):
        args = self.service_args()
        with patch.object(runner, 'capture', return_value=(1, '')), \
                patch.object(runner.subprocess, 'run', side_effect=subprocess.TimeoutExpired('tmux', 15)) as launch:
            with self.assertRaises(subprocess.TimeoutExpired):
                runner.Codex.service(self.j, args)
            self.assertEqual(self.j.get('service_receipt')['state'], 'launching')
            self.assertEqual(runner.Codex.service(self.j, args)['state'], 'disconnected')
            self.assertEqual(launch.call_count, 1)

    async def test_service_cannot_adopt_preexisting_unrelated_session(self):
        args = self.service_args()
        with patch.object(runner, 'capture', return_value=(0, '')), patch.object(runner.subprocess, 'run') as launch:
            with self.assertRaisesRegex(ValueError, 'without an ownership receipt'):
                runner.Codex.service(self.j, args)
            launch.assert_not_called()


if __name__ == '__main__':
    unittest.main()
