// proc.spawn — the ONLY execution tool. Typed argv (no shell), hard timeout,
// captured stdout/stderr. For compilers, test runners, build tools.
import { spawn } from 'node:child_process';
import { resolve } from 'node:path';
import { ToolError } from './errors.mjs';
import { inWorkspace } from './paths.mjs';

export function makeProcTools(root) {
  const spawnTool = async ({ cmd, args = [], cwd = '.', timeoutMs = 120_000, maxOutputBytes = 2_000_000 }) => {
    if (typeof cmd !== 'string' || cmd.length === 0) throw new ToolError('ERR_BAD_INPUT', 'cmd must be a non-empty string');
    if (!Array.isArray(args) || args.some((a) => typeof a !== 'string')) {
      throw new ToolError('ERR_BAD_INPUT', 'args must be an array of strings (typed argv — no shell)');
    }
    const cwdAbs = inWorkspace(root, cwd);
    const cmdAbs = cmd.includes('/') || cmd.includes('\\') ? inWorkspace(root, cmd) : cmd;

    return await new Promise((resolvePromise) => {
      let stdout = '';
      let stderr = '';
      let timedOut = false;
      let settled = false;
      let child;
      try {
        child = spawn(cmdAbs, args, { cwd: cwdAbs, shell: false, windowsHide: true });
      } catch (e) {
        throw new ToolError('ERR_SPAWN', `failed to spawn ${cmd}: ${e.message}`);
      }
      const timer = setTimeout(() => {
        timedOut = true;
        try { child.kill('SIGKILL'); } catch { /* already dead */ }
      }, timeoutMs);

      child.stdout?.on('data', (d) => { if (stdout.length < maxOutputBytes) stdout += d.toString('utf8'); });
      child.stderr?.on('data', (d) => { if (stderr.length < maxOutputBytes) stderr += d.toString('utf8'); });
      child.on('error', (e) => {
        if (settled) return;
        settled = true; clearTimeout(timer);
        if (e.code === 'ENOENT') {
          resolvePromise({ pid: null, exitCode: null, signal: null, timedOut: false, stdout, stderr,
            error: { code: 'ERR_CMD_NOT_FOUND', message: `command not found: ${cmd}` } });
        } else {
          resolvePromise({ pid: child.pid ?? null, exitCode: null, signal: null, timedOut: false, stdout, stderr,
            error: { code: 'ERR_SPAWN', message: e.message } });
        }
      });
      child.on('close', (code, signal) => {
        if (settled) return;
        settled = true; clearTimeout(timer);
        resolvePromise({
          pid: child.pid ?? null,
          exitCode: code,
          signal,
          timedOut,
          stdout: stdout.slice(0, maxOutputBytes),
          stderr: stderr.slice(0, maxOutputBytes),
          error: undefined,
        });
      });
    });
  };

  return { 'proc.spawn': { handler: spawnTool } };
}
