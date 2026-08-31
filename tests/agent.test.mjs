// Integration test: agent loop driven by a scripted in-process chat function
// (deterministic, no network) verifying the loop mechanics end to end.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Kernel } from '../oracle/kernel/kernel.mjs';
import { runAgent, fromWire, toWire } from '../oracle/agent/agent.mjs';

let root;
beforeEach(() => { root = mkdtempSync(join(tmpdir(), 'nc-agent-')); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); });

test('name mapping round-trips dotted tool names', () => {
  assert.equal(toWire('fs.read'), 'fs__read');
  assert.equal(fromWire('fs__read'), 'fs.read');
  assert.equal(toWire('proc.spawn'), 'proc__spawn');
  assert.equal(fromWire('plainname'), 'plainname');
});

test('agent loop executes tool calls and returns final text', async () => {
  const kernel = new Kernel(root);
  let phase = 0;
  const scriptedChat = async (body) => {
    phase += 1;
    if (phase === 1) {
      return {
        choices: [{ finish_reason: 'tool-calls', message: {
          role: 'assistant', content: null,
          tool_calls: [{ id: 'call_1', type: 'function', function: { name: 'fs__write', arguments: JSON.stringify({ path: 'out.txt', content: 'from agent\n' }) } }],
        } }],
        usage: { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 },
      };
    }
    return {
      choices: [{ finish_reason: 'stop', message: { role: 'assistant', content: 'done writing file' } }],
      usage: { prompt_tokens: 150, completion_tokens: 10, total_tokens: 160 },
    };
  };
  const r = await runAgent({ chat: scriptedChat, kernel, task: 'write out.txt', maxSteps: 5 });
  assert.equal(r.stopped, 'done');
  assert.equal(r.toolCalls, 1);
  assert.equal(r.finalText, 'done writing file');
  assert.equal(r.usage.total_tokens, 280);
  const check = await kernel.call('fs.read', { path: 'out.txt' });
  assert.match(check.result.content, /from agent/);
});

test('agent records tool errors and continues', async () => {
  const kernel = new Kernel(root);
  let phase = 0;
  const scriptedChat = async () => {
    phase += 1;
    if (phase === 1) {
      return { choices: [{ finish_reason: 'tool-calls', message: { role: 'assistant', content: null,
        tool_calls: [{ id: 'c1', type: 'function', function: { name: 'fs__read', arguments: JSON.stringify({ path: 'nope.txt' }) } }] } }] };
    }
    return { choices: [{ finish_reason: 'stop', message: { role: 'assistant', content: 'file was missing; acknowledged' } }] };
  };
  const r = await runAgent({ chat: scriptedChat, kernel, task: 'read nope.txt', maxSteps: 5 });
  assert.equal(r.stopped, 'done');
  assert.equal(r.errors, 1);
  assert.equal(r.toolCalls, 1);
});

test('agent stops at maxSteps without final answer', async () => {
  const kernel = new Kernel(root);
  let n = 0;
  const endlessChat = async () => {
    n += 1;
    return { choices: [{ finish_reason: 'tool-calls', message: { role: 'assistant', content: null,
      tool_calls: [{ id: `c${n}`, type: 'function', function: { name: 'sys__workspace', arguments: '{}' } }] } }] };
  };
  const r = await runAgent({ chat: endlessChat, kernel, task: 'loop forever', maxSteps: 3 });
  assert.equal(r.stopped, 'max_steps');
  assert.equal(r.toolCalls, 3);
});

test('agent surfaces API errors as stopped=api_error', async () => {
  const kernel = new Kernel(root);
  const failingChat = async () => { throw new Error('HTTP 500: upstream dead'); };
  const r = await runAgent({ chat: failingChat, kernel, task: 'anything', maxSteps: 3 });
  assert.equal(r.stopped, 'api_error');
  assert.match(r.finalText, /HTTP 500/);
});
