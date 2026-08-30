// fs.* tools — typed file operations, jailed to the workspace.
import { readFileSync, writeFileSync, readdirSync, statSync, mkdirSync, rmSync, renameSync, existsSync } from 'node:fs';
import { join, relative, dirname, basename, resolve, isAbsolute } from 'node:path';
import { ToolError } from './errors.mjs';
import { inWorkspace } from './paths.mjs';

export function makeFsTools(root) {
  const read = ({ path, offset, limit }) => {
    const abs = inWorkspace(root, path);
    if (!existsSync(abs)) throw new ToolError('ERR_NOT_FOUND', `No such file: ${path}`, { path });
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
    };
  };

  const write = ({ path, content }) => {
    const abs = inWorkspace(root, path);
    const existed = existsSync(abs);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, content, 'utf8');
    const bytes = Buffer.byteLength(content, 'utf8');
    return { path, bytes, created: !existed, overwrote: existed };
  };

  const list = ({ path = '.', recursive = false }) => {
    const abs = inWorkspace(root, path);
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
        if (isDir && recursive && depth < 8) walk(full, depth + 1);
      }
    };
    walk(abs, 0);
    return { entries, total: entries.length };
  };

  const stat = ({ path }) => {
    const abs = inWorkspace(root, path);
    if (!existsSync(abs)) return { exists: false, path };
    const st = statSync(abs);
    return {
      exists: true, path,
      type: st.isDirectory() ? 'dir' : 'file',
      size: st.size, mtimeMs: st.mtimeMs,
    };
  };

  const mkdir = ({ path, recursive = true }) => {
    const abs = inWorkspace(root, path);
    const existed = existsSync(abs);
    mkdirSync(abs, { recursive });
    return { path, created: !existed };
  };

  const del = ({ path, recursive = false }) => {
    const abs = inWorkspace(root, path);
    if (abs === resolve(root)) throw new ToolError('ERR_REFUSED', 'Refusing to delete the workspace root');
    if (!existsSync(abs)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${path}`, { path });
    const st = statSync(abs);
    if (st.isDirectory() && !recursive) {
      throw new ToolError('ERR_IS_DIRECTORY', `${path} is a directory; pass recursive=true`, { path });
    }
    rmSync(abs, { recursive: !!recursive });
    return { path, deleted: true };
  };

  const move = ({ from, to }) => {
    const fromAbs = inWorkspace(root, from);
    const toAbs = inWorkspace(root, to);
    if (!existsSync(fromAbs)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${from}`, { path: from });
    mkdirSync(dirname(toAbs), { recursive: true });
    renameSync(fromAbs, toAbs);
    return { from, to, moved: true };
  };

  return {
    'fs.read': { handler: read },
    'fs.write': { handler: write },
    'fs.list': { handler: list },
    'fs.stat': { handler: stat },
    'fs.mkdir': { handler: mkdir },
    'fs.delete': { handler: del },
    'fs.move': { handler: move },
  };
}
