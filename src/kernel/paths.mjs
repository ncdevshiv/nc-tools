// Workspace path jail: resolve any tool path against the workspace root and
// refuse escapes. This is the safety boundary of the whole kernel.
import { isAbsolute, join, resolve, sep, dirname, basename, relative } from 'node:path';
import { readdirSync, existsSync, realpathSync } from 'node:fs';
import { ToolError } from './errors.mjs';

const isWin = process.platform === 'win32';
// Windows paths are case-insensitive; resolve() preserves the case the caller
// used, so comparisons must be case-folded or legitimately-inside absolute
// paths (F:/NC-TOOLS/… vs F:\nc-tools\) would be falsely rejected.
const KEY = isWin ? (p) => p.toLowerCase() : (p) => p;

/** True if absolute path candidate resides inside absolute root (string compare). */
export function isInsidePath(root, candidate) {
  const rootKey = KEY(resolve(root));
  const absKey = KEY(candidate);
  return absKey === rootKey || absKey.startsWith(rootKey + KEY(sep));
}

/**
 * Resolve the real on-disk location of abs, treating it like a path that is
 * being created: nonexistent trailing components are appended to the deepest
 * existing ancestor, which IS realpath'd (so symlinks/junctions in the
 * existing portion are exposed).
 */
export function realPathOf(abs) {
  let p = abs;
  const tail = [];
  for (;;) {
    try {
      const real = realpathSync(p);
      return tail.length ? join(real, ...tail) : real;
    } catch {
      const parent = dirname(p);
      if (parent === p) return abs; // filesystem root unreachable; lexical check already held
      tail.unshift(basename(p));
      p = parent;
    }
  }
}

/**
 * @param {string} root absolute workspace root
 * @param {string} p user-supplied path (relative preferred, absolute tolerated if inside)
 * @returns {string} absolute path guaranteed inside root
 */
export function inWorkspace(root, p) {
  if (typeof p !== 'string' || p.length === 0) {
    throw new ToolError('ERR_BAD_PATH', 'path must be a non-empty string');
  }
  const abs = isAbsolute(p) ? resolve(p) : resolve(join(root, p));
  const normRoot = resolve(root);
  if (!isInsidePath(normRoot, abs)) {
    // Actionable hint: what DOES exist near where the agent probably wanted to be?
    const hint = { workspaceRoot: normRoot, attempted: abs };
    try {
      const dir = dirname(abs);
      const names = existsSync(dir) ? readdirSync(dir).filter((n) => !n.startsWith('.')).sort().slice(0, 8) : [];
      if (names.length) hint.existingEntries = names.map((n) => relative(normRoot, join(dir, n)).replaceAll('\\', '/'));
      hint.suggestion = 'Paths must stay inside the workspace root. Use fs.list {path:"."} to orient.';
    } catch { /* best-effort hint */ }
    throw new ToolError(
      'ERR_PATH_ESCAPE',
      `Path escapes workspace: ${p} resolves outside ${normRoot}`,
      hint
    );
  }
  // Reparse-point check: a symlink or junction inside the workspace may point
  // outside; the lexical check alone would let reads/writes walk through it.
  const realRoot = realPathOf(normRoot);
  const real = realPathOf(abs);
  if (!isInsidePath(realRoot, real)) {
    throw new ToolError(
      'ERR_PATH_ESCAPE',
      `Path escapes workspace through a symlink/junction: ${p} resolves to ${real}`,
      {
        workspaceRoot: normRoot, attempted: abs, resolvedTo: real,
        suggestion: 'The workspace contains a symlink or junction pointing outside the workspace root.',
      }
    );
  }
  return abs;
}
