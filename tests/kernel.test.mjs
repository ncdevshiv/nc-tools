// Kernel test suite: real filesystem, real git, real processes. No mocks.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, basename, parse } from 'node:path';
import { spawnSync } from 'node:child_process';
import { Kernel } from '../src/kernel/kernel.mjs';

let root;
function freshRoot() {
  root = mkdtempSync(join(tmpdir(), 'nctools-test-'));
  return root;
}
function gitInit(r) {
  spawnSync('git', ['init'], { cwd: r });
  spawnSync('git', ['config', 'user.email', 'test@nc-tools.local'], { cwd: r });
  spawnSync('git', ['config', 'user.name', 'nc-tools test'], { cwd: r });
}

beforeEach(() => { freshRoot(); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); });

test('fs.write creates file + parent dirs, reports created', async () => {
  const k = new Kernel(root);
  const out = await k.call('fs.write', { path: 'src/a/b.txt', content: 'hello\n' });
  assert.equal(out.ok, true);
  assert.equal(out.result.created, true);
  assert.equal(readFileSync(join(root, 'src/a/b.txt'), 'utf8'), 'hello\n');
});

test('fs.read returns numbered lines and pagination', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'f.txt', content: Array.from({ length: 30 }, (_, i) => `line${i + 1}`).join('\n') });
  const all = await k.call('fs.read', { path: 'f.txt' });
  assert.equal(all.ok, true);
  assert.equal(all.result.totalLines, 30);
  assert.match(all.result.content, /^\s*1\tline1/);
  const page = await k.call('fs.read', { path: 'f.txt', offset: 28, limit: 5 });
  assert.equal(page.result.truncated, false);
  assert.match(page.result.content, /line28/);
});

test('fs.list skips .nc-tools and .git', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'keep.txt', content: 'x' });
  const out = await k.call('fs.list', { path: '.', recursive: true });
  assert.equal(out.ok, true);
  assert.equal(out.result.total, 1);
  assert.equal(out.result.entries[0].path, 'keep.txt');
});

test('paths outside the base dir are allowed (global tool system)', async () => {
  const k = new Kernel(root);
  const outside = mkdtempSync(join(tmpdir(), 'nctools-out-'));
  try {
    const w = await k.call('fs.write', { path: join(outside, 'x.txt'), content: 'out' });
    assert.equal(w.ok, true, JSON.stringify(w.error || ''));
    const r = await k.call('fs.read', { path: join(outside, 'x.txt') });
    assert.equal(r.ok, true);
    assert.match(r.result.content, /out/);
    // relative climbs through the parent of the base dir are allowed too
    const climb = await k.call('fs.read', { path: `../${basename(outside)}/x.txt` });
    assert.equal(climb.ok, true, JSON.stringify(climb.error || ''));
  } finally {
    rmSync(outside, { recursive: true, force: true });
  }
});

test('patch.apply applies unique replace; reports PATCH_NO_MATCH with hints', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'app.js', content: 'function add(a, b) {\n  return a + b;\n}\nfunction sub(a, b) {\n  return a - b;\n}\n' });
  const ok = await k.call('patch.apply', {
    path: 'app.js',
    edits: [{ oldText: 'return a + b;', newText: 'return a + b + 0;' }],
  });
  assert.equal(ok.ok, true);
  assert.equal(ok.result.applied[0].replacements, 1);
  assert.match(readFileSync(join(root, 'app.js'), 'utf8'), /a \+ b \+ 0/);

  const miss = await k.call('patch.apply', { path: 'app.js', edits: [{ oldText: 'return a * b;', newText: 'x' }] });
  assert.equal(miss.ok, false);
  assert.equal(miss.error.code, 'PATCH_NO_MATCH');
  assert.ok(Array.isArray(miss.error.hint.nearestCandidateLines));
});

test('patch.apply ambiguity guard: two occurrences without expectedCount fails', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'dup.txt', content: 'same line\nother\nsame line\n' });
  const out = await k.call('patch.apply', { path: 'dup.txt', edits: [{ oldText: 'same line', newText: 'new' }] });
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'PATCH_AMBIGUOUS');
  assert.equal(out.error.hint.occurrences, 2);
  // with expectedCount=2 it succeeds
  const ok = await k.call('patch.apply', { path: 'dup.txt', edits: [{ oldText: 'same line', newText: 'new', expectedCount: 2 }] });
  assert.equal(ok.ok, true);
  assert.equal(readFileSync(join(root, 'dup.txt'), 'utf8'), 'new\nother\nnew\n');
});

test('search.grep finds matches with file/line', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'a.js', content: 'const alpha = 1;\nconst beta = 2;\n' });
  await k.call('fs.write', { path: 'nested/b.js', content: 'const gamma = alpha;\n' });
  const out = await k.call('search.grep', { pattern: 'alpha' });
  assert.equal(out.result.total, 2);
  const files = new Set(out.result.matches.map((m) => m.file));
  assert.deepEqual([...files].sort(), ['a.js', 'nested/b.js']);
  const one = out.result.matches.find((m) => m.file === 'a.js');
  assert.equal(one.line, 1);
});

test('search.files glob matches nested', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'x.test.mjs', content: '' });
  await k.call('fs.write', { path: 'sub/y.test.mjs', content: '' });
  await k.call('fs.write', { path: 'sub/z.js', content: '' });
  const out = await k.call('search.files', { pattern: '**/*.test.mjs' });
  assert.equal(out.result.total, 2);
});

test('git workflow: status, add, commit, log', async () => {
  gitInit(root);
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'hello.txt', content: 'hi' });
  const st = await k.call('git.status', {});
  assert.equal(st.ok, true);
  assert.ok(st.result.files.some((f) => f.path === 'hello.txt'));
  await k.call('git.add', { paths: ['hello.txt'] });
  const cm = await k.call('git.commit', { message: 'add hello' });
  assert.equal(cm.ok, true);
  assert.ok(cm.result.sha.length >= 7);
  const log = await k.call('git.log', {});
  assert.equal(log.result.commits[0].message, 'add hello');
  const st2 = await k.call('git.status', {});
  assert.equal(st2.result.files.length, 0);
});

test('git on non-repo returns ERR_NOT_A_REPO', async () => {
  const k = new Kernel(root);
  const out = await k.call('git.status', {});
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'ERR_NOT_A_REPO');
});

test('proc.spawn runs node with typed argv and captures output', async () => {
  const k = new Kernel(root);
  const out = await k.call('proc.spawn', { cmd: 'node', args: ['-e', 'console.log("spawned-ok")'] });
  assert.equal(out.ok, true);
  assert.equal(out.result.exitCode, 0);
  assert.match(out.result.stdout, /spawned-ok/);
  assert.equal(out.result.error, undefined);
});

test('proc.spawn captures failing exit code and stderr', async () => {
  const k = new Kernel(root);
  const out = await k.call('proc.spawn', { cmd: 'node', args: ['-e', 'console.error("bad"); process.exit(3)'] });
  assert.equal(out.ok, true); // the tool worked; the program failed
  assert.equal(out.result.exitCode, 3);
  assert.match(out.result.stderr, /bad/);
});

test('proc.spawn times out', async () => {
  const k = new Kernel(root);
  const out = await k.call('proc.spawn', { cmd: 'node', args: ['-e', 'setInterval(()=>{},1000)'], timeoutMs: 500 });
  assert.equal(out.ok, true);
  assert.equal(out.result.timedOut, true);
});

test('proc.spawn rejects shell metacharacter attempts via argv typing', async () => {
  const k = new Kernel(root);
  // no shell: "echo hi > file" is passed as a single arg to a command that doesn't exist
  const out = await k.call('proc.spawn', { cmd: 'definitely-not-a-real-cmd-xyz', args: ['echo hi > pwned.txt'] });
  assert.equal(out.result.error.code, 'ERR_CMD_NOT_FOUND');
  assert.equal(existsSync(join(root, 'pwned.txt')), false);
});

test('journal records call+result pairs and is readable via sys.journal', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'j.txt', content: 'x' });
  await k.call('fs.read', { path: 'j.txt' });
  await k.call('fs.read', { path: 'missing.txt' });
  const j = await k.call('sys.journal', {});
  assert.equal(j.ok, true);
  const events = j.result.events.filter((e) => e.tool !== 'sys.journal');
  // 3 calls + 3 results (excluding the sys.journal call itself)
  const calls = events.filter((e) => e.kind === 'tool.call');
  const results = events.filter((e) => e.kind === 'tool.result');
  assert.equal(calls.length, 3);
  assert.equal(results.length, 3);
  const failed = results.find((e) => !e.ok);
  assert.equal(failed.error.code, 'ERR_NOT_FOUND');
  // journal file exists on disk
  assert.ok(existsSync(join(root, '.nc-tools', 'journal.jsonl')));
});

test('unknown tool returns structured error with available list', async () => {
  const k = new Kernel(root);
  const out = await k.call('bash.run', { script: 'rm -rf /' });
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'ERR_UNKNOWN_TOOL');
  assert.ok(out.error.hint.available.includes('fs.write'));
});

test('fs.move and fs.delete work', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'a.txt', content: 'data' });
  await k.call('fs.move', { from: 'a.txt', to: 'dir/b.txt' });
  assert.ok(existsSync(join(root, 'dir/b.txt')));
  await k.call('fs.delete', { path: 'dir', recursive: true });
  assert.equal(existsSync(join(root, 'dir')), false);
  const missing = await k.call('fs.delete', { path: 'nope.txt' });
  assert.equal(missing.error.code, 'ERR_NOT_FOUND');
});

test('base dir and filesystem root deletion are refused', async () => {
  const k = new Kernel(root);
  const out = await k.call('fs.delete', { path: '.', recursive: true });
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'ERR_REFUSED');
  const drive = await k.call('fs.delete', { path: parse(root).root, recursive: true });
  assert.equal(drive.ok, false);
  assert.equal(drive.error.code, 'ERR_REFUSED');
});
