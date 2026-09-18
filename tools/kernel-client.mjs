// Shared MCP-stdio client for driving the nc-tools kernel exactly like a
// client would: spawn the binary, speak JSON-RPC, get typed results.
// Used by the test suite (tests/) and the bench harness (bench/).
//
// Binary resolution: NCTOOLS_MCP_EXE override, else <repo>/target/release/
// nc-tools-mcp(.exe) — build it with `cargo build --release -p nct-mcp`.
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const binName = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
export const SERVER_BIN =
  process.env.NCTOOLS_MCP_EXE || join(here, '..', 'target', 'release', binName);

/** Spawn one nc-tools MCP server for `root`; returns an rpc()/close() handle. */
export function spawnServer(root) {
  const child = spawn(SERVER_BIN, [root], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  // Unref so idle pooled servers never hold the host event loop open (the
  // process can still exit and clean them up); in-flight rpc()s are kept
  // alive by their own timeout timers.
  child.unref();
  child.stdout.unref();
  child.stderr.unref();
  child.stdin.unref();
  child.stderr.on('data', () => {}); // startup banner
  let seq = 0;
  const pending = new Map();
  // Line reassembly: one JSON response is ONE stdout line, but a 500KB+
  // response (a full web page) spans many 'data' chunks — without a carry
  // buffer the tail chunks never parse and the call looks like a timeout.
  let carry = '';
  child.stdout.on('data', (buf) => {
    carry += buf.toString('utf8');
    let idx;
    while ((idx = carry.indexOf('\n')) >= 0) {
      const line = carry.slice(0, idx);
      carry = carry.slice(idx + 1);
      const t = line.trim();
      if (!t) continue;
      let msg;
      try { msg = JSON.parse(t); } catch { continue; }
      const p = pending.get(msg.id);
      if (p) { pending.delete(msg.id); p(msg); }
    }
  });
  const rpc = (method, params) => new Promise((resolvePromise, rejectPromise) => {
    const id = ++seq;
    // rpc ceiling: 30s covers every kernel op; live-net waves override via env
    const timer = setTimeout(() => rejectPromise(new Error(`${method} timeout`)), Number(process.env.NCTOOLS_RPC_TIMEOUT_MS) || 30_000);
    pending.set(id, (m) => { clearTimeout(timer); resolvePromise(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  const close = () => { try { child.stdin.end(); } catch {}; try { child.kill(); } catch {}; };
  return { child, rpc, close };
}

/**
 * Kernel-style facade over the nc-tools MCP server:
 *   await k.call(tool, args) -> { ok:boolean, result?:object, error?:{code,message,hint?} }
 *   await k.listTools()      -> [{name, description, inputSchema}]
 *   k.journal.readAll()      -> [event]   (direct read of the on-disk journal,
 *                             <root>/.nc-tools/journal.jsonl — protocol path)
 *   k.close()
 */
export class Kernel {
  constructor(root) {
    this.root = root;
    this.server = spawnServer(root);
    this.journal = {
      readAll: () => {
        try {
          const raw = readFileSync(join(root, '.nc-tools', 'journal.jsonl'), 'utf8');
          return raw.split('\n').filter(Boolean).map((l) => JSON.parse(l));
        } catch {
          return [];
        }
      },
    };
  }

  async call(tool, args = {}) {
    const resp = await this.server.rpc('tools/call', { name: tool, arguments: args });
    const text = resp.result?.content?.[0]?.text ?? '';
    if (resp.result?.isError) {
      let err = { code: 'ERR_PARSE', message: text };
      try { err = JSON.parse(text).error || err; } catch { /* keep raw */ }
      return { ok: false, error: err };
    }
    let result;
    try { result = JSON.parse(text); } catch { result = text; }
    return { ok: true, result };
  }

  async listTools() {
    const resp = await this.server.rpc('tools/list', {});
    return resp.result.tools;
  }

  close() { this.server.close(); }
}

// Per-root server pool: callers construct `new Kernel(root)` freely (tests do,
// once per test case); instances on the same root share one spawned server,
// torn down at process exit. Returning the pooled instance from the
// constructor makes every `new Kernel(root)` a handle, not a new process.
const pool = new Map(); // root -> Kernel
export function pooledKernel(root) {
  let k = pool.get(root);
  if (!k) {
    k = new Kernel(root);
    pool.set(root, k);
  }
  return k;
}
process.on('exit', () => { for (const k of pool.values()) k.close(); });

/** Convenience: run `fn(k)` against a dedicated kernel, always close it. */
export async function withKernel(root, fn) {
  const k = new Kernel(root);
  try {
    return await fn(k);
  } finally {
    k.close();
  }
}
