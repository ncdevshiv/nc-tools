// Reference terminal-free agent loop. Model gives tool calls; kernel executes.
// No shell, no terminal. Works with any OpenAI-compatible chat endpoint.
import { toolDescriptors } from '../kernel/descriptors.mjs';

export function systemPrompt(workspaceRoot) {
  return `You are a coding agent operating a machine through a typed tool API (nc-tools).
There is NO shell and NO terminal. Do not attempt to run bash/sh commands except via proc.spawn
for real programs (test runners, compilers, git is already provided as tools).

Workspace root: ${workspaceRoot}

Tool discipline:
- BATCH independent work: fs.readMany / fs.writeMany / patch.applyMany / batch.execute exist
  so you do not pay one round-trip per tiny operation. Use them.
- LONG-RUNNING programs (servers, watchers) use proc.start and give you a handleId —
  then proc.status / proc.readOutput / proc.stop. One-shot programs use proc.spawn.
- test.run returns structured pass/fail counts and failing test identities (node, pytest).
- pkg.* installs/lists packages and runs npm scripts; net.http makes HTTP requests;
  net.probePort checks TCP ports; env.set/get manage the session environment.
- Prefer patch.apply for edits: exact-match search/replace. Include enough context to be unique.
- fs.read returns a digest (hash+mtime). If the digest matches what you already saw, the file
  is unchanged — do not re-read it.
- Use search.grep / search.files to locate code. search.semantic ranks files by MEANING
  (local neural embeddings) — use it when you only know what the code does, not what strings
  it contains. Structured errors carry actionable hints
  (e.g. PATCH_NO_MATCH returns nearest candidate lines, ERR_NOT_FOUND returns nearest existing
  files) — use them instead of re-orienting with more calls.
- proc.spawn expects cmd + args array, never a shell string.
- When the task is done, verify it (read the file back, run tests via proc.spawn), then reply with
  a final summary message with NO tool calls.

Reply format: think briefly, call tools as needed, then finish with a short plaintext summary.`;
}

/** Convert kernel tools to OpenAI tool schema format. */
// Some providers require ^[a-zA-Z0-9_-]+$ for function names; kernel tools use
// "fs.read" style. Map dotted names to underscored for the wire, and back on return.
const WIRE = /^[a-zA-Z0-9_-]+$/;
const toWire = (name) => WIRE.test(name) ? name : name.replaceAll('.', '__');
const fromWire = (name) => name.includes('__') ? name.replaceAll('__', '.') : name;
export { toWire, fromWire };

// ---- bash comparison arm ------------------------------------------------
// The same agent loop, but with a single `bash` tool instead of the kernel.
// Used by benchmark/compare.mjs to measure typed-tools-vs-shell on identical
// tasks with the same models and the same verifier.
export function bashSystemPrompt(workspaceRoot) {
  return `You are a coding agent operating a machine through a single bash tool.
Every machine interaction — reading, writing, searching, editing files, running programs, git —
must go through the bash tool with a shell script string. It returns stdout, stderr, and the
exit code.

Workspace root: ${workspaceRoot}

When the task is done, verify it (read the file back, run the tests), then reply with a final
summary message with NO tool calls.`;
}

export function bashToolDescriptor() {
  return {
    type: 'function',
    function: {
      name: 'bash',
      description: 'Run a bash shell script in the workspace root. Returns stdout, stderr, exit code.',
      parameters: {
        type: 'object',
        properties: {
          script: { type: 'string', description: 'The bash script to execute' },
          timeoutMs: { type: 'integer', minimum: 100, maximum: 600000 },
        },
        required: ['script'],
        additionalProperties: false,
      },
    },
  };
}

export function openAiTools() {
  return toolDescriptors().map((t) => ({
    type: 'function',
    function: { name: toWire(t.name), description: t.description, parameters: t.inputSchema },
  }));
}

/**
 * Run the agent loop until final answer, maxSteps cap, or API failure.
 * @param {object} deps { chat(completionsBody) => responseJSON, kernel, log? }
 * @returns {Promise<{finalText, steps, toolCalls, errors, usage, stopped: 'done'|'max_steps'|'api_error'}>}
 */
export async function runAgent({ chat, kernel, task, maxSteps = 30, log = () => {}, mode = 'kernel' }) {
  const isBash = mode === 'bash';
  const messages = [
    { role: 'system', content: isBash ? bashSystemPrompt(kernel.root) : systemPrompt(kernel.root) },
    { role: 'user', content: task },
  ];
  const tools = isBash ? [bashToolDescriptor()] : openAiTools();
  let toolCalls = 0;
  let errors = 0;
  const usage = { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 };
  let stopped = 'max_steps';
  let finalText = null;

  for (let step = 0; step < maxSteps; step++) {
    let resp;
    try {
      resp = await chat({ model: undefined, messages, tools, tool_choice: 'auto' });
    } catch (e) {
      stopped = 'api_error';
      finalText = `API error: ${e.message}`;
      break;
    }
    const u = resp.usage;
    if (u) { usage.prompt_tokens += u.prompt_tokens || 0; usage.completion_tokens += u.completion_tokens || 0; usage.total_tokens += u.total_tokens || 0; }

    const choice = resp.choices?.[0];
    const msg = choice?.message;
    if (!msg) { stopped = 'api_error'; finalText = 'Malformed API response'; break; }

    const assistantMsg = { role: 'assistant', content: msg.content ?? '' };
    if (msg.tool_calls?.length) assistantMsg.tool_calls = msg.tool_calls;
    messages.push(assistantMsg);

    if (!msg.tool_calls?.length) {
      stopped = choice.finish_reason === 'stop' ? 'done' : 'max_steps';
      finalText = msg.content ?? '';
      break;
    }

    for (const tc of msg.tool_calls) {
      let args = {};
      try { args = JSON.parse(tc.function.arguments || '{}'); } catch { args = {}; }
      const toolName = fromWire(tc.function.name);
      log(`  step ${step + 1}: ${toolName} ${JSON.stringify(args).slice(0, 120)}`);
      const out = isBash ? await kernel.call('proc.spawn', { cmd: 'bash', args: ['-c', args.script ?? ''], timeoutMs: args.timeoutMs ?? 120_000 })
                         : await kernel.call(toolName, args);
      if (!out.ok) errors += 1;
      else if (isBash && (out.result.exitCode !== 0 || out.result.error)) errors += 1;
      toolCalls += 1;
      messages.push({
        role: 'tool',
        tool_call_id: tc.id,
        content: JSON.stringify(out.ok ? out.result : { error: out.error }).slice(0, 20_000),
      });
    }
  }

  if (stopped === 'max_steps' && finalText === null) {
    finalText = 'Stopped at max steps without a final answer.';
  }
  return { finalText, steps: messages.length, toolCalls, errors, usage, stopped };
}
