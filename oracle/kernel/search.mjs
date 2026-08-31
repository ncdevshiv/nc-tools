// search.* tools — line-based regex grep and glob-ish file search, machine-wide.
import { readdirSync, readFileSync, lstatSync, statSync, existsSync } from 'node:fs';
import { join, relative, extname, dirname, basename } from 'node:path';
import { ToolError } from './errors.mjs';
import { resolvePath, isReparsePoint } from './paths.mjs';
import { nearestSiblings } from './fs.mjs';

const TEXT_EXT = new Set(['.js', '.mjs', '.cjs', '.ts', '.tsx', '.jsx', '.json', '.md', '.txt', '.css', '.html', '.py', '.rs', '.go', '.java', '.yml', '.yaml', '.toml', '.sh', '.c', '.h', '.cpp', '.hpp', '.sql', '.env', '.gitignore', '.log', '']);

function* walkFiles(root, dir, depth = 0) {
  if (depth > 12) return;
  for (const name of readdirSync(dir).sort()) {
    if (name === '.git' || name === 'node_modules' || name === '.nc-tools') continue;
    const full = join(dir, name);
    let st;
    try { st = lstatSync(full); } catch { continue; }
    if (st.isSymbolicLink()) continue; // never follow links (cycles)
    if (st.isDirectory()) {
      if (isReparsePoint(full)) continue; // junction: skip (cycle safety)
      yield* walkFiles(root, full, depth + 1);
    } else yield full;
  }
}

export function makeSearchTools(root) {
  const grep = ({ pattern, path = '.', glob, maxResults = 200 }) => {
    let re;
    try { re = new RegExp(pattern); } catch (e) {
      throw new ToolError('ERR_BAD_REGEX', `Invalid regex: ${e.message}`, { pattern });
    }
    const base = resolvePath(root, path);
    if (!existsSync(base)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${path}`, { nearestExisting: nearestSiblings(root, base) });
    // path may name a single file (e.g. docs/PROTOCOL.md); only walk if a dir
    const files = [];
    if (statSync(base).isDirectory()) files.push(...walkFiles(root, base));
    else files.push(base);
    const matches = [];
    let total = 0;
    let truncated = false;
    for (const file of files) {
      if (glob && !globMatch(glob, relative(base, file).replaceAll('\\', '/'))) continue;
      if (!TEXT_EXT.has(extname(file).toLowerCase()) && extname(file) !== '') continue;
      let content;
      try { content = readFileSync(file, 'utf8'); } catch { continue; }
      if (content.includes('\u0000')) continue; // binary
      const lines = content.split('\n');
      for (let i = 0; i < lines.length; i++) {
        if (re.test(lines[i])) {
          total += 1;
          if (matches.length < maxResults) {
            matches.push({
              file: relative(root, file).replaceAll('\\', '/'),
              line: i + 1,
              text: lines[i].slice(0, 400),
            });
          } else {
            truncated = true;
          }
        }
      }
    }
    return { matches, total, truncated };
  };

  const files = ({ pattern, path = '.' }) => {
    const base = resolvePath(root, path);
    if (!existsSync(base)) throw new ToolError('ERR_NOT_FOUND', `No such path: ${path}`, { path });
    const out = [];
    if (statSync(base).isDirectory()) {
      for (const file of walkFiles(root, base)) {
        const rel = relative(base, file).replaceAll('\\', '/');
        if (globMatch(pattern, rel)) out.push(relative(root, file).replaceAll('\\', '/'));
      }
    } else if (globMatch(pattern, basename(base)) || globMatch(pattern, relative(root, base).replaceAll('\\', '/'))) {
      out.push(relative(root, base).replaceAll('\\', '/'));
    }
    return { files: out, total: out.length };
  };

  return { 'search.grep': { handler: grep }, 'search.files': { handler: files } };
}

/** Minimal glob: ** crosses directories, * within a segment, ? one char. */
export function globMatch(glob, str) {
  const re = globEscape(glob);
  return re.test(str);
}
function globEscape(glob) {
  let rx = '';
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === '*') {
      if (glob[i + 1] === '*') { rx += '.*'; i++; if (glob[i + 1] === '/') i++; }
      else rx += '[^/]*';
    } else if (c === '?') rx += '[^/]';
    else rx += c.replace(/[.+^${}()|[\]\\]/g, '\\$&');
  }
  return new RegExp(`^${rx}$`);
}
