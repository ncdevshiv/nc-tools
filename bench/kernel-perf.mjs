// Deterministic performance benchmark for the nc-tools MCP server: opaque
// MCP-stdio process, fixed workload, fixed corpus, no LLM involved.
//
// Usage: node bench/kernel-perf.mjs [outFile]
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const out = resolve(process.argv[2] || join(root, 'bench', 'results', 'kernel-perf.json'));
const ITERS = 15;

const binName = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const arms = {
  rust: { cmd: join(root, 'target', 'release', binName), args: [] },
};

// ---- fixed corpus -----------------------------------------------------------
function makeCorpus(ws) {
  mkdirSync(join(ws, 'src', 'mod'), { recursive: true });
  for (let i = 0; i < 40; i++) {
    const lines = [];
    for (let j = 0; j < 30; j++) {
      lines.push(`export function fn${i}_${j}(x) { return x * ${i + j}; }`);
    }
    if (i % 8 === 0) lines.push('// TODO-RARE: revisit this module');
    writeFileSync(join(ws, 'src', 'mod', `file${i}.js`), lines.join('\n') + '\n', 'utf8');
  }
  mkdirSync(join(ws, 'src'), { recursive: true });
  writeFileSync(join(ws, 'src', 'target.js'), 'export const value = 1;\nexport const sentinel = "KEEP";\n', 'utf8');
  writeFileSync(join(ws, 'src', 'big.js'), Array.from({ length: 400 }, (_, k) => `const pad${k} = ${k};`).join('\n'), 'utf8');
}

// ---- MCP stdio client -------------------------------------------------------
function connect(arm, ws) {
  return new Promise((res, rej) => {
    const t0 = performance.now();
    const child = spawn(arm.cmd, [...arm.args, ws], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    child.stderr.on('data', () => {});
    let buf = '';
    const pending = new Map();
    let id = 0;
    child.stdout.on('data', (d) => {
      buf += d.toString('utf8');
      let idx;
      while ((idx = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, idx).trim();
        buf = buf.slice(idx + 1);
        if (!line) continue;
        let m; try { m = JSON.parse(line); } catch { continue; }
        const p = pending.get(m.id);
        if (p) { pending.delete(m.id); p(m); }
      }
    });
    const rpc = (method, params) => new Promise((r) => {
      const myId = ++id;
      pending.set(myId, r);
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: myId, method, params }) + '\n');
    });
    (async () => {
      await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'kernel-perf', version: '1' } });
      const cold = performance.now() - t0;
      const tl = await rpc('tools/list', {});
      res({
        child, rpc,
        coldStartMs: cold,
        toolCount: tl.result.tools.length,
        kill: () => child.kill(),
      });
    })().catch(rej);
  });
}

const med = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.floor(s.length / 2)]; };
const p90 = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.min(s.length - 1, Math.floor(s.length * 0.9))]; };

async function timeOp(fn, iters = ITERS) {
  const samples = [];
  for (let i = 0; i < iters; i++) {
    const t = performance.now();
    await fn();
    samples.push(performance.now() - t);
  }
  return { median: Math.round(med(samples) * 10) / 10, p90: Math.round(p90(samples) * 10) / 10 };
}

// ---- workload definition ----------------------------------------------------
// Each op has a timed fn and a parity extractor (both arms must agree on the
// observable value, not just the timing).
function workload() {
  return {
    'fs.write (1KB)': {
      fn: (rpc) => rpc('tools/call', { name: 'fs.write', arguments: { path: 'out/w.txt', content: 'x'.repeat(1024) } }),
      parity: (r) => JSON.parse(r.result.content[0].text).bytes,
    },
    'fs.read (400 lines)': {
      fn: (rpc) => rpc('tools/call', { name: 'fs.read', arguments: { path: 'src/big.js' } }),
      parity: (r) => JSON.parse(r.result.content[0].text).content.length,
    },
    'fs.readMany (8 files)': {
      fn: (rpc) => rpc('tools/call', { name: 'fs.readMany', arguments: { paths: Array.from({ length: 8 }, (_, i) => `src/mod/file${i * 4}.js`) } }),
      parity: (r) => JSON.parse(r.result.content[0].text).files.filter((x) => x.ok).length,
    },
    'fs.list (recursive)': {
      fn: (rpc) => rpc('tools/call', { name: 'fs.list', arguments: { path: 'src', recursive: true } }),
      parity: (r) => JSON.parse(r.result.content[0].text).entries.length,
    },
    'search.grep (corpus)': {
      fn: (rpc) => rpc('tools/call', { name: 'search.grep', arguments: { pattern: 'TODO-RARE', path: 'src' } }),
      parity: (r) => JSON.parse(r.result.content[0].text).matches.length,
    },
    'patch.apply': {
      setup: (rpc) => rpc('tools/call', { name: 'fs.write', arguments: { path: 'src/target.js', content: 'export const value = 1;\nexport const sentinel = "KEEP";\n' } }),
      fn: (rpc) => rpc('tools/call', { name: 'patch.apply', arguments: { path: 'src/target.js', edits: [{ oldText: 'export const sentinel = "KEEP";', newText: 'export const sentinel = "PATCHED";' }] } }),
      parity: (r) => JSON.parse(r.result.content[0].text).applied.map((a) => a.replacements).join(','),
    },
    'snapshot+rollback': {
      fn: async (rpc) => {
        const s = await rpc('tools/call', { name: 'sys.snapshot', arguments: {} });
        const sid = JSON.parse(s.result.content[0].text).id;
        await rpc('tools/call', { name: 'fs.write', arguments: { path: 'out/after.txt', content: 'mutate' } });
        return rpc('tools/call', { name: 'sys.rollback', arguments: { id: sid } });
      },
      parity: (r) => JSON.stringify(Object.keys(JSON.parse(r.result.content[0].text)).sort()),
    },
    'batch.execute (10 reads)': {
      fn: (rpc) => rpc('tools/call', { name: 'batch.execute', arguments: { calls: Array.from({ length: 10 }, (_, i) => ({ tool: 'fs.read', args: { path: `src/mod/file${i}.js` } })) } }),
      parity: (r) => JSON.parse(r.result.content[0].text).results.filter((x) => x.ok).length,
    },
  };
}

// ---- run --------------------------------------------------------------------
const results = { ts: new Date().toISOString(), iters: ITERS, arms: {}, parity: {} };
const parityFails = [];

for (const [name, arm] of Object.entries(arms)) {
  const ws = mkdtempSync(join(tmpdir(), 'ncperf-'));
  makeCorpus(ws);
  const conn = await connect(arm, ws);
  console.log(`${name}: cold start ${conn.coldStartMs.toFixed(0)}ms, ${conn.toolCount} tools`);
  results.arms[name] = { coldStartMs: Math.round(conn.coldStartMs), toolCount: conn.toolCount, ops: {} };
  const wl = workload();
  for (const [op, spec] of Object.entries(wl)) {
    if (spec.setup) await spec.setup(conn.rpc);
    const first = await spec.fn(conn.rpc);
    let parityVal;
    try { parityVal = spec.parity(first); } catch { parityVal = 'parse-error'; }
    (results.parity[op] ??= {})[name] = parityVal;
    const t = await timeOp(spec.fn.bind(null, conn.rpc));
    results.arms[name].ops[op] = { median: t.median, p90: t.p90 };
    console.log(`  ${op.padEnd(24)} median ${String(t.median).padStart(7)}ms  p90 ${String(t.p90).padStart(7)}ms  parity=${parityVal}`);
  }
  conn.kill();
  rmSync(ws, { recursive: true, force: true });
}

// parity cross-check
for (const [op, byArm] of Object.entries(results.parity)) {
  const vals = Object.values(byArm);
  if (new Set(vals).size !== 1) parityFails.push(`${op}: ${JSON.stringify(byArm)}`);
}
results.parityMatch = parityFails.length === 0;
console.log(`\nPARITY: ${parityFails.length === 0 ? 'MATCH — identical observable results on every op' : 'MISMATCH: ' + parityFails.join('; ')}`);
console.log(`\ncold start: js ${results.arms.js.coldStartMs}ms vs rust ${results.arms.rust.coldStartMs}ms`);
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, JSON.stringify(results, null, 2) + '\n', 'utf8');
console.log(`saved: ${out}`);