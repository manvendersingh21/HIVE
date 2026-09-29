"""Native interface contracts; no providers or model processes are started.

Fixtures follow AGY headless/hooks documentation, OpenCode 1.18.29 /doc and
Cursor Agent 2026.09.26 stream-json events.
"""
import asyncio
import contextlib
import io
import json
import shlex
import subprocess
import sys
from pathlib import Path
import tempfile
import time
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

    async def test_agy_yolo_adds_dangerously_skip_permissions(self):
        adapter = self.agy()
        adapter.start = AsyncMock()
        yolo_assignment = dict(self.assignment, autonomy='yolo')
        with patch.object(runner, 'executable', return_value='/bin/agy'):
            await adapter.connect(yolo_assignment, self.j)
        args = adapter.start.call_args.args[0]
        self.assertIn('--dangerously-skip-permissions', args)

    async def test_agy_non_yolo_omits_dangerously_skip_permissions(self):
        adapter = self.agy()
        adapter.start = AsyncMock()
        with patch.object(runner, 'executable', return_value='/bin/agy'):
            await adapter.connect(self.assignment, self.j)
        args = adapter.start.call_args.args[0]
        self.assertNotIn('--dangerously-skip-permissions', args)

    async def test_agy_denied_actions_with_empty_response_fails_turn(self):
        adapter = self.agy()
        adapter.notifications.put_nowait(dict(event='result', result=dict(
            conversation_id='native-id', status='SUCCESS',
            denied_actions=[dict(tool='bash', command='rm -rf /')],
            response='',
        )))
        with self.assertRaisesRegex(RuntimeError, 'denied actions'):
            await adapter.turn('start')

    async def test_agy_denied_actions_with_nonempty_response_succeeds(self):
        adapter = self.agy()
        adapter.notifications.put_nowait(dict(event='result', result=dict(
            conversation_id='native-id', status='SUCCESS',
            denied_actions=[dict(tool='bash', command='rm -rf /')],
            response='partial output',
        )))
        await adapter.turn('start')

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
            if path == '/config':
                return {}
            self.fail('Reconnect must not create a new session: '+path)
        adapter.http = AsyncMock(side_effect=http)
        with patch('socket.socket', return_value=socket), patch.object(runner, 'executable', return_value='/bin/opencode'), \
                patch.object(runner.asyncio, 'create_subprocess_exec', new=AsyncMock()) as launch:
            await adapter.connect(self.assignment, self.j)
        self.assertEqual(adapter.native, 'ses_existing')
        self.assertEqual(adapter.model, dict(providerID='online', modelID='usable'))
        self.assertEqual(self.j.get('available_models'), ['online/usable'])
        warnings = [json.loads(row[0]) for row in self.j.db.execute("SELECT payload FROM events WHERE kind='warning'")]
        self.assertEqual(warnings, [dict(message='No model was assigned; OpenCode model online/usable was chosen automatically', model='online/usable')])
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

    async def test_opencode_model_selection_prefers_configured_default_and_skips_qwq_plus(self):
        providers = dict(all=[dict(id='alibaba', models={'qwq-plus': {}, 'qwen3.5-plus': {}}),
                              dict(id='zai-coding-plan', models={'glm-5.3': {}})],
                         connected=['alibaba', 'zai-coding-plan'],
                         default={'alibaba': 'qwq-plus', 'zai-coding-plan': 'glm-5.3'})
        available = ['alibaba/qwq-plus', 'alibaba/qwen3.5-plus', 'zai-coding-plan/glm-5.3']
        choose = runner.OpenCode.choose_model
        # The provider OpenCode itself marks as default (config "model") wins.
        self.assertEqual(choose(providers, available, 'zai-coding-plan/glm-5.3'), 'zai-coding-plan/glm-5.3')
        # Without a configured default the arbitrary per-provider catalog defaults
        # are ignored and the first tool-capable connected model is chosen.
        self.assertEqual(choose(providers, available, None), 'alibaba/qwen3.5-plus')
        # Even an explicitly configured qwq default is never auto-chosen.
        self.assertEqual(choose(providers, available, 'alibaba/qwq-plus'), 'alibaba/qwen3.5-plus')
        self.assertEqual(choose(providers, ['zai-coding-plan/qvq-max', 'zai-coding-plan/glm-5.3'], None), 'zai-coding-plan/glm-5.3')
        only_reasoning = dict(all=[dict(id='alibaba', models={'qwq-plus': {}})], connected=['alibaba'],
                              default={'alibaba': 'qwq-plus'})
        with self.assertRaisesRegex(RuntimeError, 'tool-capable'):
            choose(only_reasoning, ['alibaba/qwq-plus'], 'alibaba/qwq-plus')

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
                    modelID='actual', providerID='provider', time={'completed': 1}, finish='stop'),
                    parts=[dict(type='text', text='done')])]
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

    async def test_opencode_reasoning_only_reply_fails_with_turn_produced_no_actions(self):
        adapter = self.opencode()
        message_id = None
        async def http(method, path, body=None):
            nonlocal message_id
            if path.endswith('/prompt_async'):
                message_id = body['messageID']
                return None
            if path == '/permission':
                return []
            if '?limit=' in path:
                # The reply reasons at length but never calls a tool or answers.
                return [dict(info=dict(role='assistant', parentID=message_id, time={'completed': 1}, finish='stop'),
                             parts=[dict(type='step-start'), dict(type='reasoning', text='thinking about the task'),
                                    dict(type='reasoning', text='still only thinking')])]
            if path == '/session/status':
                return {adapter.native: dict(type='idle')}
            self.fail(path)
        adapter.http = AsyncMock(side_effect=http)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            with self.assertRaisesRegex(RuntimeError, 'turn produced no actions'):
                await adapter.turn('implement')
        # The turn itself ended (session idle), so no prompt stays pending.
        self.assertIsNone(self.j.get('opencode_pending_message'))

    async def test_opencode_no_action_turn_marks_the_run_failed(self):
        self.j.set('assignment', dict(self.assignment, agent='opencode'))
        class ReasoningOnly:
            async def connect(self, assignment, journal):
                journal.set('native_conversation_id', 'ses_native')
            async def turn(self, prompt):
                raise RuntimeError('turn produced no actions')
        with patch.object(runner, 'OpenCode', ReasoningOnly):
            await runner.run(dict(self.assignment, agent='opencode'), self.j)
        self.assertEqual(self.j.get('state'), 'failed')
        errors = [json.loads(row[0]) for row in self.j.db.execute("SELECT payload FROM events WHERE kind='error'")]
        self.assertTrue(any(error['message'] == 'turn produced no actions' for error in errors), errors)

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
                    finish='tool-calls' if polls == 1 else 'stop'),
                    parts=[dict(type='tool', tool='read', callID='call_1', state=dict(status='completed', input={}))])]
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
                        return [dict(info=dict(role='assistant', parentID=message_id, time={'completed': 1}, finish='stop'), parts=[part])]
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

    def cursor_script(self, events, exit_code=0, stderr=''):
        """A fake `cursor-agent` CLI: logs argv, prints canned stream-json events."""
        script = self.root/'cursor-agent'
        log = shlex.quote(str(self.root/'cursor-args.log'))
        parts = ['#!/bin/sh', '{ IFS=$(printf \'\\037\'); printf \'%s\\n\' "$*"; } >> '+log]
        for event in events:
            parts.append('printf \'%s\\n\' '+shlex.quote(runner.encode(event)))
        if stderr:
            parts.append('printf \'%s\\n\' '+shlex.quote(stderr)+' >&2')
        parts.append('exit '+str(exit_code))
        script.write_text('\n'.join(parts)+'\n')
        script.chmod(0o755)

    def cursor_args(self):
        return [line.split('\x1f') for line in (self.root/'cursor-args.log').read_text().splitlines()]

    def cursor_stream(self, text='ok', chat='chat-native', **result):
        """The event sequence a real `cursor-agent -p ... stream-json` turn emits."""
        init = dict(type='system', subtype='init', apiKeySource='login', cwd=str(self.workspace),
                    session_id=chat, model='GPT-5.2 Medium', permissionMode='default')
        user = dict(type='user', message=dict(role='user', content=[dict(type='text', text=text)]), session_id=chat)
        assistant = dict(type='assistant', message=dict(role='assistant', content=[dict(type='text', text=text)]), session_id=chat)
        end = dict(type='result', subtype='success', duration_ms=1844, duration_api_ms=1844,
                   is_error=False, result=text, session_id=chat, request_id='req-native',
                   usage=dict(inputTokens=11379, outputTokens=8, cacheReadTokens=0, cacheWriteTokens=0))
        end.update(result)
        return [init, user, assistant, end]

    async def test_cursor_per_turn_arguments_resume_and_native_events(self):
        self.cursor_script(self.cursor_stream())
        adapter = runner.Cursor()
        assignment = dict(self.assignment, agent='cursor', autonomy='yolo', model='gpt-5.2')
        with patch.object(runner, 'executable', return_value=str(self.root/'cursor-agent')):
            await adapter.connect(assignment, self.j)
            await adapter.turn('first prompt')
            await adapter.turn('follow-up')
        turns = self.cursor_args()
        self.assertEqual(turns[0], ['-p', 'first prompt', '--output-format', 'stream-json',
                                    '--trust', '--workspace', str(self.workspace),
                                    '--force', '--model', 'gpt-5.2'])
        self.assertNotIn('--resume', turns[0])
        # Turn 2 resumes the chat stored from turn 1; --force and --model persist.
        self.assertEqual(turns[1], ['-p', 'follow-up', '--output-format', 'stream-json',
                                    '--trust', '--workspace', str(self.workspace),
                                    '--force', '--model', 'gpt-5.2', '--resume', 'chat-native'])
        self.assertEqual(self.j.get('native_conversation_id'), 'chat-native')
        self.assertEqual(self.j.get('actual_model'), 'gpt-5.2')
        self.assertEqual(self.native_events(), self.cursor_stream()*2)

    def native_events(self):
        return [json.loads(row['payload']) for row in self.j.db.execute("SELECT payload FROM events WHERE kind='native' ORDER BY seq")]

    def metadata_writes(self):
        """Counts journal metadata writes per key from here on."""
        writes = {}
        original = self.j.set
        def counted(key, value):
            writes[key] = writes.get(key, 0) + 1
            original(key, value)
        self.j.set = counted
        return writes

    def recorded_cursor_turn(self, chat='chat-native'):
        """Shape of a real cursor-agent 2026.09.26 stream-json turn: every
        thinking token is its own line, tool calls start and complete, then
        assistant text arrives in segments before the terminal result."""
        stamp = dict(session_id=chat)
        think = lambda text, ms: dict(type='thinking', subtype='delta', text=text, timestamp_ms=ms, **stamp)
        say = lambda text, call: dict(type='assistant', model_call_id=call, timestamp_ms=1790615951602,
                                      message=dict(role='assistant', content=[dict(type='text', text=text)]), **stamp)
        shell = dict(shellToolCall=dict(args=dict(command='ls -la')))
        init, user = self.cursor_stream(chat=chat)[:2]
        return [init, user,
                think('**Exploring', 1), think(' the', 2), think(' workspace**', 3),
                dict(type='thinking', subtype='completed', timestamp_ms=4, **stamp),
                dict(type='tool_call', subtype='started', call_id='call_1', tool_call=shell, **stamp),
                dict(type='tool_call', subtype='completed', call_id='call_1',
                     tool_call=dict(shellToolCall=dict(shell['shellToolCall'], result=dict(success=dict(exitCode=0, stdout='a\n')))), **stamp),
                say('Workspace', 'call-a'), say(' is empty.', 'call-b'),
                think('Done', 5), think('.', 6),
                say('Finished.', 'call-c'),
                dict(type='result', subtype='success', is_error=False, result='Workspace is empty.Finished.',
                     request_id='req-native', **stamp)]

    async def test_cursor_coalesces_text_deltas_and_writes_metadata_once(self):
        events = self.recorded_cursor_turn()
        self.cursor_script(events)
        adapter = runner.Cursor()
        assignment = dict(self.assignment, agent='cursor', autonomy='yolo', model='gpt-5.2')
        with patch.object(runner, 'executable', return_value=str(self.root/'cursor-agent')):
            await adapter.connect(assignment, self.j)
            writes = self.metadata_writes()
            await adapter.turn('first prompt')
            await adapter.turn('follow-up')
        init, user = events[:2]
        thinking, completed, started, finished = events[2], events[5], events[6], events[7]
        assistant = events[8]
        expected = [
            init, user,
            dict(thinking, text='**Exploring the workspace**', coalesced=3),
            completed, started, finished,
            dict(assistant, message=dict(role='assistant', content=[dict(type='text', text='Workspace is empty.')]), coalesced=2),
            dict(events[10], text='Done.', coalesced=2),
            events[12],
            events[13],
        ]
        self.assertEqual(self.native_events(), expected*2)
        # Every line carries session_id; the id is written once, never re-written.
        self.assertEqual(writes, {'native_conversation_id': 1})
        self.assertEqual(self.j.get('native_conversation_id'), 'chat-native')

    async def test_cursor_init_display_name_is_never_the_verified_model(self):
        self.j.set('assignment', dict(self.assignment, agent='cursor', autonomy='yolo'))
        # Evidence recorded by an earlier runner from the init display name.
        self.j.set('actual_model', 'GPT-5.2 Medium')
        self.j.set('invocation', dict(model='GPT-5.2 Medium', verified_at=1))
        self.j.set('available_models', ['GPT-5.2 Medium', 'gpt-5.2'])
        self.cursor_script(self.cursor_stream())
        with patch.object(runner, 'executable', return_value=str(self.root/'cursor-agent')), \
                patch.object(runner.asyncio, 'sleep', new=AsyncMock(side_effect=asyncio.CancelledError)):
            # The run idles on an empty inbox after the first turn completes.
            with self.assertRaises(asyncio.CancelledError):
                await runner.run(dict(self.assignment, agent='cursor', autonomy='yolo'), self.j)
        self.assertEqual(self.native_events()[0]['model'], 'GPT-5.2 Medium')
        self.assertEqual(self.j.get('actual_model'), 'auto')
        self.assertEqual(self.j.get('invocation')['model'], 'auto')
        self.assertEqual(self.j.get('available_models'), ['gpt-5.2'])
        adapter = runner.Cursor()
        self.j.set('invocation', dict(model='GPT-5.2 Medium', verified_at=1))
        await adapter.connect(dict(self.assignment, agent='cursor', autonomy='yolo', model='gpt-5.2'), self.j)
        self.assertIsNone(self.j.get('invocation'))
        self.assertEqual(self.j.get('actual_model'), 'gpt-5.2')

    async def test_cursor_rejected_model_error_yields_available_models(self):
        adapter = runner.Cursor()
        assignment = dict(self.assignment, agent='cursor', autonomy='yolo', model='not-a-real-model')
        # Verbatim shape of cursor-agent's stderr for a rejected --model (list shortened).
        error = 'Cannot use this model: not-a-real-model. Available models: auto, gpt-5.3-codex, gpt-5.2, composer-2.5, claude-opus-4-8-high'
        self.cursor_script([], exit_code=1, stderr=error)
        with patch.object(runner, 'executable', return_value=str(self.root/'cursor-agent')):
            await adapter.connect(assignment, self.j)
            with self.assertRaisesRegex(RuntimeError, 'Cannot use this model'):
                await adapter.turn('work')
        self.assertEqual(self.j.get('available_models'), ['auto', 'gpt-5.3-codex', 'gpt-5.2', 'composer-2.5', 'claude-opus-4-8-high'])
        self.assertIsNone(self.j.get('invocation'))

    async def test_agy_coalesces_response_deltas_and_writes_conversation_once(self):
        adapter = self.agy()
        step = lambda **fields: dict(event='step_update', step_update=dict(conversation_id='native-id', **fields))
        delta = lambda index, text: step(state='ACTIVE', step_index=index, step_type='agent_response', text_delta=text)
        tool = step(state='ACTIVE', step_index=3, step_type='tool', tool_name='run_command',
                    tool_info=dict(name='run_command', parameters=dict(CommandLine='ls')))
        stream = [dict(event='init', conversation_id='native-id', init=dict(model='available-model')),
                  delta(2, '### Plan'), delta(2, '\n\nInspect'), delta(2, ' first.'), tool,
                  dict(tool, step_update=dict(tool['step_update'], state='DONE', duration_seconds=0.2)),
                  delta(4, 'Done'), delta(5, 'Next'),
                  step(state='DONE', step_index=5, step_type='agent_response', duration_seconds=1.0, usage={}),
                  dict(event='result', result=dict(conversation_id='native-id', status='SUCCESS', response='Done'))]
        for event in stream:
            adapter.notifications.put_nowait(event)
        writes = self.metadata_writes()
        await adapter.turn('start')
        self.assertEqual(self.native_events(), [
            stream[0],
            dict(delta(2, '### Plan\n\nInspect first.'), coalesced=3),
            stream[4], stream[5], stream[6], stream[7], stream[8], stream[9],
        ])
        self.assertEqual(writes, {'native_conversation_id': 1, 'actual_model': 1})

    async def test_opencode_journals_each_message_revision_once_across_turns(self):
        adapter = self.opencode()
        message_id, polls = None, 0
        def reply(parent, ident, text, completed=False, parts=()):
            info = dict(id=ident, role='assistant', parentID=parent, modelID='actual', providerID='provider',
                        time=dict(created=1, **({'completed': 2} if completed else {})), finish='stop' if completed else None)
            return dict(info=info, parts=[dict(type='text', text=text), *parts])
        history = []
        async def http(method, path, body=None):
            nonlocal message_id, polls
            if path.endswith('/prompt_async'):
                message_id, polls = body['messageID'], 0
            elif path == '/permission':
                return []
            elif '?limit=' in path:
                polls += 1
                ident = 'msg_reply_'+message_id
                tool = [dict(type='tool', tool='read', callID='c', state=dict(status='running', input={}))] if polls > 2 else []
                current = reply(message_id, ident, 'do'+'ne'*polls, completed=polls == 4, parts=tool)
                return history+[current]
            elif path == '/session/status':
                return {adapter.native: dict(type='idle')}
            else:
                self.fail(path)
        adapter.http = AsyncMock(side_effect=http)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            await adapter.turn('first')
            first = self.native_events()
            history.append(first[-1])
            await adapter.turn('second')
        # Growing text alone is not journaled; a new tool part and completion are.
        self.assertEqual([(e['parts'][0]['text'], len(e['parts'])) for e in first],
                         [('done', 1), ('donenene', 2), ('donenenene', 2)])
        # The prior turn's messages are in every later message list; never again.
        self.assertEqual(len(self.native_events()), 6)
        restarted = self.opencode()
        restarted.j = self.j
        self.assertEqual(restarted.journaled(), adapter.revisions)

    def opencode_session(self, adapter, history, turn, steps):
        """Serve `history` plus `turn(message_id)` the way OpenCode 1.18 does:
        GET ...?limit=N returns the newest N messages in chronological order.
        Each poll reveals the next `steps` messages of the turn; the session is
        busy until the whole turn is visible."""
        state = dict(message_id=None, shown=0, timeline=None, pages=[])
        async def http(method, path, body=None):
            if path.endswith('/prompt_async'):
                state['message_id'] = body['messageID']
                state['timeline'] = turn(body['messageID'])
            elif path == '/permission':
                return []
            elif '/message?limit=' in path:
                state['shown'] = min(state['shown']+steps, len(state['timeline']))
                limit = int(path.rsplit('=', 1)[1])
                page = (history+state['timeline'][:state['shown']])[-limit:]
                state['pages'].append(page)
                return page
            elif path == '/session/status':
                done = state['shown'] == len(state['timeline'])
                return {adapter.native: dict(type='idle' if done else 'busy')}
            else:
                self.fail(path)
        adapter.http = AsyncMock(side_effect=http)
        return state

    @staticmethod
    def opencode_id(milliseconds, counter):
        """OpenCode's ascending id: low 48 bits of milliseconds*4096+counter."""
        return 'msg_'+format((milliseconds*0x1000+counter) & ((1 << 48)-1), '012x')+'Zz09AbCdEfGhIj'

    @staticmethod
    def opencode_message(ident, role, created, parent=None, finish='stop', parts=(), **extra):
        info = dict(id=ident, sessionID='ses_native', role=role, time=dict(created=created), **extra)
        if role == 'assistant':
            info['time']['completed'] = created+1
            info.update(parentID=parent, finish=finish, providerID='provider', modelID='big-pickle')
        return dict(info=info, parts=list(parts))

    def compaction_turn(self, pre_steps=1, post_steps=0, replies_act=True):
        """The BUG15 recorded shape: the runner prompt, tool rounds, an
        auto-created compaction user message with its summary, OpenCode's own
        continue message and the final reply parented to that message."""
        message = self.opencode_message
        tool = dict(type='tool', tool='bash', callID='call_pre', state=dict(status='completed', input=dict(command='ls')))
        text = [dict(type='text', text='summary / done')] if replies_act else [dict(type='reasoning', text='thinking')]
        def turn(prompt):
            now = int(time.time()*1000)
            later = lambda step: (now+step, self.opencode_id(now+step, 0))
            timeline = [message(prompt, 'user', now, parts=[dict(type='text', text='follow-up')])]
            timeline += [message(later(1+step)[1], 'assistant', later(1+step)[0], prompt, finish='tool-calls', parts=[tool])
                         for step in range(pre_steps)]
            created, compaction = later(1000)
            timeline.append(message(compaction, 'user', created, parts=[dict(type='compaction', auto=True)]))
            timeline.append(message(later(1001)[1], 'assistant', later(1001)[0], compaction, summary=True, parts=text))
            created, resumed = later(1002)
            timeline.append(message(resumed, 'user', created, parts=[dict(type='text', text='Continue if you have next steps', synthetic=True)]))
            timeline += [message(later(1003+step)[1], 'assistant', later(1003+step)[0], resumed, finish='tool-calls', parts=[])
                         for step in range(post_steps)]
            timeline.append(message(later(5000)[1], 'assistant', later(5000)[0], resumed, parts=text))
            return timeline
        return turn

    def older_history(self, count):
        """Completed messages of earlier turns, already journaled."""
        base = int(time.time()*1000)-600_000
        history = []
        for index in range(0, count, 2):
            user = self.opencode_id(base+index, 0)
            history.append(self.opencode_message(user, 'user', base+index, parts=[dict(type='text', text='old')]))
            history.append(self.opencode_message(self.opencode_id(base+index+1, 0), 'assistant', base+index+1, user,
                                                 parts=[dict(type='text', text='old reply')]))
        for message in history:
            self.j.emit('native', message)
        return history

    async def test_opencode_turn_ends_on_reply_parented_to_compaction_message(self):
        adapter = self.opencode()
        history = self.older_history(20)
        state = self.opencode_session(adapter, history, self.compaction_turn(), steps=2)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            await adapter.turn('follow-up')
        self.assertIsNone(self.j.get('opencode_pending_message'))
        self.assertEqual(self.j.get('actual_model'), 'provider/big-pickle')
        # Every message of the turn is journaled once; the old history never again.
        journaled = [event['info']['id'] for event in self.native_events()]
        self.assertEqual(journaled, [m['info']['id'] for m in history+state['timeline']])

    async def test_opencode_counts_actions_made_before_compaction(self):
        # After compaction only reasoning follows; the tool call made under the
        # runner prompt before compaction still makes the turn verifiable.
        adapter = self.opencode()
        self.opencode_session(adapter, [], self.compaction_turn(replies_act=False), steps=1)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            await adapter.turn('follow-up')
        self.assertIsNone(self.j.get('opencode_pending_message'))
        adapter = self.opencode()
        self.j.set('opencode_pending_message', None)
        self.opencode_session(adapter, [], self.compaction_turn(pre_steps=0, replies_act=False), steps=1)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            with self.assertRaisesRegex(RuntimeError, 'turn produced no actions'):
                await adapter.turn('follow-up')

    async def test_opencode_session_longer_than_page_size(self):
        # 150 old messages, 120 tool rounds before compaction and 110 after it:
        # the prompt and the compaction messages leave the newest page long
        # before the final reply arrives.
        adapter = self.opencode()
        history = self.older_history(150)
        state = self.opencode_session(adapter, history, self.compaction_turn(pre_steps=120, post_steps=110), steps=7)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            await adapter.turn('follow-up')
        self.assertIsNone(self.j.get('opencode_pending_message'))
        final = state['timeline'][-1]['info']['id']
        self.assertEqual(state['pages'][-1][-1]['info']['id'], final)
        self.assertTrue(all(len(page) <= 100 for page in state['pages']))
        self.assertNotIn(state['message_id'], [m['info']['id'] for m in state['pages'][-1]])
        journaled = [event['info']['id'] for event in self.native_events()]
        self.assertEqual(journaled, [m['info']['id'] for m in history+state['timeline']])

    async def test_opencode_older_user_messages_never_end_a_new_turn(self):
        # Idle session whose newest completed reply answers an earlier turn.
        adapter = self.opencode()
        history = self.older_history(10)
        polls = 0
        class StillWaiting(Exception):
            pass
        async def http(method, path, body=None):
            nonlocal polls
            if path.endswith('/prompt_async') or path == '/permission':
                return []
            if '/message?limit=' in path:
                polls += 1
                if polls == 3:
                    raise StillWaiting
                return history
            if path == '/session/status':
                return {adapter.native: dict(type='idle')}
            self.fail(path)
        adapter.http = AsyncMock(side_effect=http)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            with self.assertRaises(StillWaiting):
                await adapter.turn('follow-up')
        self.assertTrue(self.j.get('opencode_pending_message'))

    async def test_cursor_omits_model_and_resume_before_they_apply(self):
        self.cursor_script(self.cursor_stream())
        adapter = runner.Cursor()
        assignment = dict(self.assignment, agent='cursor', autonomy='yolo')
        with patch.object(runner, 'executable', return_value=str(self.root/'cursor-agent')):
            await adapter.connect(assignment, self.j)
            await adapter.turn('first prompt')
        self.assertEqual(self.cursor_args()[0], ['-p', 'first prompt', '--output-format', 'stream-json',
                                                '--trust', '--workspace', str(self.workspace), '--force'])

    async def test_cursor_failures_raise_with_error_text(self):
        adapter = runner.Cursor()
        assignment = dict(self.assignment, agent='cursor', autonomy='yolo')
        with patch.object(runner, 'executable', return_value=str(self.root/'cursor-agent')):
            await adapter.connect(assignment, self.j)
            # A rejected model never reaches NDJSON: stderr plus a non-zero exit.
            self.cursor_script([], exit_code=1, stderr='Cannot use this model: not-a-real-model')
            with self.assertRaisesRegex(RuntimeError, 'Cannot use this model: not-a-real-model'):
                await adapter.turn('work')
            # An error event mid-stream carries the message.
            self.cursor_script([dict(type='error', message='rate limited')])
            with self.assertRaisesRegex(RuntimeError, 'rate limited'):
                await adapter.turn('work')
            # A failed result is not a completed turn.
            self.cursor_script(self.cursor_stream(is_error=True, subtype='error', result=''))
            with self.assertRaisesRegex(RuntimeError, 'error'):
                await adapter.turn('work')
            # A stream that stops early is never treated as a completed turn.
            self.cursor_script(self.cursor_stream()[:2])
            with self.assertRaisesRegex(RuntimeError, 'without a terminal result event'):
                await adapter.turn('work')

    async def test_cursor_requires_yolo_autonomy_for_setup(self):
        adapter = runner.Cursor()
        for autonomy in ('ask', None):
            with self.subTest(autonomy=autonomy):
                with self.assertRaisesRegex(RuntimeError, 'no approval bridge'):
                    await adapter.connect(dict(self.assignment, agent='cursor', autonomy=autonomy), self.j)

    def test_cursor_probe_uses_the_cursor_agent_binary(self):
        self.assertEqual(runner.executable('cursor'), runner.executable('cursor-agent'))

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

    def codex(self):
        adapter = runner.Codex()
        adapter.j, adapter.a, adapter.native, adapter.items = self.j, self.assignment, 'native-thread', {}
        adapter.rpc, adapter.send, adapter.notifications = AsyncMock(), AsyncMock(), asyncio.Queue()
        return adapter

    # Shapes follow `codex app-server generate-json-schema` (codex-cli 0.155):
    # AccountRateLimitsUpdatedNotification, ErrorNotification, TurnCompletedNotification.
    @staticmethod
    def rate_limits(primary, secondary=None, reached=None):
        limits = dict(limitId='codex', limitName=None, planType='plus', credits=None,
                      rateLimitReachedType=reached, primary=primary, secondary=secondary)
        return dict(method='account/rateLimits/updated', params=dict(rateLimits=limits))

    USAGE_ERROR = dict(message="You've hit your usage limit. Upgrade to Pro or try again later.",
                       codexErrorInfo='usageLimitExceeded', additionalDetails=None)

    async def test_codex_usage_limit_with_reset_pauses_instead_of_failing(self):
        adapter = self.codex()
        resets_at = int(time.time()) + 3600
        for event in (
                self.rate_limits(dict(usedPercent=82, windowDurationMins=300, resetsAt=resets_at-600),
                                 dict(usedPercent=40, windowDurationMins=10080, resetsAt=resets_at+86400)),
                self.rate_limits(dict(usedPercent=100, windowDurationMins=300, resetsAt=resets_at), None,
                                 reached='rate_limit_reached'),
                dict(method='error', params=dict(error=self.USAGE_ERROR, threadId='native-thread', turnId='turn-1', willRetry=False)),
                dict(method='turn/completed', params=dict(threadId='native-thread', turn=dict(
                    id='turn-1', items=[], status='failed', error=self.USAGE_ERROR)))):
            adapter.notifications.put_nowait(event)
        with self.assertRaises(runner.QuotaPaused) as raised:
            await adapter.turn('work')
        self.assertEqual((raised.exception.agent, raised.exception.resets_at), ('codex', resets_at))
        usage = self.j.get('usage')
        self.assertEqual((usage['used_percent'], usage['resets_at'], usage['exhausted']), (100, resets_at, True))
        # The sparse second update kept the weekly window from the first.
        self.assertEqual(usage['windows']['secondary']['used_percent'], 40)

    async def test_codex_usage_limit_error_before_a_bare_failed_turn_still_pauses(self):
        adapter = self.codex()
        resets_at = int(time.time()) + 1800
        adapter.notifications.put_nowait(self.rate_limits(dict(usedPercent=100, windowDurationMins=300, resetsAt=resets_at)))
        adapter.notifications.put_nowait(dict(method='error', params=dict(error=self.USAGE_ERROR, threadId='native-thread', turnId='t', willRetry=False)))
        adapter.notifications.put_nowait(dict(method='turn/completed', params=dict(threadId='native-thread', turn=dict(id='t', items=[], status='failed', error=None))))
        with self.assertRaises(runner.QuotaPaused) as raised:
            await adapter.turn('work')
        self.assertEqual(raised.exception.resets_at, resets_at)

    async def test_codex_usage_limit_without_any_reset_time_stays_a_failure(self):
        adapter = self.codex()
        adapter.notifications.put_nowait(dict(method='turn/completed', params=dict(threadId='native-thread', turn=dict(
            id='t', items=[], status='failed', error=self.USAGE_ERROR))))
        with self.assertRaises(RuntimeError) as raised:
            await adapter.turn('work')
        self.assertNotIsInstance(raised.exception, runner.QuotaPaused)

    async def test_codex_other_turn_failures_and_retried_limits_are_not_quota_pauses(self):
        resets_at = int(time.time()) + 600
        other = dict(message='stream disconnected', codexErrorInfo='internalServerError')
        for events in (
                [dict(method='turn/completed', params=dict(threadId='native-thread', turn=dict(id='t', items=[], status='failed', error=other)))],
                [self.rate_limits(dict(usedPercent=100, resetsAt=resets_at)),
                 dict(method='error', params=dict(error=self.USAGE_ERROR, threadId='native-thread', turnId='t', willRetry=True)),
                 dict(method='turn/completed', params=dict(threadId='native-thread', turn=dict(id='t', items=[], status='interrupted', error=None)))]):
            with self.subTest(events=events[-1]):
                adapter = self.codex()
                for event in events:
                    adapter.notifications.put_nowait(event)
                with self.assertRaises(RuntimeError) as raised:
                    await adapter.turn('work')
                self.assertNotIsInstance(raised.exception, runner.QuotaPaused)

    async def test_codex_rate_limit_updates_record_usage_during_a_successful_turn(self):
        adapter = self.codex()
        adapter.notifications.put_nowait(self.rate_limits(dict(usedPercent=37, windowDurationMins=300, resetsAt=1790000000)))
        adapter.notifications.put_nowait(dict(method='turn/completed', params=dict(threadId='native-thread', turn=dict(id='t', items=[], status='completed', error=None))))
        await adapter.turn('work')
        usage = self.j.get('usage')
        self.assertEqual((usage['agent'], usage['used_percent'], usage['resets_at'], usage['exhausted']),
                         ('codex', 37, 1790000000, False))

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


class Clock:
    def __init__(self, now=1_790_000_000.0):
        self.now = now

    def __call__(self):
        return self.now

    def advance(self, minutes):
        self.now += minutes*60


class StallWatchdogContracts(unittest.IsolatedAsyncioTestCase):
    """BUG18: a background loop inherited a bash tool's output pipe, so the
    finished build's tool call never saw EOF and the run sat 'working'."""

    BUILD = "/bin/zsh -lc 'cargo build --workspace --locked; ( while true; do date; sleep 20; done > progress.log ) &'"

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.ident = str(uuid.uuid4())
        self.j = runner.Journal(self.root / self.ident)
        self.j.quiet = True
        self.assignment = dict(id=self.ident, agent='codex', workspace=str(self.root), objective='test')
        self.j.set('assignment', self.assignment)
        self.clock = Clock()
        self.watchdog = runner.StallWatchdog(self.j, clock=self.clock)

    def tearDown(self):
        self.j.db.close()
        self.temp.cleanup()

    def stalled(self):
        return [json.loads(row[0]) for row in self.j.db.execute("SELECT payload FROM events WHERE kind='stalled' ORDER BY seq")]

    def test_a_silent_tool_stalls_exactly_once_and_names_its_command(self):
        self.watchdog.event(started={'call_1': ('commandExecution', self.BUILD)})
        self.clock.advance(runner.STALL_MINUTES - 0.5)
        self.assertIsNone(self.watchdog.check())
        self.assertEqual(self.stalled(), [])
        self.clock.advance(0.5)
        self.watchdog.check()
        for _ in range(20):
            self.clock.advance(1)
            self.watchdog.check()
        stalled = self.stalled()
        self.assertEqual(len(stalled), 1)
        self.assertEqual(stalled[0]['tool'], 'commandExecution')
        self.assertEqual(stalled[0]['command'], self.BUILD)
        self.assertEqual(stalled[0]['silent_minutes'], runner.STALL_MINUTES)
        self.assertEqual(stalled[0]['reason'], 'Stalled: '+self.BUILD+' silent for 10 min')
        # The run's metadata keeps the current silence; the event is not repeated.
        self.assertEqual(self.j.get('stall')['silent_minutes'], runner.STALL_MINUTES+20)
        self.assertEqual(self.j.get('stall')['reason'], 'Stalled: '+self.BUILD+' silent for 30 min')

    def test_the_command_is_bounded(self):
        self.watchdog.event(started={'call_1': ('bash', 'x'*1000)})
        self.clock.advance(runner.STALL_MINUTES)
        stall = self.watchdog.check()
        self.assertEqual(len(stall['command']), 300)
        self.assertEqual(self.j.get('stall')['command'], 'x'*300)

    def test_new_output_clears_the_stall_and_a_fresh_one_needs_another_full_silence(self):
        self.watchdog.event(started={'call_1': ('bash', 'cargo build')})
        self.clock.advance(runner.STALL_MINUTES)
        self.watchdog.check()
        self.assertEqual(len(self.stalled()), 1)
        self.watchdog.event()
        self.assertIsNone(self.j.get('stall'))
        self.clock.advance(runner.STALL_MINUTES - 1)
        self.watchdog.check()
        self.assertEqual(len(self.stalled()), 1)
        self.clock.advance(1)
        self.watchdog.check()
        self.assertEqual(len(self.stalled()), 2)

    def test_no_stall_while_events_keep_flowing_or_no_tool_runs(self):
        self.watchdog.event(started={'call_1': ('bash', 'cargo test')})
        for _ in range(120):
            self.clock.advance(1)
            self.watchdog.event()
            self.watchdog.check()
        self.watchdog.event(finished=['call_1'])
        self.clock.advance(runner.STALL_MINUTES*6)
        self.watchdog.check()
        self.assertEqual(self.stalled(), [])
        self.assertIsNone(self.j.get('stall'))

    def test_recorded_shapes_name_their_tool_and_command(self):
        started = dict(method='item/started', params=dict(threadId='thr', turnId='turn', item=dict(
            type='commandExecution', id='call_1', command=self.BUILD, cwd='/w', status='inProgress',
            commandActions=[], aggregatedOutput=None, exitCode=None)))
        completed = dict(started, method='item/completed')
        agent = dict(method='item/started', params=dict(item=dict(type='agentMessage', id='msg_1', text='')))
        self.assertEqual(runner.codex_tool_calls(started), ({'call_1': ('commandExecution', self.BUILD)}, ()))
        self.assertEqual(runner.codex_tool_calls(completed), ({}, ('call_1',)))
        self.assertEqual(runner.codex_tool_calls(agent), ({}, ()))
        # Node bridge: SDK messages. Python bridge: flattened dataclasses.
        node_use = dict(type='assistant', message=dict(content=[dict(type='text', text='Building'),
            dict(type='tool_use', id='toolu_1', name='Bash', input=dict(command='cargo build'))]))
        python_use = dict(type='assistant', content=[dict(text='Building'),
            dict(id='toolu_2', name='Read', input=dict(file_path='a.py'))])
        node_result = dict(type='user', message=dict(content=[dict(type='tool_result', tool_use_id='toolu_1', content='ok')]))
        python_result = dict(type='user', content=[dict(tool_use_id='toolu_2', content='x', is_error=False)])
        self.assertEqual(runner.claude_tool_calls(node_use), ({'toolu_1': ('Bash', 'cargo build')}, []))
        self.assertEqual(runner.claude_tool_calls(python_use), ({'toolu_2': ('Read', '{"file_path":"a.py"}')}, []))
        self.assertEqual(runner.claude_tool_calls(node_result), ({}, ['toolu_1']))
        self.assertEqual(runner.claude_tool_calls(python_result), ({}, ['toolu_2']))
        self.assertEqual(runner.claude_tool_calls(dict(type='user', message=dict(content='prompt'))), ({}, []))
        shell = dict(shellToolCall=dict(args=dict(command='ls -la')))
        self.assertEqual(runner.cursor_tool_calls(dict(type='tool_call', subtype='started', call_id='c1', tool_call=shell)),
                         ({'c1': ('shell', 'ls -la')}, ()))
        self.assertEqual(runner.cursor_tool_calls(dict(type='tool_call', subtype='completed', call_id='c1', tool_call=shell)),
                         ({}, ('c1',)))

    async def test_codex_turn_reports_a_silent_command_once_and_clears_on_output(self):
        adapter = runner.Codex()
        adapter.a, adapter.j, adapter.native, adapter.items = self.assignment, self.j, 'thr', {}
        adapter.notifications = asyncio.Queue()
        adapter.rpc, adapter.send = AsyncMock(), AsyncMock()
        adapter.watchdog = self.watchdog
        item = dict(type='commandExecution', id='call_1', command=self.BUILD, cwd='/w', status='inProgress')
        put = adapter.notifications.put_nowait
        put(dict(method='turn/started', params=dict(threadId='thr', turn=dict(id='turn', status='inProgress'))))
        put(dict(method='item/started', params=dict(threadId='thr', turnId='turn', item=item)))
        with patch.object(runner, 'STALL_CHECK', 0.01):
            turn = asyncio.create_task(adapter.turn('build'))
            await asyncio.sleep(0.05)
            self.assertEqual(self.stalled(), [])
            self.clock.advance(runner.STALL_MINUTES)
            await asyncio.sleep(0.1)
            self.assertEqual(len(self.stalled()), 1)
            self.assertEqual(self.j.get('stall')['command'], self.BUILD)
            put(dict(method='item/commandExecution/outputDelta', params=dict(itemId='call_1', delta='Finished\n')))
            await asyncio.sleep(0.05)
            self.assertIsNone(self.j.get('stall'))
            self.clock.advance(runner.STALL_MINUTES)
            await asyncio.sleep(0.1)
            self.assertEqual(len(self.stalled()), 2)
            put(dict(method='item/completed', params=dict(threadId='thr', turnId='turn', item=dict(item, status='completed', exitCode=0))))
            put(dict(method='turn/completed', params=dict(threadId='thr', turn=dict(id='turn', status='completed', error=None))))
            await asyncio.wait_for(turn, 1)
        self.assertEqual(len(self.stalled()), 2)
        self.assertIsNone(self.j.get('stall'))
        # The turn's own events are all still journaled: checks lose nothing.
        methods = [json.loads(row[0]).get('method') for row in self.j.db.execute("SELECT payload FROM events WHERE kind='native' ORDER BY seq")]
        self.assertEqual(methods, ['turn/started', 'item/started', 'item/commandExecution/outputDelta', 'item/completed', 'turn/completed'])

    async def test_codex_time_waiting_for_an_approval_is_not_a_stall(self):
        adapter = runner.Codex()
        adapter.a, adapter.j, adapter.native, adapter.items = self.assignment, self.j, 'thr', {}
        adapter.notifications = asyncio.Queue()
        adapter.rpc, adapter.send = AsyncMock(), AsyncMock()
        adapter.watchdog = self.watchdog
        async def decided_after_half_an_hour(*args):
            self.clock.advance(30)
            return True
        item = dict(type='commandExecution', id='call_1', command='cargo publish', status='inProgress')
        put = adapter.notifications.put_nowait
        put(dict(method='item/started', params=dict(threadId='thr', turnId='turn', item=item)))
        put(dict(id=7, method='item/commandExecution/requestApproval', params=dict(threadId='thr', itemId='call_1', command='cargo publish')))
        with patch.object(runner, 'STALL_CHECK', 0.01), patch.object(runner, 'permission', new=decided_after_half_an_hour):
            turn = asyncio.create_task(adapter.turn('publish'))
            await asyncio.sleep(0.1)
            self.assertEqual(self.stalled(), [])
            put(dict(method='item/completed', params=dict(threadId='thr', turnId='turn', item=dict(item, status='completed', exitCode=0))))
            put(dict(method='turn/completed', params=dict(threadId='thr', turn=dict(id='turn', status='completed', error=None))))
            await asyncio.wait_for(turn, 1)
        self.assertEqual(self.stalled(), [])

    async def test_claude_turn_with_flowing_events_never_stalls(self):
        adapter = runner.Claude()
        adapter.a, adapter.j = self.assignment, self.j
        adapter.notifications = asyncio.Queue()
        adapter.send = AsyncMock()
        adapter.watchdog = self.watchdog
        with patch.object(runner, 'STALL_CHECK', 0.01):
            turn = asyncio.create_task(adapter.turn('build'))
            adapter.notifications.put_nowait(dict(type='assistant', message=dict(content=[
                dict(type='tool_use', id='toolu_1', name='Bash', input=dict(command='cargo build'))])))
            for _ in range(30):
                await asyncio.sleep(0.02)
                self.clock.advance(1)
                adapter.notifications.put_nowait(dict(type='system', subtype='status'))
            await asyncio.sleep(0.05)
            self.assertEqual(self.stalled(), [])
            self.clock.advance(runner.STALL_MINUTES)
            await asyncio.sleep(0.1)
            self.assertEqual([s['tool'] for s in self.stalled()], ['Bash'])
            adapter.notifications.put_nowait(dict(type='user', message=dict(content=[dict(type='tool_result', tool_use_id='toolu_1', content='ok')])))
            adapter.notifications.put_nowait(dict(type='result', subtype='success', is_error=False, result='done'))
            await asyncio.wait_for(turn, 1)
        self.assertEqual(len(self.stalled()), 1)
        self.assertIsNone(self.j.get('stall'))

    async def test_opencode_running_bash_part_stalls_once_per_silence(self):
        adapter = runner.OpenCode()
        adapter.j, adapter.a = self.j, dict(self.assignment, agent='opencode')
        adapter.native = 'ses_native'
        adapter.model = dict(providerID='provider', modelID='model')
        adapter.watchdog = self.watchdog
        message_id, polls, observed = None, 0, []
        def tool(status, output=''):
            return dict(type='tool', tool='bash', callID='call_1', state=dict(
                status=status, input=dict(command=self.BUILD, description='Build'),
                metadata=dict(output=output), time=dict(start=1)))
        async def http(method, path, body=None):
            nonlocal message_id, polls
            if path.endswith('/prompt_async'):
                message_id = body['messageID']
            elif path == '/permission':
                return []
            elif '?limit=' in path:
                polls += 1
                # Poll: 1 starts the tool; 2 is silent for a full interval; 3 is
                # silent still; 4 has new output; 5 is silent for less than an
                # interval, 6 for a full one; 7 finishes the tool and the turn.
                self.clock.advance({2: runner.STALL_MINUTES, 3: 5, 5: runner.STALL_MINUTES-1, 6: 1}.get(polls, 0))
                output = 'Compiling' if polls < 4 else 'Compiling\nFinished'
                done = polls >= 7
                info = dict(id='msg_reply', role='assistant', parentID=message_id, time=dict(created=1, **({'completed': 2} if done else {})),
                            finish='stop' if done else None, providerID='provider', modelID='model')
                return [dict(info=info, parts=[tool('completed' if done else 'running', output)])]
            elif path == '/session/status':
                observed.append(len(self.stalled()))
                return {adapter.native: dict(type='idle' if polls >= 7 else 'busy')}
            else:
                self.fail(path)
        adapter.http = AsyncMock(side_effect=http)
        with patch.object(runner.asyncio, 'sleep', new=AsyncMock()):
            await adapter.turn('build')
        self.assertEqual(observed, [0, 1, 1, 1, 1, 2, 2])
        self.assertEqual([(s['tool'], s['command']) for s in self.stalled()], [('bash', self.BUILD)]*2)
        self.assertIsNone(self.j.get('stall'))


if __name__ == '__main__':
    unittest.main()
