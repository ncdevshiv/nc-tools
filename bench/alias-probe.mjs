// Probe the response shapes the wire-alias conformance case asserts on:
// underscore-form tool names (fs_stat, sys__workspace) resolve like dotted ones.
// Usage: node bench/alias-probe.mjs [<path-to-nc-tools-mcp>]
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');
const exe = process.argv[2]
  || join(repoRoot, 'target', 'release', process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp');

const ws = mkdtempSync(join(tmpdir(), 'nc-alias-probe-'));
const child = spawn(exe, [ws], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
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
    try { const m = JSON.parse(l); const cb = pending.get(m.id); if (cb) { pending.delete(m.id); cb(m); } } catch {}
  }
});
const rpc = (method, params) => new Promise((r) => {
  const my = ++id;
  pending.set(my, r);
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: my, method, params }) + '\n');
});

await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'probe', version: '1' } });
await rpc('tools/call', { name: 'fs.write', arguments: { path: 'a.txt', content: 'abc' } });
const s = await rpc('tools/call', { name: 'fs_stat', arguments: { path: 'a.txt' } });
console.log('fs_stat:', s.result.content[0].text.slice(0, 240).replaceAll('\n', ' '));
const w = await rpc('tools/call', { name: 'sys__workspace', arguments: {} });
console.log('sys__workspace:', w.result.content[0].text.slice(0, 240).replaceAll('\n', ' '));
child.kill();
rmSync(ws, { recursive: true, force: true });
