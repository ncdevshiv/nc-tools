// Wave-2 kernel tests: managed processes, structured test drivers, package
// drivers, network tools, session env. All real — real servers, real pytest,
// real npm. No mocks.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Kernel } from './driver.mjs';

let root;
beforeEach(() => { root = mkdtempSync(join(tmpdir(), 'nc-w2-')); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); });

const SERVER = `const http = require('node:http');
const s = http.createServer((req, res) => { res.end('pong-from-server'); });
s.listen(4199, () => console.log('listening on 4199'));
`;

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

test('proc.start/status/readOutput/stop manage a real HTTP server', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'server.js', content: SERVER });
  const started = await k.call('proc.start', { cmd: 'node', args: ['server.js'] });
  assert.equal(started.ok, true);
  const { handleId } = started.result;
  assert.ok(handleId);
  assert.ok(started.result.pid > 0);

  // wait for boot, then status shows running and output captured
  let status, output;
  for (let i = 0; i < 20; i++) {
    await sleep(200);
    status = await k.call('proc.status', { handleId });
    output = await k.call('proc.readOutput', { handleId });
    if (output.result.output.includes('listening')) break;
  }
  assert.equal(status.result.running, true, JSON.stringify(status.result));
  assert.match(output.result.output, /listening on 4199/);

  // port probe sees it
  const probe = await k.call('net.probePort', { port: 4199 });
  assert.equal(probe.result.open, true);

  // HTTP against it works
  const http = await k.call('net.http', { url: 'http://127.0.0.1:4199/ping' });
  assert.equal(http.result.status, 200);
  assert.match(http.result.body, /pong-from-server/);

  // stop, then port closes
  await k.call('proc.stop', { handleId });
  let closed = false;
  for (let i = 0; i < 15; i++) {
    await sleep(200);
    const p = await k.call('net.probePort', { port: 4199 });
    if (!p.result.open) { closed = true; break; }
  }
  assert.equal(closed, true, 'port should close after proc.stop');
  const after = await k.call('proc.status', { handleId });
  assert.equal(after.result.running, false);
});

test('proc tools validate input and unknown handles', async () => {
  const k = new Kernel(root);
  const bad = await k.call('proc.start', { cmd: 'node', args: ['-c', 42] });
  assert.equal(bad.error.code, 'ERR_BAD_INPUT');
  const ghost = await k.call('proc.stop', { handleId: 'h999' });
  assert.equal(ghost.error.code, 'ERR_UNKNOWN_HANDLE');
  assert.ok(Array.isArray(ghost.error.hint.known));
});

test('test.run node: structured counts and failure identities', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'test/a.test.mjs', content: [
    "import { test } from 'node:test';",
    "import assert from 'node:assert/strict';",
    "test('passes', () => assert.equal(1, 1));",
    "test('fails loudly', () => assert.equal(1, 2));",
    "test('also passes', () => assert.ok(true));",
    '',
  ].join('\n') });
  const out = await k.call('test.run', { framework: 'node' });
  assert.equal(out.ok, true);
  assert.equal(out.result.passed, 2);
  assert.equal(out.result.failed, 1);
  assert.equal(out.result.total, 3);
  assert.equal(out.result.failures.length, 1);
  assert.equal(out.result.failures[0].name, 'fails loudly');
  assert.match(out.result.failures[0].message, /1 !== 2|Expected values to be strictly equal/);
});

test('test.run pytest: junitxml parse with real python', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'test_py/test_it.py', content: 'def test_ok():\n    assert 1 + 1 == 2\n\ndef test_bad():\n    assert 1 + 1 == 3\n' });
  const out = await k.call('test.run', { framework: 'pytest', path: 'test_py' });
  assert.equal(out.ok, true, JSON.stringify(out.error));
  assert.equal(out.result.framework, 'pytest');
  assert.equal(out.result.passed, 1);
  assert.equal(out.result.failed, 1);
  assert.equal(out.result.failures[0].name.includes('test_bad'), true);
});

test('test.run rejects unknown frameworks and empty suites', async () => {
  const k = new Kernel(root);
  const bad = await k.call('test.run', { framework: 'jest' });
  assert.equal(bad.error.code, 'ERR_BAD_INPUT');
  const empty = await k.call('test.run', { framework: 'node', path: 'nowhere/' });
  assert.equal(empty.ok, false);
  // a missing path is rejected before the runner is even spawned
  assert.equal(empty.error.code, 'ERR_NOT_FOUND');
});

test('pkg.scripts reads real package.json; pkg.runScript runs a script', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'package.json', content: JSON.stringify({
    name: 'demo', version: '1.0.0',
    scripts: { greet: 'node -e "console.log(42)"' },
  }) });
  const s = await k.call('pkg.scripts', {});
  assert.equal(s.result.scripts.greet, 'node -e "console.log(42)"');
  const r = await k.call('pkg.runScript', { name: 'greet' });
  assert.equal(r.result.ok, true);
  assert.match(r.result.stdout, /42/);
});

test('pkg.list npm reports installed state from real npm', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'package.json', content: JSON.stringify({
    name: 'demo', version: '1.0.0', dependencies: { isarray: '^2.0.5' },
  }) });
  // no node_modules → npm ls reports missing but still returns structured data
  const out = await k.call('pkg.list', { manager: 'npm' });
  assert.equal(out.ok, true);
  const isarray = out.result.packages.find((p) => p.name === 'isarray');
  assert.ok(isarray, 'isarray should be listed');
});

test('pkg.add real npm install (skips honestly if registry unreachable)', async (t) => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'package.json', content: JSON.stringify({ name: 'demo', version: '1.0.0' }) });
  const out = await k.call('pkg.add', { names: ['isarray'], timeoutMs: 60_000 });
  if (!out.ok && out.error.code === 'ERR_NETWORK') {
    t.skip(`registry unreachable from this machine: ${out.error.message}`);
    return;
  }
  assert.equal(out.ok, true, JSON.stringify(out.error || ''));
  const list = await k.call('pkg.list', { manager: 'npm' });
  const isarray = list.result.packages.find((p) => p.name === 'isarray');
  assert.ok(isarray && !isarray.missing, 'isarray should be installed after pkg.add');
});

test('env.set propagates to spawned processes; env.get resolves precedence', async () => {
  const k = new Kernel(root);
  await k.call('env.set', { name: 'NCTOOLS_TEST_VAR', value: 'hello-env' });
  const get = await k.call('env.get', { name: 'NCTOOLS_TEST_VAR' });
  assert.equal(get.result.source, 'session');
  assert.equal(get.result.value, 'hello-env');
  const spawn = await k.call('proc.spawn', { cmd: 'node', args: ['-e', 'console.log(process.env.NCTOOLS_TEST_VAR)'] });
  assert.match(spawn.result.stdout, /hello-env/);
  const badName = await k.call('env.set', { name: '1-bad', value: 'x' });
  assert.equal(badName.error.code, 'ERR_BAD_INPUT');
});

test('net.http and net.probePort validate input and report failures', async () => {
  const k = new Kernel(root);
  const badUrl = await k.call('net.http', { url: 'ftp://nope' });
  assert.equal(badUrl.error.code, 'ERR_BAD_INPUT');
  const badPort = await k.call('net.probePort', { port: 99999 });
  assert.equal(badPort.error.code, 'ERR_BAD_INPUT');
  const dead = await k.call('net.probePort', { port: 1, timeoutMs: 300 });
  assert.equal(dead.result.open, false);
});
