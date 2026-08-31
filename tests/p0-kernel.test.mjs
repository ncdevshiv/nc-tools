// Tests for the P0 kernel upgrades: batch ops, error hint enrichment, digests.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Kernel } from '../src/kernel/kernel.mjs';

let root;
beforeEach(() => { root = mkdtempSync(join(tmpdir(), 'nc-p0-')); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); });

test('fs.readMany reads several files in one call, with per-item errors', async () => {
  const k = new Kernel(root);
  await k.call('fs.writeMany', { files: [
    { path: 'a.txt', content: 'alpha' },
    { path: 'sub/b.txt', content: 'beta' },
  ] });
  const out = await k.call('fs.readMany', { paths: ['a.txt', 'sub/b.txt', 'ghost.txt'] });
  assert.equal(out.ok, true);
  assert.equal(out.result.files.length, 3);
  assert.equal(out.result.files[0].ok, true);
  assert.equal(out.result.files[0].content.includes('alpha'), true);
  assert.equal(out.result.files[1].ok, true);
  assert.equal(out.result.files[2].ok, false);
  assert.equal(out.result.files[2].error.code, 'ERR_NOT_FOUND');
});

test('fs.writeMany writes many files; per-item failure does not abort the batch', async () => {
  const k = new Kernel(root);
  const out = await k.call('fs.writeMany', { files: [
    { path: 'ok1.txt', content: '1' },
    { path: '', content: 'bad' },
    { path: 'deep/nested/ok2.txt', content: '2' },
  ] });
  assert.equal(out.ok, true);
  assert.equal(out.result.written, 2);
  assert.equal(out.result.failed, 1);
  assert.equal(out.result.results[1].ok, false);
  assert.equal(out.result.results[1].error.code, 'ERR_BAD_PATH');
  assert.equal(readFileSync(join(root, 'ok1.txt'), 'utf8'), '1');
  assert.equal(readFileSync(join(root, 'deep/nested/ok2.txt'), 'utf8'), '2');
});

test('patch.applyMany edits multiple files in one call', async () => {
  const k = new Kernel(root);
  await k.call('fs.writeMany', { files: [
    { path: 'x.js', content: 'const one = 1;\n' },
    { path: 'y.js', content: 'const two = 2;\n' },
  ] });
  const out = await k.call('patch.applyMany', { edits: [
    { path: 'x.js', edits: [{ oldText: 'const one = 1;', newText: 'const one = 11;' }] },
    { path: 'y.js', edits: [{ oldText: 'nope-not-there', newText: 'z' }] },
  ] });
  assert.equal(out.ok, true);
  assert.equal(out.result.patched, 1);
  assert.equal(out.result.failed, 1);
  assert.equal(out.result.results[0].ok, true);
  assert.equal(out.result.results[1].ok, false);
  assert.equal(out.result.results[1].error.code, 'PATCH_NO_MATCH');
  assert.match(readFileSync(join(root, 'x.js'), 'utf8'), /const one = 11;/);
});

test('batch.execute runs mixed calls in one round-trip, journaled individually', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'b.txt', content: 'data' });
  const out = await k.call('batch.execute', { calls: [
    { tool: 'fs.read', args: { path: 'b.txt' } },
    { tool: 'search.grep', args: { pattern: 'data' } },
    { tool: 'fs.read', args: { path: 'missing.txt' } },
    { tool: 'fs.stat', args: { path: '.' } },
  ] });
  assert.equal(out.ok, true);
  assert.equal(out.result.ok, 3);
  assert.equal(out.result.failed, 1);
  assert.equal(out.result.results[2].ok, false);
  // every sub-call must appear in the journal as its own call+result pair
  const j = await k.call('sys.journal', {});
  const calls = j.result.events.filter((e) => e.kind === 'tool.call' && e.tool !== 'sys.journal');
  const tools = calls.map((e) => e.tool);
  assert.ok(tools.includes('fs.read'));
  assert.ok(tools.includes('search.grep'));
  assert.ok(tools.includes('fs.stat'));
  assert.ok(tools.includes('batch.execute'));
});

test('batch.execute refuses nesting and enforces size cap', async () => {
  const k = new Kernel(root);
  const nested = await k.call('batch.execute', { calls: [{ tool: 'batch.execute', args: { calls: [] } }] });
  assert.equal(nested.result.results[0].ok, false);
  assert.equal(nested.result.results[0].error.code, 'ERR_REFUSED');
  const tooBig = await k.call('batch.execute', { calls: Array.from({ length: 26 }, (_, i) => ({ tool: 'sys.workspace' })) });
  assert.equal(tooBig.ok, false);
  assert.equal(tooBig.error.code, 'ERR_BAD_INPUT');
});

test('fs.read returns digest; unchanged file has same digest', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'd.txt', content: 'stable content' });
  const r1 = await k.call('fs.read', { path: 'd.txt' });
  const r2 = await k.call('fs.read', { path: 'd.txt' });
  assert.ok(r1.result.digest);
  assert.ok(r1.result.digest.sha256_16.length === 16);
  assert.deepEqual(r1.result.digest, r2.result.digest);
  await k.call('fs.write', { path: 'd.txt', content: 'changed content' });
  const r3 = await k.call('fs.read', { path: 'd.txt' });
  assert.notEqual(r1.result.digest.sha256_16, r3.result.digest.sha256_16);
});

test('ERR_NOT_FOUND carries nearestExisting hints', async () => {
  const k = new Kernel(root);
  mkdirSync(join(root, 'src'), { recursive: true });
  writeFileSync(join(root, 'src', 'config.js'), 'x');
  const out = await k.call('fs.read', { path: 'src/confg.js' }); // typo
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'ERR_NOT_FOUND');
  assert.ok(out.error.hint.nearestExisting.some((p) => p.replaceAll('\\\\', '/').includes('config.js')),
    `hint should include src/config.js, got ${JSON.stringify(out.error.hint)}`);
});

test('writes to absolute paths outside the base dir succeed', async () => {
  const k = new Kernel(root);
  const p = join(tmpdir(), `nc-p0-out-${Date.now()}.txt`);
  try {
    const out = await k.call('fs.write', { path: p, content: 'x' });
    assert.equal(out.ok, true, JSON.stringify(out.error || ''));
    assert.equal(readFileSync(p, 'utf8'), 'x');
  } finally {
    rmSync(p, { force: true });
  }
});

test('search.grep accepts a FILE path (regression: ENOTDIR bug)', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'docs/spec.md', content: 'the magic token appears here\n' });
  const out = await k.call('search.grep', { pattern: 'magic token', path: 'docs/spec.md' });
  assert.equal(out.ok, true, JSON.stringify(out.error || ''));
  assert.equal(out.result.total, 1);
  assert.equal(out.result.matches[0].file, 'docs/spec.md');
  assert.equal(out.result.matches[0].line, 1);
});

test('search.grep missing file path returns ERR_NOT_FOUND with sibling hints', async () => {
  const k = new Kernel(root);
  await k.call('fs.write', { path: 'docs/a.md', content: 'x' });
  const out = await k.call('search.grep', { pattern: 'x', path: 'docs/zzz.md' });
  assert.equal(out.ok, false);
  assert.equal(out.error.code, 'ERR_NOT_FOUND');
  assert.ok(out.error.hint.nearestExisting.includes('docs/a.md'));
});
