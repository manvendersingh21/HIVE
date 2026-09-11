import { query } from '@anthropic-ai/claude-agent-sdk';
import readline from 'node:readline';
import { randomUUID } from 'node:crypto';

const output = value => process.stdout.write(JSON.stringify(value) + '\n');
const decisions = new Map();
const prompts = [];
let wake;
async function* input() {
  for (;;) {
    if (!prompts.length) await new Promise(resolve => { wake = resolve; });
    while (prompts.length) yield { type: 'user', message: { role: 'user', content: prompts.shift() } };
  }
}
async function permission(tool, args) {
  const id = randomUUID();
  const answer = new Promise(resolve => decisions.set(id, resolve));
  output({ type: 'permission', request_id: id, tool, input: args });
  return await answer;
}
async function start(config) {
  const a = config.assignment;
  const session = query({ prompt: input(), options: {
    cwd: a.workspace,
    ...(a.model ? { model: a.model } : {}),
    ...(config.resume ? { resume: config.resume } : {}),
    pathToClaudeCodeExecutable: a.executable,
    permissionMode: 'default',
    settingSources: [],
    // The hook runs even when native allow rules would skip canUseTool.
    hooks: { PreToolUse: [{ hooks: [async data => ({ hookSpecificOutput: {
      hookEventName: 'PreToolUse',
      permissionDecision: await permission(data.tool_name, data.tool_input) ? 'allow' : 'deny',
      permissionDecisionReason: 'Hive task policy and one-use approval'
    } })] }] },
    canUseTool: async (tool, args) => await permission(tool, args)
      ? { behavior: 'allow', updatedInput: args }
      : { behavior: 'deny', message: 'Denied by Hive' },
  } });
  for await (const event of session) output(event);
}
if (process.argv[2] === '--models') {
  const session = query({ prompt: input(), options: { pathToClaudeCodeExecutable: process.argv[3], settingSources: [], permissionMode: 'default' } });
  try { output((await session.supportedModels()).map(model => model.value)); }
  finally { session.close(); }
} else readline.createInterface({ input: process.stdin }).on('line', line => {
  try {
    const message = JSON.parse(line);
    if (message.type === 'configure') start(message).catch(e => { output({ type: 'error', message: String(e) }); process.exitCode = 1; });
    if (message.type === 'prompt') { prompts.push(message.text); wake?.(); }
    if (message.type === 'decision') { decisions.get(message.id)?.(message.allowed === true); decisions.delete(message.id); }
  } catch (e) { output({ type: 'error', message: String(e) }); }
});
