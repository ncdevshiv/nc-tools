// End-to-end probe of the foreign-lock guard through the INSTALLED binary,
// across TWO real server processes (so each has its own agent identity).
// Process A locks a path; process B, in the same workspace, must be refused by
// guardLocks. No baseDir is passed anywhere — that is the default path the
// guard used to fail silently on.
import { spawn } from 'node:child_process';
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const bin = join(process.env.USERPROFILE || process.env.HOME, '.local', 'bin', 'nc-tools-mcp.exe');
const ws = join(process.env.USERPROFILE, '.local', 'tmp', 'nct-e2e-guard');

let nextId = 10;

function server(workspace) {
  const child = spawn(bin, [workspace], { stdio: ['pipe', 'pipe', 'inherit'] });
  let buf = '';
  const pending = new Map();
  child.stdout.on('data', (d) => {
    buf += d.toString();
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!line) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      if (msg.id && pending.has(msg.id)) {
        pending.get(msg.id)(msg);
        pending.delete(msg.id);
      }
    }
  });
  const send = (method, params, notify = false) => new Promise((resolve, reject) => {
    const msg = { jsonrpc: '2.0', method, params };
    if (!notify) msg.id = nextId++;
    child.stdin.write(JSON.stringify(msg) + '\n');
    if (notify) return resolve();
    pending.set(msg.id, resolve);
    setTimeout(() => reject(new Error(`timeout waiting for ${method}`)), 20000);
  });
  const init = async () => {
    await send('initialize', {
      protocolVersion: '2024-11-05',
      clientInfo: { name: 'e2e-guard-probe', version: '1.0.0' },
    });
    await send('notifications/initialized', undefined, true);
  };
  const call = async (name, arguments_) => {
    const r = await send('tools/call', { name, arguments: arguments_ });
    const text = r.result?.content?.[0]?.text ?? '';
    let body;
    try { body = JSON.parse(text); } catch { body = text; }
    return { isError: !!r.result?.isError, body };
  };
  return { child, init, call, close: () => child.kill('SIGKILL') };
}

const results = [];
const check = (name, ok, detail) => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS  ' : 'FAIL  '} ${name}${detail ? ` — ${detail}` : ''}`);
};

rmSync(ws, { recursive: true, force: true });
mkdirSync(join(ws, '.nc-tools'), { recursive: true });
writeFileSync(join(ws, 'a.txt'), 'original\n');

const a = server(ws);
const b = server(ws);
try {
  await a.init();
  await b.init();

  const lock = await a.call('agent.lock', { path: 'a.txt', holdMs: 600000 });
  check('A locks a.txt', !lock.isError, JSON.stringify(lock.body)?.slice(0, 120));

  const refused = await b.call('fs.write', { path: 'a.txt', content: 'clobbered', guardLocks: true });
  check(
    'B is refused by the guard (no baseDir)',
    refused.isError && /ERR_REFUSED/.test(JSON.stringify(refused.body)),
    JSON.stringify(refused.body)?.slice(0, 200),
  );

  const file = readFileSync(join(ws, 'a.txt'), 'utf8');
  check('locked file untouched', file === 'original\n', JSON.stringify(file));

  // Advisory mode: the write proceeds and names the holder.
  const advisory = await b.call('fs.write', { path: 'b.txt', content: 'x' });
  check('B writes an unlocked path', !advisory.isError, JSON.stringify(advisory.body)?.slice(0, 120));

  // A's own lock is not foreign: A can still write its own file.
  const own = await a.call('fs.write', { path: 'a.txt', content: 'own write', guardLocks: true });
  check('A writes through its own lock', !own.isError, JSON.stringify(own.body)?.slice(0, 160));

  const hash = await b.call('search.replace', {
    pattern: 'own',
    replacement: 'edited',
    path: 'a.txt',
    dryRun: false,
    guardLocks: true,
  });
  check(
    'search.replace.guardLocks refuses a locked file',
    hash.isError && /ERR_REFUSED/.test(JSON.stringify(hash.body)),
    JSON.stringify(hash.body)?.slice(0, 200),
  );

  const copy = await b.call('fs.copy', { from: 'a.txt', to: 'a.txt' });
  check('fs.copy self-path refused', copy.isError && /ERR_BAD_INPUT/.test(JSON.stringify(copy.body)), JSON.stringify(copy.body)?.slice(0, 160));
} catch (e) {
  console.log('probe error:', e.message);
} finally {
  a.close();
  b.close();
  rmSync(ws, { recursive: true, force: true });
}

const failed = results.filter((r) => !r.ok);
console.log(failed.length === 0 ? `\nE2E GUARD: ${results.length}/${results.length} passed against ${bin}` : `\nE2E GUARD: ${failed.length} FAILED`);
process.exit(failed.length === 0 ? 0 : 1);
