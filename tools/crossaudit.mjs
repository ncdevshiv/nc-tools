// Cross-audit: drives the nc-tools MCP server (spawned as an opaque stdio
// process, exactly like a client would) and verifies the codebase against the
// protocol it exposes. Every check is a real tool call. No direct file reads.
//
// Usage: node tools/crossaudit.mjs [workspaceRoot] [--sandbox-out DIR]
import { mkdtempSync, rmSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = join(here, '..');
const binName = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const serverPath = join(REPO, 'rust', 'target', 'release', binName);
const workspace = process.argv[2] || REPO;

const findings = []; // {check, status: 'PASS'|'FAIL'|'INFO', evidence}
function report(check, status, evidence) {
  findings.push({ check, status, evidence });
  console.log(`  [${status}] ${check} ${evidence ? '— ' + String(evidence).slice(0, 160) : ''}`);
}

function connect(root) {
  const child = spawn(serverPath, [root], { stdio: ['pipe', 'pipe', 'pipe'] });
  child.stderr.on('data', () => {});
  let id = 0;
  const pending = new Map();
  child.stdout.on('data', (buf) => {
    for (const line of buf.toString('utf8').split('\n')) {
      const t = line.trim();
      if (!t) continue;
      let m;
      try { m = JSON.parse(t); } catch { continue; }
      const p = pending.get(m.id);
      if (p) { pending.delete(m.id); p(m); }
    }
  });
  const rpc = (method, params) => new Promise((res, rej) => {
    const myId = ++id;
    const timer = setTimeout(() => rej(new Error(`${method} timeout`)), 30_000);
    pending.set(myId, (m) => { clearTimeout(timer); res(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: myId, method, params }) + '\n');
  });
  const close = () => { try { child.stdin.end(); } catch {}; };
  return { rpc, close };
}

(async () => {
  const client = connect(workspace);
  const call = async (name, args) => {
    const r = await client.rpc('tools/call', { name, arguments: args ?? {} });
    if (r.result?.isError) return { ok: false, error: JSON.parse(r.result.content[0].text).error };
    return { ok: true, result: JSON.parse(r.result.content[0].text) };
  };

  console.log('=== MCP cross-audit:', workspace, '===');

  try {
    // ---- 1. protocol contract ------------------------------------------------
    const init = await client.rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'audit', version: '1' } });
    const proto = init.result?.protocolVersion === '2024-11-05' && init.result?.serverInfo?.name === 'nc-tools';
    report('protocol handshake (2024-11-05, nc-tools)', proto ? 'PASS' : 'FAIL', JSON.stringify(init.result?.serverInfo));

    const list = await client.rpc('tools/list', {});
    const tools = list.result.tools;
    const expected = ['batch.execute','code.symbols','env.get','env.list','env.set','fs.append','fs.copy','fs.delete','fs.list','fs.mkdir','fs.move','fs.read','fs.readMany','fs.readRange','fs.stat','fs.tree','fs.write','fs.writeMany','git.add','git.blame','git.branch','git.checkout','git.commit','git.diff','git.log','git.pull','git.push','git.status','net.fetch','net.http','net.probePort','net.robots','net.search','patch.apply','patch.applyMany','pkg.add','pkg.list','pkg.runScript','pkg.scripts','proc.kill','proc.list','proc.readOutput','proc.runScript','proc.spawn','proc.start','proc.status','proc.stop','proc.watch','search.files','search.grep','search.replace','search.semantic','sys.doctor','sys.journal','sys.listSnapshots','sys.rollback','sys.snapshot','sys.workspace','test.run','text.diff'];
    report('tool count matches expected list', tools.length === expected.length ? 'PASS' : 'FAIL', `expected ${expected.length}, got ${tools.length}`);
    const noSchemas = tools.filter((t) => !t.inputSchema || t.inputSchema.type !== 'object' || !t.description);
    report('all tools have schema + description', noSchemas.length === 0 ? 'PASS' : 'FAIL', noSchemas.map((t) => t.name).join(','));
    const names = tools.map((t) => t.name).sort();
    const missing = expected.filter((n) => !names.includes(n));
    report('tool names match PROTOCOL.md list', missing.length === 0 ? 'PASS' : 'FAIL', missing.join(',') || `all ${expected.length} present`);

    // ---- 2. live behavior in a sandbox ---------------------------------------
    const sandbox = mkdtempSync(join(tmpdir(), 'nc-audit-'));
    const s = connect(sandbox);
    const sc = async (name, args) => {
      const r = await s.rpc('tools/call', { name, arguments: args ?? {} });
      return { isError: !!r.result?.isError, data: JSON.parse(r.result.content[0].text), raw: r.result };
    };
    await s.rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'audit', version: '1' } });

    // gateway checks: fs round-trip, patch guard, path escape, search, git, proc, journal
    const w = await sc('fs.write', { path: 'a.txt', content: 'hello audit' });
    const r = await sc('fs.read', { path: 'a.txt' });
    report('fs.write+fs.read round-trip', !w.isError && !r.isError && r.data.content.includes('hello audit') ? 'PASS' : 'FAIL', '');
    const miss = await sc('fs.read', { path: 'missing-xyz.txt' });
    report('ERR_NOT_FOUND + nearestExisting hint', miss.isError && miss.data.error.code === 'ERR_NOT_FOUND' && Array.isArray(miss.data.error.hint.nearestExisting) ? 'PASS' : 'FAIL', JSON.stringify(miss.data.error?.code));
    const escPath = join(tmpdir(), `nc-audit-out-${Date.now()}.txt`);
    const esc = await sc('fs.write', { path: escPath, content: 'x' });
    report('absolute path outside base dir is writable (global tool system)', !esc.isError ? 'PASS' : 'FAIL', JSON.stringify(esc.data.error?.code));
    rmSync(escPath, { force: true });
    await sc('fs.write', { path: 'patch.js', content: 'const x = 1;\nconst y = 2;\n' });
    const p1 = await sc('patch.apply', { path: 'patch.js', edits: [{ oldText: 'const x = 1;', newText: 'const x = 10;' }] });
    const p2 = await sc('patch.apply', { path: 'patch.js', edits: [{ oldText: 'nope', newText: 'x' }] });
    report('patch.apply exact + PATCH_NO_MATCH hints', !p1.isError && p2.isError && p2.data.error.code === 'PATCH_NO_MATCH' ? 'PASS' : 'FAIL', '');
    const g = await sc('search.grep', { pattern: 'const x' });
    report('search.grep file/line/text', !g.isError && g.data.total >= 1 ? 'PASS' : 'FAIL', `total=${g.data.total}`);
    const unk = await sc('bash.run', { script: 'rm -rf /' });
    report('ERR_UNKNOWN_TOOL (no shell tool exists)', unk.isError && unk.data.error.code === 'ERR_UNKNOWN_TOOL' && unk.data.error.hint.available.includes('fs.write') ? 'PASS' : 'FAIL', '');
    const sp = await sc('proc.spawn', { cmd: 'node', args: ['-e', 'console.log("spawn-ok")'] });
    report('proc.spawn typed argv + exit code', !sp.isError && sp.data.exitCode === 0 && sp.data.stdout.includes('spawn-ok') ? 'PASS' : 'FAIL', '');
    const ju = await sc('sys.journal', { lastN: 200 });
    const jcalls = ju.data.events.filter((e) => e.kind === 'tool.call');
    const jresults = ju.data.events.filter((e) => e.kind === 'tool.result');
    const orphans = jresults.filter((e) => !jcalls.some((c) => c.seq === e.callSeq));
    report('journal call/result pairing (no orphan callSeq)', orphans.length === 0 ? 'PASS' : 'FAIL', `calls=${jcalls.length} results=${jresults.length} orphans=${orphans.length}`);
    const snap = await sc('sys.snapshot', { label: 'pre' });
    await sc('fs.write', { path: 'b.txt', content: 'new file' });
    await sc('sys.rollback', { id: snap.data.id });
    const bGone = await sc('fs.stat', { path: 'b.txt' });
    report('snapshot+rollback removes post-snapshot files', !bGone.isError && bGone.data.exists === false ? 'PASS' : 'FAIL', '');
    const batch = await sc('batch.execute', { calls: [{ tool: 'sys.workspace', args: {} }, { tool: 'fs.read', args: { path: 'nope.txt' } }] });
    report('batch.execute per-item ok/failed', !batch.isError && batch.data.ok === 1 && batch.data.failed === 1 ? 'PASS' : 'FAIL', `ok=${batch.data.ok} failed=${batch.data.failed}`);
    // git in sandbox (init + status + commit)
    const gitInit = spawn('git', ['init'], { cwd: sandbox });
    await new Promise((res) => gitInit.on('close', res));
    spawn('git', ['config', 'user.email', 'audit@x'], { cwd: sandbox });
    spawn('git', ['config', 'user.name', 'audit'], { cwd: sandbox });
    await sc('fs.write', { path: 'g.txt', content: 'x' });
    const st = await sc('git.status', {});
    report('git.status on real repo', !st.isError && Array.isArray(st.data.files) ? 'PASS' : 'FAIL', `files=${st.data.files?.length}`);
    await sc('git.add', { paths: ['g.txt'] });
    const cm = await sc('git.commit', { message: 'audit commit' });
    report('git add+commit round-trip', !cm.isError && typeof cm.data.sha === 'string' && cm.data.sha.length >= 7 ? 'PASS' : 'FAIL', `sha=${cm.data.sha?.slice(0, 8)}`);

    // ---- 3. codebase hygiene via the tools themselves (no direct reads) ------
    // cross-check the docs claim against code surfaced through search
    const todo = await call('search.grep', { pattern: '\\bTODO\\b|\\bFIXME\\b|\\bXXX\\b', path: 'rust/crates', maxResults: 50 });
    report('no TODO/FIXME/XXX in rust/crates/', !todo.isError && todo.result.total === 0 ? 'PASS' : 'FAIL', `total=${todo.result.total}`);
    const mock = await call('search.grep', { pattern: '\\bplaceholder\\b|\\bnot implemented\\b|\\bto-implement\\b', path: 'rust/crates', maxResults: 50 });
    report('no placeholder/not-implemented markers in rust/crates/', !mock.isError && mock.result.total === 0 ? 'PASS' : 'FAIL', `total=${mock.result.total}`);
    // docs claim check
    const doc = await call('search.grep', { pattern: `${expected.length} tools`, path: 'docs' });
    report(`docs claim "${expected.length} tools" present`, !doc.isError && doc.result.total >= 1 ? 'PASS' : 'INFO', `hits=${doc.result.total}`);
    // spec/tool cross-check: PROTOCOL.md tool count statement
    const protoDoc = await call('search.grep', { pattern: `${expected.length} tools`, path: 'docs/PROTOCOL.md', maxResults: 5 });
    report('PROTOCOL.md tool-count consistent', !protoDoc.isError && protoDoc.result.total >= 1 ? 'PASS' : 'FAIL', protoDoc.isError ? JSON.stringify(protoDoc.error) : protoDoc.result.matches.map((m) => m.text.slice(0, 60)).join(' | ') || '(no matching line)');

    s.close();
    await new Promise((res) => setTimeout(res, 400));
    rmSync(sandbox, { recursive: true, force: true });
  } catch (e) {
    report('audit run', 'FAIL', e.message);
  } finally {
    client.close();
  }

  const failed = findings.filter((f) => f.status === 'FAIL');
  console.log(`\n=== CROSS-AUDIT: ${findings.length - failed.length}/${findings.length} PASS ===`);
  if (failed.length) {
    for (const f of failed) console.log(`  FAIL: ${f.check} ${f.evidence}`);
    process.exit(1);
  }
})();
