// Semantic parity probe: run the same queries against BOTH implementations and
// compare the ranking order (scores differ in low-order digits; order must agree).
// Usage: node benchmark/semantic-parity.mjs
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';

const ws = mkdtempSync(join(tmpdir(), 'nc-embed-parity-'));
const files = {
  'f1.txt': 'short text about payments',
  'f2.txt': 'word '.repeat(100),
  'f3.txt': 'sentence about billing and credit cards. '.repeat(50),
  'f4.txt': 'x'.repeat(5000),
  'f5.txt': 'data '.repeat(2000),
  'f6.txt': 'alpha beta gamma '.repeat(400),
};
for (const [n, c] of Object.entries(files)) writeFileSync(join(ws, n), c);

function probe(cmd, args) {
  return new Promise((resolvePromise) => {
    const child = spawn(cmd, [...args, ws], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    child.stderr.on('data', () => {});
    let buf = '';
    const pending = new Map();
    let id = 0;
    child.stdout.on('data', (d) => {
      buf += d.toString();
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const l = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (!l) continue;
        try { const m = JSON.parse(l); const p = pending.get(m.id); if (p) { pending.delete(m.id); p(m); } } catch {}
      }
    });
    const rpc = (method, params) => new Promise((r) => {
      const my = ++id;
      pending.set(my, r);
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: my, method, params }) + '\n');
    });
    (async () => {
      await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'parity', version: '1' } });
      const out = {};
      for (const q of ['how are payments handled?', 'billing logic', 'alpha beta gamma']) {
        const r = await rpc('tools/call', { name: 'search.semantic', arguments: { query: q, topK: 3 } });
        try {
          const res = JSON.parse(r.result.content[0].text);
          out[q] = res.top ? res.top.map((t) => t.file) : `ERR: ${res.error?.message?.slice(0, 60)}`;
        } catch { out[q] = 'parse-error'; }
      }
      child.kill();
      resolvePromise(out);
    })();
  });
}

const js = await probe(process.execPath, ['F:/nc-tools/oracle/mcp/server.mjs']);
const rust = await probe(process.env.NCTOOLS_RUST_EXE || 'F:/nc-tools/rust/target/release/nc-tools-mcp.exe', []);
for (const q of Object.keys(js)) {
  const same = JSON.stringify(js[q]) === JSON.stringify(rust[q]);
  console.log(`${same ? 'MATCH ' : 'DIFF  '} ${JSON.stringify(q)}\n  js:   ${JSON.stringify(js[q])}\n  rust: ${JSON.stringify(rust[q])}`);
}
rmSync(ws, { recursive: true, force: true });