#!/usr/bin/env node
// MCP stdio server exposing the nc-tools kernel over JSON-RPC 2.0.
// Protocol: MCP 2024-11-05 (initialize, tools/list, tools/call).
// Usage: node src/mcp/server.mjs [workspaceRoot]   (also: npx nc-tools-mcp [workspaceRoot])
//   Falls back to NCTOOLS_WORKSPACE, then cwd.
// workspaceRoot anchors the path jail, the journal and snapshots; every
// path-taking tool refuses to touch anything outside it (ERR_PATH_ESCAPE).
// Idle auto-sleep: if no request arrives for NCTOOLS_MCP_IDLE_MS ms (default
// 30 min), the server exits(0). Set "0" or leave empty to disable the idle
// timer. MCP clients restart a stdio server on demand, so this makes dormant
// agents free the process until the next call.
import { createInterface } from 'node:readline';
import { resolve } from 'node:path';
import { Kernel } from '../kernel/kernel.mjs';
import { toolDescriptors } from '../kernel/descriptors.mjs';

const workspace = resolve(process.argv[2] || process.env.NCTOOLS_WORKSPACE || process.cwd());
const kernel = new Kernel(workspace);
const PROTOCOL_VERSION = '2024-11-05';
const SERVER_INFO = { name: 'nc-tools', version: '0.1.0' };
// "0" (or garbage) disables the idle timer: a 0ms timer would exit the server
// between requests, and a misconfigured env must never kill the process.
function idleMsFromEnv() {
  const raw = process.env.NCTOOLS_MCP_IDLE_MS;
  if (raw === undefined || raw === '') return 30 * 60 * 1000;
  const n = Number(raw);
  return Number.isFinite(n) && n > 0 ? n : null;
}
const IDLE_MS = idleMsFromEnv();

let idleTimer = null;
function touch() {
  if (IDLE_MS === null) return;
  if (idleTimer) clearTimeout(idleTimer);
  idleTimer = setTimeout(() => {
    process.stderr.write(`[nc-tools-mcp] idle ${IDLE_MS}ms — exiting; clients restart on demand\n`);
    process.exit(0);
  }, IDLE_MS);
  idleTimer.unref?.(); // don't hold the event loop open on its own
}

function rpcResult(id, result) {
  return JSON.stringify({ jsonrpc: '2.0', id, result });
}
function rpcError(id, code, message) {
  return JSON.stringify({ jsonrpc: '2.0', id, error: { code, message } });
}

async function handle(msg) {
  const { id, method, params } = msg;
  if (method === 'initialize') {
    return rpcResult(id, {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: { tools: { listChanged: false } },
      serverInfo: SERVER_INFO,
    });
  }
  if (method === 'notifications/initialized' || method?.startsWith('notifications/')) {
    return null; // notifications expect no response
  }
  if (method === 'tools/list') {
    return rpcResult(id, { tools: toolDescriptors() });
  }
  if (method === 'tools/call') {
    const name = params?.name;
    let args = params?.arguments ?? {};
    const out = await kernel.call(name, args);
    const text = JSON.stringify(out.ok ? out.result : { error: out.error }, null, 2);
    return rpcResult(id, {
      content: [{ type: 'text', text }],
      isError: !out.ok,
    });
  }
  if (method === 'ping') {
    return rpcResult(id, {});
  }
  return rpcError(id, -32601, `Method not found: ${method}`);
}

const rl = createInterface({ input: process.stdin });
rl.on('line', async (line) => {
  touch();
  const trimmed = line.trim();
  if (!trimmed) return;
  let msg;
  try { msg = JSON.parse(trimmed); } catch {
    process.stdout.write(rpcError(null, -32700, 'Parse error') + '\n');
    return;
  }
  try {
    const resp = await handle(msg);
    if (resp) process.stdout.write(resp + '\n');
  } catch (e) {
    process.stdout.write(rpcError(msg?.id ?? null, -32603, e.message) + '\n');
  }
});
rl.on('close', () => process.exit(0));
touch();

process.stderr.write(`[nc-tools-mcp] serving workspace: ${workspace}\n`);
