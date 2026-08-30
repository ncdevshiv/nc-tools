// Kernel: wires tool families together, owns the journal, executes calls with
// uniform event recording. This is the object both the agent and the MCP
// server bind to.
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { Journal } from './journal.mjs';
import { makeFsTools } from './fs.mjs';
import { makePatchTools } from './patch.mjs';
import { makeSearchTools } from './search.mjs';
import { makeGitTools } from './git.mjs';
import { makeProcTools } from './proc.mjs';
import { ToolError } from './errors.mjs';

export class Kernel {
  /** @param {string} root absolute workspace root */
  constructor(root, { journalPath } = {}) {
    this.root = root;
    this.journal = new Journal(journalPath ?? join(root, '.nc-tools', 'journal.jsonl'));
    const families = [
      makeFsTools(root),
      makePatchTools(root),
      makeSearchTools(root),
      makeGitTools(root),
      makeProcTools(root),
    ];
    /** @type {Map<string, {handler: Function, description?: string, inputSchema?: object}>} */
    this.tools = new Map();
    for (const fam of families) {
      for (const [name, def] of Object.entries(fam)) this.tools.set(name, def);
    }
    // sys tools (need the kernel itself)
    this.tools.set('sys.journal', {
      handler: ({ lastN }) => ({ events: this.journal.lastN(lastN ?? 100), total: this.journal.seq }),
    });
    this.tools.set('sys.workspace', {
      handler: () => ({ root: this.root, platform: process.platform, node: process.version }),
    });
    // batch.execute — run a list of kernel calls in one round-trip.
    // Each sub-call is executed and journaled individually; one bad item does not abort the rest.
    this.tools.set('batch.execute', {
      handler: async ({ calls }) => {
        if (!Array.isArray(calls) || calls.length === 0) {
          throw new ToolError('ERR_BAD_INPUT', 'calls must be a non-empty array of {tool, args}');
        }
        if (calls.length > 25) throw new ToolError('ERR_BAD_INPUT', 'max 25 calls per batch.execute');
        const results = [];
        for (const c of calls) {
          if (!c || typeof c.tool !== 'string') {
            results.push({ ok: false, error: { code: 'ERR_BAD_INPUT', message: 'each call needs a string tool' } });
            continue;
          }
          if (c.tool === 'batch.execute') {
            results.push({ ok: false, error: { code: 'ERR_REFUSED', message: 'batch.execute cannot nest itself' } });
            continue;
          }
          results.push(await this.call(c.tool, c.args ?? {}));
        }
        const okCount = results.filter((r) => r.ok).length;
        return { results, ok: okCount, failed: results.length - okCount };
      },
    });
  }

  listTools() {
    return [...this.tools.keys()].sort();
  }

  /**
   * Execute a tool call with journaling.
   * @returns {Promise<{ok: boolean, result?: object, error?: {code, message, hint?}, durationMs: number, seq: number}>}
   */
  async call(tool, args = {}) {
    const started = Date.now();
    const def = this.tools.get(tool);
    const callEvent = this.journal.append('tool.call', { tool, args });
    if (!def) {
      const err = new ToolError('ERR_UNKNOWN_TOOL', `Unknown tool: ${tool}`, { available: this.listTools() });
      return this.#finish(callEvent, tool, false, null, err, started);
    }
    try {
      const result = await def.handler(args);
      return this.#finish(callEvent, tool, true, result, null, started);
    } catch (e) {
      const err = e instanceof ToolError ? e : new ToolError('ERR_INTERNAL', e.message);
      return this.#finish(callEvent, tool, false, null, err, started);
    }
  }

  #finish(callEvent, tool, ok, result, error, started) {
    const durationMs = Date.now() - started;
    const resultEvent = this.journal.append('tool.result', { tool, ok, result, error: error?.toJSON(), durationMs, callSeq: callEvent.seq });
    return { ok, result, error: error?.toJSON(), durationMs, seq: resultEvent.seq };
  }
}

export { ToolError };
export function ensureDir(p) { mkdirSync(p, { recursive: true }); }
