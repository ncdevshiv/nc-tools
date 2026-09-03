// Wave-3 workspace anchoring: MCP `roots` handshake integration test.
// A roots-capable client declares the `roots` capability in initialize; the
// server then asks IT via roots/list (server → client) and binds the first
// filesystem root as the session default base. Proves the exact incident is
// dead: a client working on TARGET with a server rooted on SERVER gets
// TARGET results from bare (baseDir-less) tool calls, while the server root
// side-channels stay put. Also proves a non-roots client is 100% unaffected.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, mkdirSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { SERVER_BIN } from './driver.mjs';

const here = dirname(fileURLToPath(import.meta.url));

/** Minimal JSON-RPC stdio duplex with a CLIENT-SIDE roots/list responder. */
class Client {
  constructor(rootArg) {
    this.child = spawn(SERVER_BIN, [rootArg], { stdio: ['pipe', 'pipe', 'pipe'] });
    this.child.stderr.on('data', () => {});
    this.pendingId = 0;
    this.buffer = '';
    this.waiters = []; // { id, resolve, reject, timer }
    this.rootsResponse = null; // set BEFORE initialize to control roots/list
    this.child.stdout.on('data', (buf) => this.#onData(buf));
  }

  #onData(buf) {
    this.buffer += buf.toString('utf8');
    let idx;
    while ((idx = this.buffer.indexOf('\n')) >= 0) {
      const line = this.buffer.slice(0, idx).trim();
      this.buffer = this.buffer.slice(idx + 1);
      if (!line) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      // Server → client request: answer roots/list per the scripted response.
      if (msg.method === 'roots/list' && msg.id !== undefined) {
        const resp = this.rootsResponse
          ? { jsonrpc: '2.0', id: msg.id, result: this.rootsResponse }
          : { jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'roots unsupported' } };
        this.child.stdin.write(JSON.stringify(resp) + '\n');
        continue;
      }
      const w = this.waiters.find((x) => x.id === msg.id);
      if (w) {
        clearTimeout(w.timer);
        this.waiters = this.waiters.filter((x) => x !== w);
        w.resolve(msg);
      }
    }
  }

  request(method, params) {
    const id = ++this.pendingId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiters = this.waiters.filter((x) => x.id !== id);
        reject(new Error(`timeout waiting for ${method}`));
      }, 15_000);
      this.waiters.push({ id, resolve, reject, timer });
      this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }

  notify(method, params) {
    this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n');
  }

  kill() { this.child.kill(); }
}

function wsDirs() {
  const serverWs = mkdtempSync(join(tmpdir(), 'nc-anchoring-server-'));
  const clientWs = mkdtempSync(join(tmpdir(), 'nc-anchoring-client-'));
  mkdirSync(join(clientWs, 'src'));
  return { serverWs, clientWs };
}

function fileUri(p) {
  const posix = p.replace(/\\/g, '/');
  return 'file:///' + posix.replace(/^\/+/, '');
}

test('roots-capable client: initialize anchors the client workspace; bare calls answer the client ws', async () => {
  const { serverWs, clientWs } = wsDirs();
  const c = new Client(serverWs);
  try {
    // Script the roots/list answer BEFORE initialize.
    c.rootsResponse = { roots: [{ uri: fileUri(clientWs), name: 'work' }] };
    const init = await c.request('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: { roots: { listChanged: true } },
      clientInfo: { name: 'roots-test-client', version: '0' },
    });
    assert.equal(init.result.serverInfo.name, 'nc-tools');
    assert.equal(init.result.anchoring.requested, true, 'roots capability seen');
    assert.equal(init.result.anchoring.anchored, true, 'anchor accepted');
    assert.ok(init.result.anchoring.base.toLowerCase().includes('nc-anchoring-client'), `anchor base: ${init.result.anchoring.base}`);

    // THE INCIDENT SCENARIO: bare sys.workspace (no baseDir) must now answer
    // the CLIENT workspace, not the server root the binary was spawned on.
    const ws = await c.request('tools/call', { name: 'sys.workspace', arguments: {} });
    const wsVal = JSON.parse(ws.result.content[0].text);
    assert.ok(wsVal.root.toLowerCase().includes('nc-anchoring-client'), `bare sys.workspace must report the client ws: ${wsVal.root}`);
    assert.ok(wsVal.root.toLowerCase().includes('server') === false, 'must NOT report the server root');
    assert.equal(wsVal.anchored, true);
    assert.ok(wsVal.serverRoot.toLowerCase().includes('nc-anchoring-server'), 'serverRoot echo stays the spawn root');
    assert.equal(wsVal.capabilities.fs, true);

    // fs.tree with NO path now walks the client workspace (which has src/).
    const tree = await c.request('tools/call', { name: 'fs.tree', arguments: {} });
    const treeVal = JSON.parse(tree.result.content[0].text);
    assert.ok(treeVal.entries.some((e) => e.path === 'src'), `tree must walk the client ws: ${treeVal.tree}`);

    // Per-call baseDir still wins over the anchor.
    const over = await c.request('tools/call', { name: 'sys.workspace', arguments: { baseDir: serverWs } });
    const overVal = JSON.parse(over.result.content[0].text);
    assert.ok(overVal.root.toLowerCase().includes('nc-anchoring-server'), 'explicit baseDir overrides the anchor');
  } finally {
    c.kill();
    rmSync(serverWs, { recursive: true, force: true });
    rmSync(clientWs, { recursive: true, force: true });
  }
});

test('non-roots client: no roots/list is sent, behavior identical to before', async () => {
  const { serverWs, clientWs } = wsDirs();
  const c = new Client(serverWs);
  try {
    c.rootsResponse = null; // would 500 if the server asked — proving it must not
    const init = await c.request('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: {},
      clientInfo: { name: 'plain-client', version: '0' },
    });
    assert.equal(init.result.anchoring.requested, false, 'no roots capability → no anchoring');
    assert.ok(init.result.anchoring.anchored === undefined);

    const ws = await c.request('tools/call', { name: 'sys.workspace', arguments: {} });
    const wsVal = JSON.parse(ws.result.content[0].text);
    assert.ok(wsVal.root.toLowerCase().includes('nc-anchoring-server'), 'unanchored: server-root default unchanged');
    assert.equal(wsVal.anchored, false);
    assert.ok(!existsSync(join(clientWs, '.nc-tools')), 'nothing written to the untouched client ws');
  } finally {
    c.kill();
    rmSync(serverWs, { recursive: true, force: true });
    rmSync(clientWs, { recursive: true, force: true });
  }
});

test('roots/list_changed re-anchors the session mid-flight', async () => {
  const { serverWs, clientWs } = wsDirs();
  const secondWs = mkdtempSync(join(tmpdir(), 'nc-anchoring-second-'));
  const c = new Client(serverWs);
  try {
    c.rootsResponse = { roots: [{ uri: fileUri(clientWs) }] };
    const init = await c.request('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: { roots: { listChanged: true } },
      clientInfo: { name: 'roots-test-client', version: '0' },
    });
    assert.equal(init.result.anchoring.anchored, true);

    // The client "moved": roots/list now returns the second workspace.
    c.rootsResponse = { roots: [{ uri: fileUri(secondWs) }] };
    c.notify('notifications/roots/list_changed');
    // The notification is async on the server side; poll until the anchor moved.
    let anchoredOk = false;
    for (let i = 0; i < 40 && !anchoredOk; i++) {
      await new Promise((r) => setTimeout(r, 100));
      const ws = await c.request('tools/call', { name: 'sys.workspace', arguments: {} });
      const wsVal = JSON.parse(ws.result.content[0].text);
      anchoredOk = wsVal.root.toLowerCase().includes('nc-anchoring-second');
    }
    assert.ok(anchoredOk, 'roots/list_changed must re-anchor to the second workspace');
  } finally {
    c.kill();
    rmSync(serverWs, { recursive: true, force: true });
    rmSync(clientWs, { recursive: true, force: true });
    rmSync(secondWs, { recursive: true, force: true });
  }
});

test('roots/list answering with a nonexistent dir: anchor refused, session stays on server root', async () => {
  const { serverWs, clientWs } = wsDirs();
  const c = new Client(serverWs);
  try {
    c.rootsResponse = { roots: [{ uri: fileUri(join(clientWs, 'missing-subdir')) }] };
    const init = await c.request('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: { roots: {} },
      clientInfo: { name: 'roots-bad-client', version: '0' },
    });
    assert.equal(init.result.anchoring.requested, true);
    assert.equal(init.result.anchoring.anchored, false, 'nonexistent root refused');
    assert.ok(init.result.anchoring.reason);

    const ws = await c.request('tools/call', { name: 'sys.workspace', arguments: {} });
    const wsVal = JSON.parse(ws.result.content[0].text);
    assert.ok(wsVal.root.toLowerCase().includes('nc-anchoring-server'), 'refused anchor → server-root default, never a silent wrong ws');
  } finally {
    c.kill();
    rmSync(serverWs, { recursive: true, force: true });
    rmSync(clientWs, { recursive: true, force: true });
  }
});
