// Regression tests for the post-wave-8 audit fixes:
// path resolution (case-insensitivity, junction walk safety), search.files on
// single files, test.run zero-test honesty, underscore->dot name translation,
// proc bounds, git.status dot-dir filter, MCP idle-0 semantics.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname, basename } from 'node:path';
import { spawnSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { Kernel } from './driver.mjs';
import { SERVER_BIN } from './driver.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const serverPath = SERVER_BIN;
const isWin = process.platform === 'win32';

let root;
function freshRoot() {
  root = mkdtempSync(join(tmpdir(), 'nctools-fix-'));
  return root;
}
function gitInit(r) {
  spawnSync('git', ['init'], { cwd: r });
  spawnSync('git', ['config', 'user.email', 'test@nc-tools.local'], { cwd: r });
  spawnSync('git', ['config', 'user.name', 'nc-tools test'], { cwd: r });
}

beforeEach(() => { freshRoot(); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); });

test('absolute paths with different case are accepted on Windows', { skip: !isWin }, async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'f.txt', content: 'x' });
  const upper = await k.call('fs.stat', { path: `${root.toUpperCase()}\\f.txt` });
  assert.equal(upper.ok, true);
  assert.equal(upper.result.exists, true);
  const driveLower = root[0].toLowerCase() + root.slice(1);
  const lower = await k.call('fs.stat', { path: `${driveLower}\\f.txt` });
  assert.equal(lower.ok, true);
  assert.equal(lower.result.exists, true);
});

test('junction: explicit reads through it work; recursive walks never follow it', { skip: !isWin }, async () => {
  const k = new Kernel(root);
  const secret = join(tmpdir(), `nc-secret-${Date.now()}.txt`);
  writeFileSync(secret, 'OUTSIDE-SECRET', 'utf8');
  try {
    const mk = spawnSync('cmd', ['/c', 'mklink', '/J', 'escapedir', tmpdir()], { cwd: root });
    assert.equal(mk.status, 0, `mklink failed: ${mk.stderr}`);
    // no jail: an explicit path THROUGH the junction reads outside the base
    const read = await k.call('fs.read', { path: `escapedir/${basename(secret)}` });
    assert.equal(read.ok, true, JSON.stringify(read.error || ''));
    assert.match(read.result.content, /OUTSIDE-SECRET/);
    // recursive walks still skip reparse points (cycle safety, not a jail)
    const grep = await k.call('search.grep', { pattern: 'OUTSIDE-SECRET' });
    assert.equal(grep.result.total, 0);
    const list = await k.call('fs.list', { path: '.', recursive: true });
    assert.ok(list.result.entries.some((e) => e.path === 'escapedir'));
    assert.equal(list.result.entries.some((e) => e.path.startsWith('escapedir/')), false);
  } finally {
    rmSync(secret, { force: true });
  }
});

test('search.files accepts a single-file path', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'sub/b.test.mjs', content: 'x' });
  await k.call('fs.write', { path: 'sub/z.js', content: 'x' });
  const hit = await k.call('search.files', { path: 'sub/b.test.mjs', pattern: '**/*.test.mjs' });
  assert.equal(hit.ok, true);
  assert.equal(hit.result.total, 1);
  assert.equal(hit.result.files[0], 'sub/b.test.mjs');
  const miss = await k.call('search.files', { path: 'sub/z.js', pattern: '*.test.mjs' });
  assert.equal(miss.ok, true);
  assert.equal(miss.result.total, 0);
});

test('test.run fails loudly when zero tests are discovered', async () => {
  const k = new Kernel(root);
  await k.call('fs.mkdir', { path: 'emptydir' });
  const out = await k.call('test.run', { path: 'emptydir' });
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'ERR_NO_TESTS');
});

test('call and batch.execute accept MCP-style underscored tool names', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'f.txt', content: 'data' });
  const direct = await k.call('fs_stat', { path: 'f.txt' });
  assert.equal(direct.ok, true);
  const batched = await k.call('batch.execute', {
    calls: [
      { tool: 'fs_stat', args: { path: 'f.txt' } },
      { tool: 'fs_read', args: { path: 'f.txt' } },
    ],
  });
  assert.equal(batched.ok, true);
  assert.equal(batched.result.ok, 2);
  assert.equal(batched.result.failed, 0);
  // a genuinely unknown name still fails with the hint
  const unknown = await k.call('batch.execute', { calls: [{ tool: 'fs_stats', args: {} }] });
  assert.equal(unknown.ok, true); // batch itself worked
  assert.equal(unknown.result.results[0].ok, false);
  assert.equal(unknown.result.results[0].error.code, 'ERR_UNKNOWN_TOOL');
});

test('proc bounds are enforced even when the schema is bypassed', async () => {
  const k = new Kernel(root);
  const tooSmall = await k.call('proc.spawn', { cmd: 'node', args: ['--version'], timeoutMs: 50 });
  assert.equal(tooSmall.ok, false);
  assert.equal(tooSmall.error.code, 'ERR_BAD_INPUT');
  const tooBig = await k.call('proc.start', { cmd: 'node', args: ['--version'], maxDurationMs: 3_600_001 });
  assert.equal(tooBig.ok, false);
  assert.equal(tooBig.error.code, 'ERR_BAD_INPUT');
});

test('git.status hides only the .nc-tools directory', async () => {
  gitInit(root);
  const k = new Kernel(root);
  await k.call('fs.write', { path: '.nc-tools/notes.txt', content: 'x' });
  await k.call('fs.write', { path: '.nc-tools-keep.txt', content: 'x' });
  const out = await k.call('git.status', {});
  assert.equal(out.ok, true);
  const paths = out.result.files.map((f) => f.path);
  assert.ok(paths.includes('.nc-tools-keep.txt'));
  assert.ok(!paths.some((p) => p === '.nc-tools' || p.startsWith('.nc-tools/')));
});

test('search.semantic accepts paths outside the base dir; missing paths fail fast', async () => {
  const k = new Kernel(root);
  const missing = join(tmpdir(), `nctools-missing-${Date.now()}`);
  // outside the base but nonexistent: the path RESOLVES (no jail), then ERR_NOT_FOUND before the model loads
  const out = await k.call('search.semantic', { query: 'x', path: missing });
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'ERR_NOT_FOUND');
});

test('MCP server with NCTOOLS_MCP_IDLE_MS=0 stays alive across requests', async () => {
  const root0 = mkdtempSync(join(tmpdir(), 'nctools-idle-'));
  const child = spawn(serverPath, [root0], {
    stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, NCTOOLS_MCP_IDLE_MS: '0' },
  });
  child.stderr.on('data', () => {});
  let seq = 0;
  const rpc = (method, params) => new Promise((resolvePromise, rejectPromise) => {
    const id = ++seq;
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
  try {
    const first = await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '0' } });
    assert.equal(first.result.serverInfo.name, 'nc-tools');
    // wait well past any hypothetical 0ms idle timer, then make a second call
    await new Promise((r) => setTimeout(r, 1500));
    const second = await rpc('tools/list', {});
    assert.ok(second.result.tools.length >= 62);
  } finally {
    child.kill();
    rmSync(root0, { recursive: true, force: true });
  }
});
