// sys.snapshot / sys.rollback — workspace snapshots with manifest-based
// restore. This is the primitive that makes agent *speculation* safe:
// snapshot before a risky edit, roll back if verification fails.
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync, lstatSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import { ToolError } from './errors.mjs';
import { isReparsePoint } from './paths.mjs';

const EXCLUDED = new Set(['.git', 'node_modules', '.nc-tools']);

function walkFiles(root, dir, rel = '') {
  const out = [];
  for (const name of readdirSync(dir).sort()) {
    if (EXCLUDED.has(name)) continue;
    const abs = join(dir, name);
    const relPath = rel ? `${rel}/${name}` : name;
    let st;
    try { st = lstatSync(abs); } catch { continue; }
    if (st.isSymbolicLink()) continue; // never follow links into the snapshot
    if (st.isDirectory()) {
      if (isReparsePoint(abs)) continue; // junction: skip (cycle safety)
      out.push(...walkFiles(root, abs, relPath));
    } else out.push(relPath);
  }
  return out;
}

export function makeSnapshotTools(root) {
  const snapshotsRoot = join(root, '.nc-tools', 'snapshots');

  const snapshot = ({ label = 'auto' }) => {
    const id = `${Date.now()}-${String(label).replace(/[^a-zA-Z0-9_-]/g, '_')}`;
    const dir = join(snapshotsRoot, id);
    const files = walkFiles(root, root);
    for (const relPath of files) {
      const src = join(root, relPath);
      const dst = join(dir, relPath);
      mkdirSync(join(dst, '..'), { recursive: true });
      cpSync(src, dst);
    }
    writeFileSync(join(dir, 'manifest.json'), JSON.stringify({ id, files, ts: new Date().toISOString() }));
    return { id, files: files.length, restoredOnRollback: files.length };
  };

  const list = () => {
    if (!existsSync(snapshotsRoot)) return { snapshots: [] };
    const snapshots = [];
    for (const name of readdirSync(snapshotsRoot).sort().reverse()) {
      const dir = join(snapshotsRoot, name);
      if (!statSync(dir).isDirectory()) continue;
      let manifest;
      try { manifest = JSON.parse(readFileSync(join(dir, 'manifest.json'), 'utf8')); } catch { continue; }
      snapshots.push({ id: name, files: manifest.files.length, ts: manifest.ts });
    }
    return { snapshots, total: snapshots.length };
  };

  const rollback = ({ id }) => {
    if (!id) throw new ToolError('ERR_BAD_INPUT', 'snapshot id required');
    const dir = join(snapshotsRoot, String(id));
    if (!existsSync(join(dir, 'manifest.json'))) {
      throw new ToolError('ERR_UNKNOWN_SNAPSHOT', `no snapshot ${id}`, { available: list().snapshots.map((s) => s.id) });
    }
    const manifest = JSON.parse(readFileSync(join(dir, 'manifest.json'), 'utf8'));
    // 1. restore every file in the manifest
    for (const relPath of manifest.files) {
      const src = join(dir, relPath);
      const dst = join(root, relPath);
      mkdirSync(join(dst, '..'), { recursive: true });
      cpSync(src, dst);
    }
    // 2. remove files created after the snapshot (not in manifest, not excluded)
    const manifestSet = new Set(manifest.files);
    const current = walkFiles(root, root);
    const removed = [];
    for (const relPath of current) {
      if (manifestSet.has(relPath)) continue;
      const abs = join(root, relPath);
      try { rmSync(abs, { force: true }); removed.push(relPath); } catch { /* locked on Windows; report below */ }
    }
    return { id, restored: manifest.files.length, removed: removed.length, removedFiles: removed.slice(0, 20) };
  };

  return {
    'sys.snapshot': { handler: snapshot },
    'sys.rollback': { handler: rollback },
    'sys.listSnapshots': { handler: list },
  };
}
