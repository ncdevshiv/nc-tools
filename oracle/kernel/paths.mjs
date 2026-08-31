// Path resolution for the global tool system: nc-tools is a machine-wide
// kernel and any remote agent may work in ANY directory. Absolute paths are
// used as-is; relative paths resolve against the base directory given to the
// server at startup. There is no jail — the base dir only anchors relative
// paths, the journal and snapshots.
import { isAbsolute, join, resolve } from 'node:path';
import { realpathSync } from 'node:fs';
import { ToolError } from './errors.mjs';

const isWin = process.platform === 'win32';
// Windows paths are case-insensitive; resolve() preserves the case the caller
// used, so reparse-point comparison must be case-folded on win32.
const KEY = isWin ? (p) => p.toLowerCase() : (p) => p;

/**
 * Resolve a user-supplied path against the base: absolute paths are accepted
 * anywhere on the machine; relative paths resolve against the base dir.
 * @param {string} base absolute base directory (anchors relative paths)
 * @param {string} p user-supplied path
 * @returns {string} absolute path
 */
export function resolvePath(base, p) {
  if (typeof p !== 'string' || p.length === 0) {
    throw new ToolError('ERR_BAD_PATH', 'path must be a non-empty string');
  }
  return isAbsolute(p) ? resolve(p) : resolve(join(base, p));
}

/**
 * True if abs is a reparse point (symlink or junction on Windows). Recursive
 * walks skip these — not to jail paths, but to keep cycles from looping.
 */
export function isReparsePoint(abs) {
  try {
    return KEY(realpathSync(abs)) !== KEY(resolve(abs));
  } catch {
    return false;
  }
}
