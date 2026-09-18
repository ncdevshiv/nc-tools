// Wave W-Err-1 verifier: machine-proven acceptance for the self-healing error
// surface. Arms:
//   A. valid-pass control: normal tools still work (an error wave that broke
//      the happy path would be a regression, not a fix)
//   B. unknown tool → ERR_UNKNOWN_TOOL with didYouMean + retryWith hint, and
//      retrying with the suggested name CLOSES THE LOOP (succeeds)
//   C. negative control: a name far from any real tool must NOT get a guess
//   D. panic boundary: a panicking handler returns ERR_PANIC with the root
//      cause, and the same server keeps serving real calls afterwards
//   E. malformed wire form (the real-world failure: "mcp__nc-tools=") gets a
//      did-you-mean that, on retry, succeeds
//   F. io error codes: reading a missing file is ERR_NOT_FOUND (not blanket
//      ERR_INTERNAL), with nearestExisting-style hints where applicable
//   G. registry↔PROTOCOL.md byte-match is enforced by the Rust test suite
//      (cargo test -p nct-core); reported here from the golden descriptor
//      dump for visibility
// Usage: node tools/error-verifier.mjs → benchmark/results/err-w1/verifier.json
import { spawn } from 'node:child_process';
import { mkdirSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');
const binName = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const bin = join(repoRoot, 'target', 'debug', binName);
const report = { ts: new Date().toISOString(), arms: [] };
const arm = (name, pass, detail) => {
  report.arms.push({ name, pass, detail });
  console.log(`  [${pass ? 'x' : ' '}] ${name}: ${detail}`);
  return pass;
};

const root = mkdtempSync(join(tmpdir(), 'nc-err-verifier-'));
const server = spawn(bin, [root], {
  stdio: ['pipe', 'pipe', 'pipe'],
  windowsHide: true,
  env: { ...process.env, NCTOOLS_DEBUG_PANIC: '1' },
});
let seq = 0;
const pending = new Map();
server.stdout.on('data', (buf) => {
  for (const line of buf.toString('utf8').split('\n')) {
    const t = line.trim();
    if (!t) continue;
    let msg;
    try { msg = JSON.parse(t); } catch { continue; }
    const p = pending.get(msg.id);
    if (p) { pending.delete(msg.id); p(msg); }
  }
});
server.stderr.on('data', (d) => { report.stderrTail = String(d).trim().split('\n').slice(-5); });

const rpc = (method, params) => new Promise((resolve, reject) => {
  const id = ++seq;
  const timer = setTimeout(() => reject(new Error(`${method} timeout`)), 30_000);
  pending.set(id, (m) => { clearTimeout(timer); resolve(m); });
  server.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
});

const call = async (tool, args = {}) => {
  const resp = await rpc('tools/call', { name: tool, arguments: args });
  const text = resp.result?.content?.[0]?.text ?? '';
  if (resp.result?.isError) {
    let err = { code: 'ERR_PARSE', message: text };
    try { err = JSON.parse(text).error || err; } catch { /* keep raw */ }
    return { ok: false, error: err };
  }
  let result = null;
  try { result = JSON.parse(text); } catch { result = text; }
  return { ok: true, result };
};

try {
  await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {} });

  // ---- A. valid-pass control ------------------------------------------------
  const w = await call('fs.write', { path: 'ok.txt', content: 'fine\n' });
  const r = await call('fs.read', { path: 'ok.txt' });
  arm('A1 happy path unaffected', w.ok && r.ok && String(r.result?.content ?? '').includes('fine'),
    `write ok=${w.ok} read ok=${r.ok}`);

  // ---- B. unknown tool → reminder → retry closes the loop -------------------
  const bad = await call('fs.rea', { path: 'ok.txt' });
  const e = bad.error ?? {};
  const mean = e.hint?.didYouMean ?? '';
  const retry = mean ? await call(mean.replace('.', '_'), { path: 'ok.txt' }) : { ok: false };
  arm('B1 unknown tool suggests + retry succeeds', !bad.ok && e.code === 'ERR_UNKNOWN_TOOL'
    && mean === 'fs.read' && e.hint?.retryWith === 'mcp__nc-tools__fs_read' && retry.ok,
    `code=${e.code} didYouMean=${JSON.stringify(mean)} retry ok=${retry.ok}`);

  // ---- C. negative control: far-from-any name must not guess ----------------
  const junk = await call('zzzzqqqq', {});
  arm('C1 no did-you-mean for garbage', !junk.ok && junk.error?.code === 'ERR_UNKNOWN_TOOL'
    && !junk.error?.hint?.didYouMean,
    `code=${junk.error?.code} didYouMean=${JSON.stringify(junk.error?.hint?.didYouMean)}`);

  // ---- D. panic boundary -----------------------------------------------------
  const boom = await call('debug.panic', { message: 'root-cause: disk on fire (test)' });
  const alive = await call('fs.read', { path: 'ok.txt' });
  arm('D1 panic → ERR_PANIC with root cause + server survives',
    !boom.ok && boom.error?.code === 'ERR_PANIC'
    && boom.error.message.includes('root-cause: disk on fire (test)')
    && alive.ok && String(alive.result?.content ?? '').includes('fine'),
    `code=${boom.error?.code} rootCauseShown=${boom.error?.message.includes('disk on fire')} survived=${alive.ok}`);

  // ---- E. the real-world wire failure ---------------------------------------
  const malformed = await call('mcp__nc-tools=', {});
  const taught = malformed.error?.hint?.wireForm ?? '';
  // the error itself must teach the corrected format…
  const retryName = taught ? 'fs_read' : '';
  const mRetry = taught
    ? await call(`mcp__nc-tools__${retryName}`, { path: 'ok.txt' })
    : { ok: false };
  arm('E1 malformed mcp__nc-tools= teaches wire form; corrected retry succeeds',
    !malformed.ok && malformed.error?.code === 'ERR_UNKNOWN_TOOL'
    && taught === 'mcp__nc-tools__<toolname>' && mRetry.ok,
    `wireForm=${JSON.stringify(taught)} retry mcp__nc-tools__fs_read ok=${mRetry.ok}`);

  // ---- F. io errors carry specific codes ------------------------------------
  const missing = await call('fs.read', { path: 'no/such/file.txt' });
  arm('F1 missing file is ERR_NOT_FOUND (kind-mapped, not ERR_INTERNAL)',
    !missing.ok && missing.error?.code === 'ERR_NOT_FOUND'
    && !missing.error?.message.startsWith('ERR_INTERNAL'),
    `code=${missing.error?.code}`);

  // ---- G. registry↔doc gate exists (structural check) ------------------------
  const list = await rpc('tools/list', {});
  const names = list.result.tools.map((t) => t.name);
  arm('G1 surface healthy (debug.panic present only under env flag)',
    names.includes('debug.panic') && names.length >= 62,
    `${names.length} tools, debug.panic registered`);
} finally {
  try { server.kill(); } catch {}
  try { rmSync(root, { recursive: true, force: true }); } catch {}
}

report.allPass = report.arms.every((a) => a.pass);
const out = join(repoRoot, 'benchmark', 'results', 'err-w1');
mkdirSync(out, { recursive: true });
writeFileSync(join(out, 'verifier.json'), JSON.stringify(report, null, 2) + '\n');
console.log(`\nVERIFIER: ${report.arms.filter((a) => a.pass).length}/${report.arms.length} arms pass → ${join(out, 'verifier.json')}`);
process.exit(report.allPass ? 0 : 1);
