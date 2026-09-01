// Driver harness: drives the Rust MCP server (rust/target/release/nc-tools-mcp)
// exactly like a client would, over MCP stdio. Exposes a small Kernel-like
// facade (`call`, `listTools`, `close`) so the test-suite can exercise the
// Rust implementation without importing the archived JS kernel in-process.
//
// The binary is the live artifact (release); override with NCTOOLS_MCP_EXE.
import { spawn } from 'node:child_process';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const binName = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
export const SERVER_BIN =
  process.env.NCTOOLS_MCP_EXE || join(here, '..', 'rust', 'target', 'release', binName);

/** Spawn one Rust MCP server for `root`, return an rpc()/close() handle. */
export function spawnRustServer(root) {
  const child = spawn(SERVER_BIN, [root], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  child.stderr.on('data', () => {}); // startup banner
  let seq = 0;
  const pending = new Map();
  child.stdout.on('data', (buf) => {
    for (const line of buf.toString('utf8').split('\n')) {
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
    const timer = setTimeout(() => rejectPromise(new Error(`${method} timeout`)), 30_000);
    pending.set(id, (m) => { clearTimeout(timer); resolvePromise(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  const close = () => { try { child.stdin.end(); } catch {}; try { child.kill(); } catch {}; };
  return { child, rpc, close };
}

/**
 * Kernel-like facade over the Rust server. Mirrors the subset of the archived
 * JS `Kernel` API used by the tests:
 *   await k.call(tool, args) -> { ok:boolean, result?:object, error?:{code,message,hint?} }
 *   await k.listTools()      -> [{name, description, inputSchema}]
 *   k.close()
 */
export class RustKernel {
  constructor(root) {
    this.root = root;
    this.server = spawnRustServer(root);
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

/** Convenience: run `fn(k)` and always close the kernel in a finally. */
export async function withKernel(root, fn) {
  const k = new RustKernel(root);
  try {
    return await fn(k);
  } finally {
    k.close();
  }
}
