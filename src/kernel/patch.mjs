// patch.apply — exact-match search/replace editing with occurrence semantics.
// This replaces sed/regex line surgery with a deliberate, verifiable edit.
import { readFileSync, writeFileSync, existsSync, statSync, readdirSync } from 'node:fs';
import { dirname, basename, relative, join } from 'node:path';
import { ToolError } from './errors.mjs';
import { inWorkspace } from './paths.mjs';
import { nearestSiblings } from './fs.mjs';

/**
 * Apply edits [{oldText, newText, expectedCount?}] to a file.
 * Every oldText must occur exactly `expectedCount ?? 1` times, else nothing is
 * written and a structured error is returned with nearest-match hints.
 */
export function makePatchTools(root) {
  const apply = ({ path, edits }) => {
    const abs = inWorkspace(root, path);
    if (!existsSync(abs) || statSync(abs).isDirectory()) {
      throw new ToolError('ERR_NOT_FOUND', `No such file: ${path}`, { path, nearestExisting: nearestSiblings(root, abs) });
    }
    const src = readFileSync(abs, 'utf8');
    const lines = src.split('\n');

    const applied = [];
    let out = src;
    for (let i = 0; i < edits.length; i++) {
      const { oldText, newText, expectedCount } = edits[i];
      if (typeof oldText !== 'string' || oldText.length === 0) {
        throw new ToolError('ERR_BAD_EDIT', `edits[${i}].oldText must be a non-empty string`);
      }
      const count = out.split(oldText).length - 1;
      const want = expectedCount ?? 1;
      if (count === 0) {
        const candidates = nearestMatches(lines, oldText);
        throw new ToolError(
          'PATCH_NO_MATCH',
          `edits[${i}]: oldText not found in ${path}`,
          { editIndex: i, occurrences: 0, expected: want, nearestCandidateLines: candidates }
        );
      }
      if (count !== want) {
        throw new ToolError(
          'PATCH_AMBIGUOUS',
          `edits[${i}]: oldText occurs ${count} time(s) in ${path}, expected ${want}. Include more surrounding context or pass expectedCount.`,
          { editIndex: i, occurrences: count, expected: want }
        );
      }
      out = out.split(oldText).join(newText);
      applied.push({ index: i, replacements: count });
    }
    writeFileSync(abs, out, 'utf8');
    return { path, applied, bytes: Buffer.byteLength(out, 'utf8') };
  };

  /** Apply patch edits to many files in one call. Stops collecting on first per-file error. */
  const applyMany = ({ edits: fileEdits }) => {
    if (!Array.isArray(fileEdits) || fileEdits.length === 0) {
      throw new ToolError('ERR_BAD_INPUT', 'edits must be a non-empty array of {path, edits}');
    }
    if (fileEdits.length > 20) throw new ToolError('ERR_BAD_INPUT', 'max 20 files per patch.applyMany call');
    const results = [];
    for (const fe of fileEdits) {
      try {
        const r = apply({ path: fe.path, edits: fe.edits });
        results.push({ path: fe.path, ok: true, applied: r.applied });
      } catch (e) {
        results.push({ path: fe.path, ok: false, error: e instanceof ToolError ? e.toJSON() : { code: 'ERR_INTERNAL', message: e.message } });
      }
    }
    const failed = results.filter((r) => !r.ok);
    return { results, patched: results.length - failed.length, failed: failed.length };
  };

  return { 'patch.apply': { handler: apply }, 'patch.applyMany': { handler: applyMany } };
}

/** Find line numbers of lines that share the most tokens with oldText. */
function nearestMatches(lines, oldText) {
  const needle = oldText.split('\n')[0].trim();
  if (!needle) return [];
  const tokens = needle.split(/\s+/).filter((t) => t.length > 3).map((t) => t.toLowerCase());
  if (tokens.length === 0) return [];
  const scored = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].toLowerCase();
    const hits = tokens.filter((t) => line.includes(t)).length;
    if (hits > 0) scored.push({ line: i + 1, hits });
  }
  scored.sort((a, b) => b.hits - a.hits);
  return scored.slice(0, 5).map((s) => s.line);
}
