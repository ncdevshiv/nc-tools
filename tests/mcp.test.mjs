// End-to-end test of the MCP server: spawn it as a real subprocess, speak
// JSON-RPC over stdio, verify initialize / tools/list / tools/call.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname } from 'node:path';
import { SERVER_BIN } from './driver.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const serverPath = SERVER_BIN;

let root;
let child;
let pendingId = 0;

function startServer() {
  child = spawn(serverPath, [root], { stdio: ['pipe', 'pipe', 'pipe'] });
  child.stderr.on('data', () => {}); // startup banner
}

/** Send one JSON-RPC request and collect the matching response. */
function rpc(method, params) {
  const id = ++pendingId;
  return new Promise((resolvePromise, rejectPromise) => {
    const timer = setTimeout(() => rejectPromise(new Error(`timeout waiting for ${method}`)), 15_000);
    const onData = (buf) => {
      for (const line of buf.toString('utf8').split('\n')) {
        if (!line.trim()) continue;
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        if (msg.id === id) {
          child.stdout.off('data', onData);
          clearTimeout(timer);
          resolvePromise(msg);
        }
      }
    };
    child.stdout.on('data', onData);
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
}

beforeEach(() => { root = mkdtempSync(join(tmpdir(), 'nc-mcp-')); startServer(); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); child?.kill(); });

test('MCP initialize handshake', async () => {
  const resp = await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'test', version: '0' } });
  assert.equal(resp.result.serverInfo.name, 'nc-tools');
  assert.equal(resp.result.protocolVersion, '2024-11-05');
});

test('MCP tools/list returns the full kernel tool surface', async () => {
  await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'test', version: '0' } });
  const resp = await rpc('tools/list', {});
  const names = resp.result.tools.map((t) => t.name);
  assert.equal(names.length, 60);
  assert.ok(names.includes('patch.apply'));
  assert.ok(names.includes('proc.spawn'));
  assert.ok(names.includes('sys.journal'));
  assert.ok(resp.result.tools.every((t) => t.inputSchema && t.description));
});

test('MCP tools/call writes a real file through the kernel', async () => {
  await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'test', version: '0' } });
  const resp = await rpc('tools/call', { name: 'fs.write', arguments: { path: 'via-mcp.txt', content: 'written over mcp\n' } });
  assert.equal(resp.result.isError, false);
  assert.match(resp.result.content[0].text, /"created": true/);
  assert.equal(readFileSync(join(root, 'via-mcp.txt'), 'utf8'), 'written over mcp\n');
});

test('MCP tools/call reports errors with isError', async () => {
  await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'test', version: '0' } });
  const resp = await rpc('tools/call', { name: 'fs.read', arguments: { path: 'ghost.txt' } });
  assert.equal(resp.result.isError, true);
  assert.match(resp.result.content[0].text, /ERR_NOT_FOUND/);
});
