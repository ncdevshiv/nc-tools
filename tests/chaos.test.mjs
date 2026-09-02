// Wave-3 tests: snapshot/rollback round-trips. (The chaos fault-injection
// probes were JS-oracle-era: they exercised the in-process hook API of the
// archived JS kernel, which the Rust-only kernel does not expose. The chaos
// experiment harness is archived in git history.)
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Kernel } from './driver.mjs';

let root;
beforeEach(() => { root = mkdtempSync(join(tmpdir(), 'nc-w3-')); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); });

function write(rel, content) {
  mkdirSync(join(root, rel, '..'), { recursive: true });
  writeFileSync(join(root, rel), content, 'utf8');
}

test('snapshot → mutate (edit+create+delete) → rollback restores exact state', async () => {
  const k = new Kernel(root);
  write('a.txt', 'alpha');
  write('sub/b.txt', 'beta');
  const snap = await k.call('sys.snapshot', { label: 'pre-edit' });
  assert.equal(snap.ok, true);
  assert.equal(snap.result.files, 2);

  // mutate: edit a.txt, create c.txt, delete sub/b.txt
  write('a.txt', 'ALPHA-CHANGED');
  write('c.txt', 'created after snapshot');
  import('node:fs').then(() => {});
  await k.call('fs.delete', { path: 'sub/b.txt' });
  assert.equal(existsSync(join(root, 'sub/b.txt')), false);

  const roll = await k.call('sys.rollback', { id: snap.result.id });
  assert.equal(roll.ok, true, JSON.stringify(roll.error || ''));
  assert.equal(roll.result.restored, 2);
  assert.equal(roll.result.removed, 1); // c.txt removed
  assert.equal(readFileSync(join(root, 'a.txt'), 'utf8'), 'alpha');
  assert.equal(readFileSync(join(root, 'sub/b.txt'), 'utf8'), 'beta');
  assert.equal(existsSync(join(root, 'c.txt')), false);
});

test('sys.listSnapshots shows taken snapshots', async () => {
  const k = new Kernel(root);
  write('x.txt', '1');
  await k.call('sys.snapshot', { label: 's1' });
  const l = await k.call('sys.listSnapshots', {});
  assert.equal(l.result.total, 1);
  assert.equal(l.result.snapshots.length, 1);
  assert.equal(l.result.snapshots[0].files, 1);
});

test('rollback of unknown snapshot returns structured error with available list', async () => {
  const k = new Kernel(root);
  write('x.txt', '1');
  await k.call('sys.snapshot', { label: 's1' });
  const bad = await k.call('sys.rollback', { id: 'nonexistent' });
  assert.equal(bad.error.code, 'ERR_UNKNOWN_SNAPSHOT');
  assert.ok(bad.error.hint.available.length >= 1);
});

