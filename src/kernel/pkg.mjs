// pkg.* — typed package-ecosystem drivers (npm, pip). Installing, listing,
// scripts — without shelling into package-manager CLI semantics ad hoc.
import { spawnSync } from 'node:child_process';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { ToolError } from './errors.mjs';
import { resolvePath } from './paths.mjs';
import { childEnv } from './childenv.mjs';

// Windows: npm is a .CMD shim; spawnSync rejects .cmd without a shell (EINVAL)
// and nc-tools never uses shells. Run npm-cli.js under the current node instead.
let npmCliPath = null;
function resolveNpmCli() {
  if (npmCliPath) return npmCliPath;
  const exeDir = dirname(process.execPath);
  const candidates = [
    join(exeDir, 'node_modules', 'npm', 'bin', 'npm-cli.js'),
    join(exeDir, '..', 'lib', 'node_modules', 'npm', 'bin', 'npm-cli.js'),
  ];
  for (const c of candidates) {
    if (existsSync(c)) { npmCliPath = c; return c; }
  }
  throw new ToolError('ERR_CMD_NOT_FOUND', 'npm-cli.js not found next to the running node executable');
}

function run(root, cmd, args, timeoutMs = 300_000) {
  let exe = cmd;
  let argv = args;
  if (cmd === 'npm') {
    exe = process.execPath;
    argv = [resolveNpmCli(), ...args];
  }
  const r = spawnSync(exe, argv, { cwd: root, encoding: 'utf8', timeout: timeoutMs, maxBuffer: 64 * 1024 * 1024, windowsHide: true, env: childEnv() });
  if (r.error) {
    if (r.error.code === 'ENOENT') throw new ToolError('ERR_CMD_NOT_FOUND', `${cmd} is not available`);
    throw new ToolError('ERR_SPAWN', `${cmd} failed: ${r.error.message}`);
  }
  return r;
}
const NETWORK_HINTS = ['ENOTFOUND', 'ETIMEDOUT', 'ECONNREFUSED', 'EAI_AGAIN', 'network', 'ECONNRESET'];

export function makePkgTools(root) {
  const add = ({ manager = 'npm', names, dev = false, timeoutMs = 300_000, dir }) => {
    if (!Array.isArray(names) || names.length === 0 || names.some((n) => typeof n !== 'string')) {
      throw new ToolError('ERR_BAD_INPUT', 'names must be a non-empty array of strings');
    }
    const d = resolvePath(root, dir ?? '.');
    if (manager === 'npm') {
      const args = ['install', '--no-audit', '--no-fund', '--loglevel=error', ...(dev ? ['--save-dev'] : []), ...names];
      const r = run(d, 'npm', args, timeoutMs);
      if (r.status !== 0) {
        const errTail = (r.stderr || r.stdout || '').slice(-400);
        const isNet = NETWORK_HINTS.some((h) => errTail.toLowerCase().includes(h.toLowerCase()));
        throw new ToolError(isNet ? 'ERR_NETWORK' : 'ERR_PKG', `npm install failed (exit ${r.status})`,
          { names, stderrTail: errTail, hint: isNet ? 'registry unreachable from this machine' : undefined });
      }
      return { manager, installed: names, dev };
    }
    if (manager === 'pip') {
      const r = run(d, 'python', ['-m', 'pip', 'install', ...names], timeoutMs);
      if (r.status !== 0) {
        const errTail = (r.stderr || r.stdout || '').slice(-400);
        const isNet = NETWORK_HINTS.some((h) => errTail.toLowerCase().includes(h.toLowerCase()));
        throw new ToolError(isNet ? 'ERR_NETWORK' : 'ERR_PKG', `pip install failed (exit ${r.status})`,
          { names, stderrTail: errTail });
      }
      return { manager, installed: names };
    }
    throw new ToolError('ERR_BAD_INPUT', `unsupported manager: ${manager} (supported: npm, pip)`);
  };

  const list = ({ manager = 'npm', dir } = {}) => {
    const d = resolvePath(root, dir ?? '.');
    if (manager === 'npm') {
      if (!existsSync(join(d, 'package.json'))) {
        return { manager, packages: [], note: 'no package.json in workspace' };
      }
      const r = run(d, 'npm', ['ls', '--json', '--depth=0']);
      let parsed;
      try { parsed = JSON.parse(r.stdout || '{}'); } catch {
        throw new ToolError('ERR_PKG', 'npm ls produced unparseable output');
      }
      const packages = Object.entries(parsed.dependencies ?? {}).map(([name, info]) => ({
        name, version: info.version ?? null, missing: !!info.missing, problems: info.problems ?? undefined,
      }));
      return { manager, packages, total: packages.length };
    }
    if (manager === 'pip') {
      const r = run(d, 'python', ['-m', 'pip', 'list', '--format', 'json']);
      let parsed;
      try { parsed = JSON.parse(r.stdout || '[]'); } catch {
        throw new ToolError('ERR_PKG', 'pip list produced unparseable output');
      }
      return { manager, packages: parsed, total: parsed.length };
    }
    throw new ToolError('ERR_BAD_INPUT', `unsupported manager: ${manager}`);
  };

  const scripts = ({ dir } = {}) => {
    const d = resolvePath(root, dir ?? '.');
    const pj = join(d, 'package.json');
    if (!existsSync(pj)) throw new ToolError('ERR_NOT_FOUND', 'no package.json in workspace', { path: 'package.json' });
    let parsed;
    try { parsed = JSON.parse(readFileSync(pj, 'utf8')); } catch (e) {
      throw new ToolError('ERR_PARSE', `package.json is not valid JSON: ${e.message}`);
    }
    return { scripts: parsed.scripts ?? {} };
  };

  const runScript = ({ name, args = [], timeoutMs = 300_000, dir }) => {
    if (typeof name !== 'string' || !name) throw new ToolError('ERR_BAD_INPUT', 'script name required');
    if (!Array.isArray(args) || args.some((a) => typeof a !== 'string')) {
      throw new ToolError('ERR_BAD_INPUT', 'args must be an array of strings');
    }
    const d = resolvePath(root, dir ?? '.');
    const r = run(d, 'npm', ['run', name, '--', ...args], timeoutMs);
    return {
      script: name, exitCode: r.status,
      stdout: (r.stdout || '').slice(-100_000),
      stderr: (r.stderr || '').slice(-50_000),
      ok: r.status === 0,
    };
  };

  return {
    'pkg.add': { handler: add },
    'pkg.list': { handler: list },
    'pkg.scripts': { handler: scripts },
    'pkg.runScript': { handler: runScript },
  };
}
