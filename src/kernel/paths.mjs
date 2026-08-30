// Workspace path jail: resolve any tool path against the workspace root and
// refuse escapes. This is the safety boundary of the whole kernel.
import { isAbsolute, join, resolve, sep } from 'node:path';
import { ToolError } from './errors.mjs';

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
  if (abs !== normRoot && !abs.startsWith(normRoot + sep)) {
    throw new ToolError(
      'ERR_PATH_ESCAPE',
      `Path escapes workspace: ${p} resolves outside ${normRoot}`,
      { workspaceRoot: normRoot, attempted: abs }
    );
  }
  return abs;
}
