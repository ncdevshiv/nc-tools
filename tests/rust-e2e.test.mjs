// Rust end-to-end tests: drive target/release/nc-tools-mcp over MCP stdio
// via the driver harness. Covers the 10 Phase-2 tools plus the behavioral
// contracts the phase depends on (case-insensitive paths, single-file search,
// proc bounds, git.status dot-dir filtering, underscored aliases).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { withKernel, SERVER_BIN } from './driver.mjs';

const tmp = () => mkdtempSync(join(tmpdir(), 'nct-rust-e2e-'));
const isWin = process.platform === 'win32';

function gitInit(r) {
  spawnSync('git', ['init'], { cwd: r });
  spawnSync('git', ['config', 'user.email', 'test@nc-tools.local'], { cwd: r });
  spawnSync('git', ['config', 'user.name', 'nc-tools test'], { cwd: r });
}

test('binary exists and is a real MCP server', () => {
  assert.ok(existsSync(SERVER_BIN), `no server binary at ${SERVER_BIN}`);
});

test('initialize handshake + tools/list is the full 62-tool surface', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      const tools = await k.listTools();
      assert.equal(tools.length, 62);
      assert.ok(tools.every((t) => t.inputSchema && t.inputSchema.type === 'object' && t.description));
      for (const n of ['fs.readRange','fs.tree','search.replace','code.symbols','text.diff','proc.runScript','proc.watch','git.blame','sys.doctor','test.run']) {
        assert.ok(tools.some((t) => t.name === n), `missing tool: ${n}`);
      }
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('fs.write/read round-trip and ERR_NOT_FOUND hint', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      const w = await k.call('fs.write', { path: 'a.txt', content: 'hello rust\n' });
      assert.equal(w.ok, true);
      const r = await k.call('fs.read', { path: 'a.txt' });
      assert.equal(r.ok, true);
      assert.ok(r.result.content.includes('hello rust'));
      const miss = await k.call('fs.read', { path: 'ghost.txt' });
      assert.equal(miss.ok, false);
      assert.equal(miss.error.code, 'ERR_NOT_FOUND');
      assert.ok(Array.isArray(miss.error.hint?.nearestExisting));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('underscored wire aliases resolve (fs_stat, sys__workspace)', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'alias.txt', content: 'alias body' });
      const s = await k.call('fs_stat', { path: 'alias.txt' });
      assert.equal(s.ok, true);
      assert.equal(s.result.size, 10);
      const w = await k.call('sys__workspace', {});
      assert.equal(w.ok, true);
      assert.ok(w.result.root.length > 0);
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('patch.apply exact match + PATCH_NO_MATCH hints', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'app.js', content: 'function add(a, b) {\n  return a + b;\n}\n' });
      const p1 = await k.call('patch.apply', { path: 'app.js', edits: [{ oldText: 'return a + b;', newText: 'return a + b + 0;' }] });
      assert.equal(p1.ok, true);
      const p2 = await k.call('patch.apply', { path: 'app.js', edits: [{ oldText: 'return a * b;', newText: 'x' }] });
      assert.equal(p2.ok, false);
      assert.equal(p2.error.code, 'PATCH_NO_MATCH');
      assert.ok(Array.isArray(p2.error.hint?.nearestCandidateLines));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});
test('fs.readRange returns a windowed byte slice', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'rr.txt', content: '0123456789\n' });
      const r = await k.call('fs.readRange', { path: 'rr.txt', byteOffset: 2, maxBytes: 4 });
      assert.equal(r.ok, true);
      assert.equal(r.result.content, '2345');
      assert.equal(r.result.byteOffset, 2);
      assert.equal(r.result.byteLength, 4);
      assert.equal(r.result.eof, false);
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('fs.tree returns structured entries with a path', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.mkdir', { path: 'td/sub' });
      await k.call('fs.write', { path: 'td/a.txt', content: 'x' });
      const r = await k.call('fs.tree', { path: 'td' });
      assert.equal(r.ok, true);
      assert.ok(Array.isArray(r.result.entries));
      assert.ok(r.result.entries.some((e) => e.path === 'a.txt'));
      assert.equal(r.result.entries.some((e) => e.path === 'sub'), true, 'dirs-first includes the subdir');
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('code.symbols extracts named symbols from TS source', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'c.ts', content: 'class Foo {}\nfunction bar() {}\n' });
      const r = await k.call('code.symbols', { path: 'c.ts' });
      assert.equal(r.ok, true);
      assert.ok(r.result.symbols.some((s) => s.name === 'Foo'));
      assert.ok(r.result.symbols.some((s) => s.name === 'bar'));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('text.diff emits a unified hunk with a changed line', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'd.old.txt', content: 'a\nb\nc\n' });
      await k.call('fs.write', { path: 'd.new.txt', content: 'a\nCHANGED\nc\n' });
      const r = await k.call('text.diff', { path: 'd.old.txt', path2: 'd.new.txt' });
      assert.equal(r.ok, true);
      assert.equal(r.result.equal, false);
      assert.ok(r.result.diff.includes('@@'));
      assert.ok(/-b/.test(r.result.diff));
      assert.ok(/\+CHANGED/.test(r.result.diff));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('search.replace dry-run reports matches without writing', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 's.js', content: 'const needle = 1;\nconst needle = 2;\n' });
      const r = await k.call('search.replace', { pattern: 'needle', replacement: 'pin', path: 's.js' });
      assert.equal(r.ok, true);
      assert.equal(r.result.dryRun, true);
      assert.equal(r.result.totalMatches, 2);
      assert.equal(r.result.files[0].changed, true);
      const read = await k.call('fs.read', { path: 's.js' });
      assert.ok(read.result.content.includes('needle'));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('proc.runScript executes inline JS and returns exitCode/stdout', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      const r = await k.call('proc.runScript', { language: 'js', source: 'console.log("runscript-ok")' });
      assert.equal(r.ok, true);
      assert.equal(r.result.exitCode, 0);
      assert.ok(r.result.stdout.includes('runscript-ok'));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('git.blame annotates every line of a committed file', async () => {
  const root = tmp();
  try {
    gitInit(root);
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'blame.txt', content: 'one\ntwo\n' });
      const add = await k.call('git.add', { paths: ['blame.txt'] });
      assert.equal(add.ok, true);
      const cm = await k.call('git.commit', { message: 'blame commit' });
      assert.equal(cm.ok, true);
      assert.ok(typeof cm.result.sha === 'string');
      const b = await k.call('git.blame', { path: 'blame.txt' });
      assert.equal(b.ok, true);
      assert.equal(b.result.lines.length, 2);
      assert.ok(b.result.lines[0].commit);
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('sys.doctor reports the 62-tool inventory', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      const r = await k.call('sys.doctor', {});
      assert.equal(r.ok, true);
      assert.equal(r.result.tools.count, 62);
      assert.ok(r.result.tools.names.includes('text.diff'));
      assert.ok(typeof r.result.limits.spawnTimeoutMs === 'number');
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('search.files accepts a single-file path', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'sub/b.test.mjs', content: 'x' });
      await k.call('fs.write', { path: 'sub/z.js', content: 'x' });
      const hit = await k.call('search.files', { path: 'sub/b.test.mjs', pattern: '**/*.test.mjs' });
      assert.equal(hit.ok, true);
      assert.equal(hit.result.total, 1);
      assert.equal(hit.result.files[0], 'sub/b.test.mjs');
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('proc bounds are enforced even when the schema is bypassed', async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      const tooSmall = await k.call('proc.spawn', { cmd: 'node', args: ['--version'], timeoutMs: 50 });
      assert.equal(tooSmall.ok, false);
      assert.equal(tooSmall.error.code, 'ERR_BAD_INPUT');
      const tooBig = await k.call('proc.start', { cmd: 'node', args: ['--version'], maxDurationMs: 3_600_001 });
      assert.equal(tooBig.ok, false);
      assert.equal(tooBig.error.code, 'ERR_BAD_INPUT');
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('git.status hides only the .nc-tools directory', async () => {
  const root = tmp();
  try {
    gitInit(root);
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: '.nc-tools/notes.txt', content: 'x' });
      await k.call('fs.write', { path: '.nc-tools-keep.txt', content: 'x' });
      const out = await k.call('git.status', {});
      assert.equal(out.ok, true);
      const paths = out.result.files.map((f) => f.path);
      assert.ok(paths.includes('.nc-tools-keep.txt'));
      assert.ok(!paths.some((p) => p === '.nc-tools' || p.startsWith('.nc-tools/')));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('absolute paths with different case are accepted on Windows', { skip: !isWin }, async () => {
  const root = tmp();
  try {
    await withKernel(root, async (k) => {
      await k.call('fs.write', { path: 'f.txt', content: 'x' });
      const upper = await k.call('fs.stat', { path: `${root.toUpperCase()}\\f.txt` });
      assert.equal(upper.ok, true);
      assert.equal(upper.result.exists, true);
      const driveLower = root[0].toLowerCase() + root.slice(1);
      const lower = await k.call('fs.stat', { path: `${driveLower}\\f.txt` });
      assert.equal(lower.ok, true);
      assert.equal(lower.result.exists, true);
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

