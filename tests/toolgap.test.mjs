// Wave-10 regression tests: git.branch/checkout/push/pull, proc.list/kill,
// fs.copy/append. Real git, real processes, real process table.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { Kernel } from '../oracle/kernel/kernel.mjs';

let root;
let remote;
let extra = [];

function gitInit(r) {
  spawnSync('git', ['init'], { cwd: r });
  spawnSync('git', ['config', 'user.email', 'test@nc-tools.local'], { cwd: r });
  spawnSync('git', ['config', 'user.name', 'nc-tools test'], { cwd: r });
}
function gitBare(dir) {
  const r = spawnSync('git', ['init', '--bare', dir], { cwd: tmpdir() });
  if (r.status !== 0) throw new Error(`bare init failed: ${r.stderr}`);
}

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), 'nctools-w10-'));
  remote = mkdtempSync(join(tmpdir(), 'nctools-remote-'));
});
afterEach(() => {
  rmSync(remote, { recursive: true, force: true });
  for (const e of extra) rmSync(e, { recursive: true, force: true });
  extra = [];
  rmSync(root, { recursive: true, force: true });
});

test('git.branch list/create + git.checkout switches branches', async () => {
  gitInit(root);
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'main.txt', content: 'main' });
  await k.call('git.add', { paths: ['main.txt'] });
  await k.call('git.commit', { message: 'base' });

  const created = await k.call('git.branch', { name: 'feature' });
  assert.equal(created.ok, true);
  assert.equal(created.result.created, true);

  const listed = await k.call('git.branch', {});
  assert.equal(listed.ok, true);
  assert.equal(listed.result.current, 'master');
  assert.ok(listed.result.branches.some((b) => b.name === 'feature' && !b.current));

  const toFeature = await k.call('git.checkout', { branch: 'feature' });
  assert.equal(toFeature.ok, true);
  await k.call('fs.write', { path: 'feat.txt', content: 'x' });
  await k.call('git.add', { paths: ['feat.txt'] });
  await k.call('git.commit', { message: 'feat' });

  const back = await k.call('git.checkout', { branch: 'master' });
  assert.equal(back.ok, true);
  assert.equal(existsSync(join(root, 'main.txt')), true);
  assert.equal(existsSync(join(root, 'feat.txt')), false);

  const makeNew = await k.call('git.checkout', { branch: 'second', create: true });
  assert.equal(makeNew.ok, true);
  assert.equal(makeNew.result.created, true);
});

test('git.push + git.pull round-trip against a local bare remote', async () => {
  gitInit(root);
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'a.txt', content: 'a' });
  await k.call('git.add', { paths: ['a.txt'] });
  await k.call('git.commit', { message: 'base' });
  gitBare(remote);
  const addRemote = spawnSync('git', ['remote', 'add', 'origin', remote], { cwd: root });
  assert.equal(addRemote.status, 0);

  const push = await k.call('git.push', { remote: 'origin', branch: 'master' });
  assert.equal(push.ok, true, JSON.stringify(push.error));
  assert.equal(push.result.branch, 'master');
  assert.equal(push.result.upstream, true);
  assert.ok(push.result.output.length >= 1);

  // a second clone makes a commit and pushes; the workspace then pulls it
  const cloneDir = join(tmpdir(), `nctools-clone-${Date.now()}`);
  extra.push(cloneDir);
  spawnSync('git', ['clone', remote, cloneDir], { cwd: tmpdir() });
  gitInit(cloneDir);
  writeFileSync(join(cloneDir, 'b.txt'), 'from-clone');
  spawnSync('git', ['add', '.'], { cwd: cloneDir });
  spawnSync('git', ['commit', '-m', 'clone commit'], { cwd: cloneDir });
  const cp = spawnSync('git', ['push', 'origin', 'master'], { cwd: cloneDir });
  assert.equal(cp.status, 0, cp.stderr);

  const pull = await k.call('git.pull', { remote: 'origin', branch: 'master' });
  assert.equal(pull.ok, true, JSON.stringify(pull.error));
  assert.equal(readFileSync(join(root, 'b.txt'), 'utf8'), 'from-clone');

  // pushing to a remote that doesn't exist is a structured ERR_GIT
  const bad = await k.call('git.push', { remote: 'no-such-remote', branch: 'master' });
  assert.equal(bad.ok, false);
  assert.equal(bad.error.code, 'ERR_GIT');
});

test('proc.list returns the OS process table with filter', async () => {
  const k = new Kernel(root);
  const out = await k.call('proc.list', {});
  assert.equal(out.ok, true, JSON.stringify(out.error));
  assert.ok(out.result.processes.length > 0);
  const first = out.result.processes[0];
  assert.ok(Number.isInteger(first.pid) && first.pid > 0);
  assert.equal(typeof first.name, 'string');
  assert.ok(out.result.total >= out.result.processes.length);

  const none = await k.call('proc.list', { filter: '__no_such_proc_xyz__' });
  assert.equal(none.result.total, 0);

  const filtered = await k.call('proc.list', { filter: 'node' });
  // the test process itself is node (or node.exe on Windows)
  assert.ok(!filtered.result.processes.some((p) => !p.name.toLowerCase().includes('node')));

  const capped = await k.call('proc.list', { maxResults: 1 });
  assert.equal(capped.result.processes.length, 1);

  const bad = await k.call('proc.list', { maxResults: 0 });
  assert.equal(bad.ok, false);
  assert.equal(bad.error.code, 'ERR_BAD_INPUT');
});

test('proc.kill kills by PID and reports ESRCH for dead pids', async () => {
  const k = new Kernel(root);
  const started = await k.call('proc.start', { cmd: 'node', args: ['-e', 'setInterval(()=>{},1000)'], maxDurationMs: 600_000 });
  assert.equal(started.ok, true);
  const pid = started.result.pid;
  assert.ok(Number.isInteger(pid) && pid > 0);

  const killed = await k.call('proc.kill', { pid });
  assert.equal(killed.ok, true, JSON.stringify(killed.error));
  assert.equal(killed.result.requested, true);

  // managed handle observes the death
  const deadline = Date.now() + 5000;
  let status;
  do {
    status = await k.call('proc.status', { handleId: started.result.handleId });
    if (!status.result.running) break;
    await new Promise((r) => setTimeout(r, 100));
  } while (Date.now() < deadline);
  assert.equal(status.result.running, false);

  // second kill of the same (now dead) pid is ESRCH → structured error
  const again = await k.call('proc.kill', { pid });
  assert.equal(again.ok, false);
  assert.equal(again.error.code, 'ERR_PROC_NOT_FOUND');

  const bad = await k.call('proc.kill', { pid: -5 });
  assert.equal(bad.error.code, 'ERR_BAD_INPUT');
});

test('fs.copy copies files and directories', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'a.txt', content: 'data' });
  const fileCopy = await k.call('fs.copy', { from: 'a.txt', to: 'b.txt' });
  assert.equal(fileCopy.ok, true, JSON.stringify(fileCopy.error));
  assert.equal(readFileSync(join(root, 'b.txt'), 'utf8'), 'data');

  mkdirSync(join(root, 'sub', 'deep'), { recursive: true });
  writeFileSync(join(root, 'sub', 'deep', 'x.txt'), 'x');
  const dirCopy = await k.call('fs.copy', { from: 'sub', to: 'sub2' });
  assert.equal(dirCopy.ok, true, JSON.stringify(dirCopy.error));
  assert.equal(readFileSync(join(root, 'sub2', 'deep', 'x.txt'), 'utf8'), 'x');

  const missing = await k.call('fs.copy', { from: 'nope.txt', to: 'x.txt' });
  assert.equal(missing.ok, false);
  assert.equal(missing.error.code, 'ERR_NOT_FOUND');

  const noRecurse = await k.call('fs.copy', { from: 'sub', to: 'x', recursive: false });
  assert.equal(noRecurse.ok, false);
  assert.equal(noRecurse.error.code, 'ERR_IS_DIRECTORY');
});

test('fs.append creates or appends', async () => {
  const k = new Kernel(root);
  const first = await k.call('fs.append', { path: 'log.txt', content: 'line1\n' });
  assert.equal(first.ok, true);
  assert.equal(first.result.created, true);
  const second = await k.call('fs.append', { path: 'log.txt', content: 'line2\n' });
  assert.equal(second.ok, true);
  assert.equal(second.result.created, false);
  assert.equal(readFileSync(join(root, 'log.txt'), 'utf8'), 'line1\nline2\n');
  await k.call('fs.append', { path: 'deep/nested/log.txt', content: 'x' });
  assert.equal(readFileSync(join(root, 'deep/nested/log.txt'), 'utf8'), 'x');
});
