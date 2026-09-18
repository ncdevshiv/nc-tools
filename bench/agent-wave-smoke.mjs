// Dr. Invi wave — cross-process smoke + coordination live proofs.
// Spawns the RELEASE MCP binary as an opaque stdio process (exactly how a real
// client drives it) and verifies the five claims this wave introduced. These
// are NOT unit tests — they prove the kernel behaves correctly across process
// boundaries under real conditions (two workspaces, a crash+resume, a
// noticeboard, an advisory lock). No mocks, no in-process kernel shortcuts.
//
// Usage: node bench/agent-wave-smoke.mjs
import { mkdtempSync, writeFileSync, rmSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, '..');
const bin = join(repo, 'target', 'release', process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp');
const outDir = resolve(process.argv[2] || join(repo, 'bench', 'results', 'agent-wave'));

const findings = [];
function report(claim, pass, evidence) {
  findings.push({ claim, pass, evidence });
  console.log(`  [${pass ? 'PASS' : 'FAIL'}] ${claim} — ${evidence}`);
}

function rpc(root) {
  return new Promise((res) => {
    const child = spawn(bin, [root], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    let buf = ''; let id = 0; const pend = new Map();
    child.stdout.on('data', (d) => {
      buf += d.toString('utf8');
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i); buf = buf.slice(i + 1);
        try { const m = JSON.parse(line); if (pend.has(m.id)) { pend.get(m.id)(m); pend.delete(m.id); } } catch {}
      }
    });
    res((method, params) => new Promise((r, j) => {
      const my = ++id;
      const t = setTimeout(() => j(new Error(`${method} timeout`)), 30000);
      pend.set(my, (m) => { clearTimeout(t); r(m); });
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: my, method, params }) + '\n');
    }));
  });
}

async function call(k, name, args) {
  const r = await k('tools/call', { name, arguments: args ?? {} });
  if (r.result?.isError) return { ok: false, error: JSON.parse(r.result.content[0].text).error };
  return { ok: true, result: JSON.parse(r.result.content[0].text) };
}

(async () => {
  console.log('=== Dr. Invi wave — live cross-process smoke ===');
  // ---- C1: git.status baseDir routes to the correct workspace ----
  const A = mkdtempSync(join(tmpdir(), 'gw-a-')); const B = mkdtempSync(join(tmpdir(), 'gw-b-'));
  const ka = await rpc(A); const kb = await rpc(B);
  for (const [k, p] of [[ka, A], [kb, B]]) {
    await k('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '1' } });
    await call(k, 'proc.spawn', { cmd: 'git', args: ['init'], timeoutMs: 20000 });
    writeFileSync(join(p, 'README.md'), 'x\n');
    await call(k, 'proc.spawn', { cmd: 'git', args: ['add', '.'], timeoutMs: 20000 });
    await call(k, 'proc.spawn', { cmd: 'git', args: ['commit', '-m', 'init'], timeoutMs: 20000 });
  }
  const stA = await call(ka, 'git.status', { baseDir: B });
  const stAd = await call(ka, 'git.status', {});
  const c1 = stA.result.repo.includes('gw-b') && stAd.result.repo.includes('gw-a');
  report('git.status baseDir routes to requested workspace (not server root)', c1,
    `baseDir=B -> ${stA.result.repo.split(/[/\\]/).pop()}; default -> ${stAd.result.repo.split(/[/\\]/).pop()}`);
  rmSync(A, { recursive: true, force: true }); rmSync(B, { recursive: true, force: true });

  // ---- C2: proc.spawn cwd routes to baseDir workspace ----
  const PA = mkdtempSync(join(tmpdir(), 'pw-a-')); const PB = mkdtempSync(join(tmpdir(), 'pw-b-'));
  const kpa = await rpc(PA);
  await kpa('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '1' } });
  const pw = await call(kpa, 'proc.spawn', { cmd: 'pwd', args: [], timeoutMs: 20000, baseDir: PB });
  const c2 = pw.result.stdout.trim().toLowerCase().includes('pw-b');
  report('proc.spawn cwd routes to baseDir workspace', c2, `pwd -> ${pw.result.stdout.trim()}`);
  rmSync(PA, { recursive: true, force: true }); rmSync(PB, { recursive: true, force: true });

  // ---- C3: agent identity survives a crash + resume at initialize ----
  const WS = mkdtempSync(join(tmpdir(), 'conti-'));
  const k1 = await rpc(WS);
  await k1('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '1' } });
  const r1 = await call(k1, 'agent.register', { agentId: 'agent-42', name: 'Bob', role: 'dev' });
  const k2 = await rpc(WS); // NEW process == the "crash"
  await k2('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '1', agentId: 'agent-42' } });
  const r2 = await call(k2, 'agent.register', { agentId: 'agent-42' });
  const c3 = r1.result.agentId === 'agent-42' && r2.result.resumed === true && r2.result.agentId === 'agent-42';
  report('identity survives crash + is bound at initialize (clientInfo.agentId)', c3,
    `#1=${r1.result.agentId}(resumed=${r1.result.resumed}); #2=${r2.result.agentId}(resumed=${r2.result.resumed})`);
  rmSync(WS, { recursive: true, force: true });

  // ---- C4: noticeboard message persisted + readable ----
  const WS2 = mkdtempSync(join(tmpdir(), 'notice-'));
  const k4 = await rpc(WS2);
  await k4('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '1' } });
  await call(k4, 'agent.register', { agentId: 'agent-1', name: 'Alice', role: 'researcher' });
  const posted = await call(k4, 'agent.post', { to: 'agent-2', message: 'can you hold on that file?', kind: 'hold' });
  const msgs = await call(k4, 'agent.messages', {});
  const c4 = posted.result.posted === true && msgs.result.messages.length >= 1 && msgs.result.messages[0].message === 'can you hold on that file?';
  report('noticeboard message posted + readable (kind=hold)', c4, `seq=${msgs.result.messages[0]?.seq}`);
  rmSync(WS2, { recursive: true, force: true });

  // ---- C5: advisory lock taken, listed live, released ----
  const WS3 = mkdtempSync(join(tmpdir(), 'lock-'));
  const k5 = await rpc(WS3);
  await k5('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '1' } });
  await call(k5, 'agent.register', { agentId: 'agent-1' });
  const lock = await call(k5, 'agent.lock', { path: 'src/config.rs', holdMs: 60000 });
  const live = await call(k5, 'agent.locks', {});
  const unlocked = await call(k5, 'agent.unlock', { path: 'src/config.rs' });
  const live2 = await call(k5, 'agent.locks', {});
  const c5 = lock.result.agentId === 'agent-1' && live.result.total === 1 && unlocked.result.released === true && live2.result.total === 0;
  report('advisory lock: take -> list live -> release', c5,
    `took=${lock.result.agentId}; live=${live.result.total}; released=${unlocked.result.released}; after=${live2.result.total}`);
  rmSync(WS3, { recursive: true, force: true });

  // ---- artifact ----
  const totalPass = findings.filter((f) => f.pass).length;
  const summary = { ts: new Date().toISOString(), total: findings.length, passed: totalPass, findings };
  console.log(`\n=== AGENT-WAVE: ${totalPass}/${findings.length} live proofs PASS ===`);
  console.log(`artifact: ${join(outDir, 'agent-wave.json')}`);
  const { mkdirSync } = await import('node:fs');
  mkdirSync(outDir, { recursive: true });
  writeFileSync(join(outDir, 'agent-wave.json'), JSON.stringify(summary, null, 2) + '\n');
  process.exit(totalPass === findings.length ? 0 : 1);
})();
