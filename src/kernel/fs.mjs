// fs.* tools — typed file operations, machine-wide (no workspace jail).
import { readFileSync, writeFileSync, readdirSync, statSync, mkdirSync, rmSync, renameSync, existsSync, cpSync } from 'node:fs';
import { join, relative, dirname, basename, resolve, isAbsolute, parse } from 'node:path';
import { createHash } from 'node:crypto';
import { ToolError } from './errors.mjs';
import { resolvePath, isReparsePoint } from './paths.mjs';

function digestOf(abs) {
  const st = statSync(abs);
  const h = createHash('sha256').update(readFileSync(abs)).digest('hex').slice(0, 16);
  return { sha256_16: h, mtimeMs: Math.round(st.mtimeMs) };
}

/** Nearest existing siblings/files for NOT_FOUND hints: same-name files elsewhere + dir siblings. */
export function nearestSiblings(root, absMissing) {
  const hints = new Set();
  const dir = dirname(absMissing);
  const base = basename(absMissing);
  // siblings of the missing file's directory
  try {
    for (const n of readdirSync(dir).sort()) {
      if (hints.size >= 5) break;
      if (n !== base && !n.startsWith('.')) hints.add(relative(root, join(dir, n)).replaceAll('\\', '/'));
    }
  } catch { /* dir may not exist */ }
  // same basename anywhere nearby (walk up a few levels)
  let probe = dir;
  for (let up = 0; up < 3 && hints.size < 8; up++) {
    try {
      for (const n of readdirSync(probe).sort()) {
        if (n === base) { hints.add(relative(root, join(probe, n)).replaceAll('\\', '/')); break; }
      }
      const parent = dirname(probe);
      if (parent === probe) break;
      probe = parent;
    } catch { break; }
  }
  return [...hints].slice(0, 8);
}

export function makeFsTools(root) {
  const read = ({ path, offset, limit }) => {
    const abs = resolvePath(root, path);
    if (!existsSync(abs)) throw new ToolError('ERR_NOT_FOUND', `No such file: ${path}`, { path, nearestExisting: nearestSiblings(root, abs) });
    const st = statSync(abs);
    if (st.isDirectory()) throw new ToolError('ERR_IS_DIRECTORY', `${path} is a directory; use fs.list`, { path });
    const raw = readFileSync(abs, 'utf8');
    const lines = raw.split('\n');
    const totalLines = lines.length;
    const off = Math.max(0, (offset ?? 1) - 1);
    const lim = limit ?? 2000;
    const slice = lines.slice(off, off + lim);
    const start = off + 1;
    const numbered = slice.map((l, i) => `${String(start + i).padStart(6)}\t${l}`).join('\n');
    return {
      content: numbered,
      totalLines,
      truncated: off + slice.length < totalLines,
      nextOffset: off + slice.length < totalLines ? start + slice.length : null,
      digest: digestOf(abs),
    };
  };

  /** Read many files in one call — the batching answer to the measured call tax. */
  const readMany = ({ paths, limit }) => {
    if (!Array.isArray(paths) || paths.length === 0) {
      throw new ToolError('ERR_BAD_INPUT', 'paths must be a non-empty array');
    }
    if (paths.length > 50) throw new ToolError('ERR_BAD_INPUT', 'max 50 paths per fs.readMany call');
    const files = [];
    for (const p of paths) {
      try {
        const r = read({ path: p, limit });
        files.push({ path: p, ok: true, ...r });
      } catch (e) {
        files.push({ path: p, ok: false, error: e instanceof ToolError ? e.toJSON() : { code: 'ERR_INTERNAL', message: e.message } });
      }
    }
    return { files, total: files.length };
  };

  /** Write many files in one call. Each write is still validated + journaled individually. */
  const writeMany = ({ files }) => {
    if (!Array.isArray(files) || files.length === 0) {
      throw new ToolError('ERR_BAD_INPUT', 'files must be a non-empty array of {path, content}');
    }
    if (files.length > 50) throw new ToolError('ERR_BAD_INPUT', 'max 50 files per fs.writeMany call');
    const results = [];
    for (const f of files) {
      try {
        results.push({ path: f.path, ok: true, ...write({ path: f.path, content: f.content }) });
      } catch (e) {
        results.push({ path: f.path, ok: false, error: e instanceof ToolError ? e.toJSON() : { code: 'ERR_INTERNAL', message: e.message } });
      }
    }
    const failed = results.filter((r) => !r.ok);
    return { results, written: results.length - failed.length, failed: failed.length };
  };

  const write = ({ path, content }) => {
    const abs = resolvePath(root, path);
    const existed = existsSync(abs);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, content, 'utf8');
    const bytes = Buffer.byteLength(content, 'utf8');
    return { path, bytes, created: !existed, overwrote: existed };
  };

  const append = ({ path, content }) => {
    const abs = resolvePath(root, path);
    const existed = existsSync(abs);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, content, { encoding: 'utf8', flag: 'a' });
    const bytes = Buffer.byteLength(content, 'utf8');
    return { path, bytes, created: !existed, appended: true };
  };

  const copy = ({ from, to, recursive = true }) => {
    const fromAbs = resolvePath(root, from);
    const toAbs = resolvePath(root, to);
    if (!existsSync(fromAbs)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${from}`, { path: from, nearestExisting: nearestSiblings(root, fromAbs) });
    const st = statSync(fromAbs);
    if (st.isDirectory() && !recursive) throw new ToolError('ERR_IS_DIRECTORY', `${from} is a directory; pass recursive=true to copy it`, { path: from });
    mkdirSync(dirname(toAbs), { recursive: true });
    cpSync(fromAbs, toAbs, { recursive: !!recursive });
    return { from, to, copied: true };
  };

  const list = ({ path = '.', recursive = false }) => {
    const abs = resolvePath(root, path);
    if (!existsSync(abs)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${path}`, { path });
    const entries = [];
    const walk = (dir, depth) => {
      for (const name of readdirSync(dir).sort()) {
        if (name === '.nc-tools' || name === '.git' || name === 'node_modules') continue;
        const full = join(dir, name);
        const st = statSync(full);
        const rel = relative(root, full).replaceAll('\\', '/');
        const isDir = st.isDirectory();
        entries.push({ name, path: rel, type: isDir ? 'dir' : 'file', size: isDir ? null : st.size });
        if (isDir && recursive && depth < 8) {
          // reparse points may loop back into the tree; list it but never recurse
          if (isReparsePoint(full)) continue;
          walk(full, depth + 1);
        }
      }
    };
    walk(abs, 0);
    return { entries, total: entries.length };
  };

  const stat = ({ path }) => {
    const abs = resolvePath(root, path);
    if (!existsSync(abs)) return { exists: false, path };
    const st = statSync(abs);
    return {
      exists: true, path,
      type: st.isDirectory() ? 'dir' : 'file',
      size: st.size, mtimeMs: st.mtimeMs,
    };
  };

  const mkdir = ({ path, recursive = true }) => {
    const abs = resolvePath(root, path);
    const existed = existsSync(abs);
    mkdirSync(abs, { recursive });
    return { path, created: !existed };
  };

  const del = ({ path, recursive = false }) => {
    const abs = resolvePath(root, path);
    if (abs === resolve(root) || abs === parse(abs).root) {
      throw new ToolError('ERR_REFUSED', 'Refusing to delete the base workspace dir or a filesystem root');
    }
    if (!existsSync(abs)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${path}`, { path, nearestExisting: nearestSiblings(root, abs) });
    const st = statSync(abs);
    if (st.isDirectory() && !recursive) {
      throw new ToolError('ERR_IS_DIRECTORY', `${path} is a directory; pass recursive=true`, { path });
    }
    rmSync(abs, { recursive: !!recursive });
    return { path, deleted: true };
  };

  const move = ({ from, to }) => {
    const fromAbs = resolvePath(root, from);
    const toAbs = resolvePath(root, to);
    if (!existsSync(fromAbs)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${from}`, { path: from });
    mkdirSync(dirname(toAbs), { recursive: true });
    renameSync(fromAbs, toAbs);
    return { from, to, moved: true };
  };

  return {
    'fs.read': { handler: read },
    'fs.readMany': { handler: readMany },
    'fs.write': { handler: write },
    'fs.writeMany': { handler: writeMany },
    'fs.append': { handler: append },
    'fs.copy': { handler: copy },
    'fs.list': { handler: list },
    'fs.stat': { handler: stat },
    'fs.mkdir': { handler: mkdir },
    'fs.delete': { handler: del },
    'fs.move': { handler: move },
  };
}
