"""Persistent Python Agent SDK bridge for devices without Node.js."""
import asyncio
from dataclasses import asdict
import json
import sys
import uuid
from claude_agent_sdk import (ClaudeSDKClient, ClaudeAgentOptions, HookMatcher,
                              PermissionResultAllow, PermissionResultDeny,
                              tool, create_sdk_mcp_server)


def output(value):
    print(json.dumps(value, default=str), flush=True)


async def main():
    if len(sys.argv)>1 and sys.argv[1]=='--models':
        async with ClaudeSDKClient(options=ClaudeAgentOptions(cli_path=sys.argv[2], setting_sources=[])) as client:
            info=await client.get_server_info()
            output([m['value'] for m in (info or {}).get('models', [])])
        return
    stream = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(stream)
    await asyncio.get_running_loop().connect_read_pipe(lambda: protocol, sys.stdin)
    config = json.loads(await stream.readline())
    assignment = config['assignment']
    prompts = asyncio.Queue()
    decisions = {}

    async def reader():
        while True:
            line = await stream.readline()
            if not line:
                await prompts.put(None)
                return
            message = json.loads(line)
            if message['type'] == 'prompt':
                await prompts.put(message['text'])
            elif message['type'] == 'decision':
                future = decisions.pop(message['id'], None)
                if future:
                    future.set_result(message['allowed'])

    async def ask(name, args):
        ident = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        decisions[ident] = future
        output(dict(type='permission', request_id=ident, tool=name, input=args))
        return await future

    async def pre_tool(data, tool_use_id, context):
        return {'hookSpecificOutput': {'hookEventName': 'PreToolUse',
                'permissionDecision': 'allow' if await ask(data['tool_name'], data['tool_input']) else 'deny',
                'permissionDecisionReason': 'Hive task policy and one-use approval'}}

    async def can_use_tool(name, args, context):
        return PermissionResultAllow(updated_input=args) if await ask(name, args) else PermissionResultDeny(message='Denied by Hive')

    @tool('peer', 'Send a task-scoped peer question, answer, agreement or deployment result through Hive.',
          {'to':str, 'kind':str, 'text':str})
    async def peer(args):
        # Native tools avoid asking an agent to write control-plane journals
        # outside its workspace sandbox.
        if args['to'] not in [p['id'] for p in assignment.get('peers', [])] or args['kind'] not in ('question','answer','agreement','deployment') or not 0<len(args['text'])<=16000:
            return {'content':[{'type':'text','text':'Invalid task peer message'}], 'isError':True}
        output(dict(type='peer', **args))
        return {'content':[{'type':'text','text':'Peer message sent to Hive'}]}

    options = ClaudeAgentOptions(cwd=assignment['workspace'], cli_path=assignment.get('executable'),
                model=assignment.get('model'), resume=config.get('resume'), permission_mode='default',
                setting_sources=[], hooks={'PreToolUse':[HookMatcher(hooks=[pre_tool])]}, can_use_tool=can_use_tool,
                mcp_servers={'hive':create_sdk_mcp_server(name='hive',tools=[peer])})
    async def input_stream():
        # Keep the input iterable alive across result boundaries. SDK versions
        # that close an empty input stream cannot support later query() calls.
        while True:
            prompt = await prompts.get()
            if prompt is None:
                return
            yield {'type':'user', 'message':{'role':'user', 'content':prompt}}

    reading = asyncio.create_task(reader())
    client = ClaudeSDKClient(options=options)
    try:
        await client.connect(input_stream())
        info=await client.get_server_info()
        output(dict(type='models',models=[m['value'] for m in (info or {}).get('models', [])]))
        async for message in client.receive_messages():
            value=asdict(message)
            kind=type(message).__name__.replace('Message','').lower()
            if kind=='system':
                value=dict(value.get('data', {}), subtype=value.get('subtype'))
            value['type'] = kind
            output(value)
    finally:
        reading.cancel()
        await client.disconnect()


if __name__=='__main__':
    try:
        asyncio.run(main())
    except Exception as error:
        output(dict(type='error',message=str(error)))
        raise
