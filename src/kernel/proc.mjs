// proc.* — execution tools. proc.spawn for bounded runs; proc.start creates a
// MANAGED background process (handle, streamed output, status, stop) — the
// typed replacement for "run a server / watcher in a terminal tab".
import { spawn } from 'node:child_process';
import { ToolError } from './errors.mjs';
import { inWorkspace } from './paths.mjs';

const MAX_BUFFER = 2_000_000;

export function makeProcTools(root, sessionEnv) {
  /** @type {Map<string, object>} handleId -> record */
  const handles = new Map();
  let handleSeq = 0;

  const childEnv = () => ({ ...process.env, ...Object.fromEntries(sessionEnv) });

  const resolveCmd = (cmd) => (cmd.includes('/') || cmd.includes('\\') ? inWorkspace(root, cmd) : cmd);

  const spawnTool = async ({ cmd, args = [], cwd = '.', timeoutMs = 120_000, maxOutputBytes = MAX_BUFFER }) => {
    if (typeof cmd !== 'string' || cmd.length === 0) throw new ToolError('ERR_BAD_INPUT', 'cmd must be a non-empty string');
    if (!Array.isArray(args) || args.some((a) => typeof a !== 'string')) {
      throw new ToolError('ERR_BAD_INPUT', 'args must be an array of strings (typed argv — no shell)');
    }
    const cwdAbs = inWorkspace(root, cwd);

    return await new Promise((resolvePromise) => {
      let stdout = '';
      let stderr = '';
      let timedOut = false;
      let settled = false;
      let child;
      try {
        child = spawn(resolveCmd(cmd), args, { cwd: cwdAbs, shell: false, windowsHide: true, env: childEnv() });
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

  // ---- managed background processes ---------------------------------------
  const start = ({ cmd, args = [], cwd = '.', maxDurationMs = 600_000 }) => {
    if (typeof cmd !== 'string' || cmd.length === 0) throw new ToolError('ERR_BAD_INPUT', 'cmd must be a non-empty string');
    if (!Array.isArray(args) || args.some((a) => typeof a !== 'string')) {
      throw new ToolError('ERR_BAD_INPUT', 'args must be an array of strings (typed argv — no shell)');
    }
    const cwdAbs = inWorkspace(root, cwd);
    let child;
    try {
      child = spawn(resolveCmd(cmd), args, { cwd: cwdAbs, shell: false, windowsHide: true, env: childEnv() });
    } catch (e) {
      throw new ToolError('ERR_SPAWN', `failed to start ${cmd}: ${e.message}`);
    }
    handleSeq += 1;
    const handleId = `h${handleSeq}`;
    const rec = {
      handleId, cmd, args, cwd: cwdAbs, pid: child.pid ?? null,
      startedAt: Date.now(), running: true, exitCode: null, signal: null,
      output: '', maxDurationMs, child, timedOut: false,
    };
    child.stdout?.on('data', (d) => { if (rec.output.length < MAX_BUFFER) rec.output += d.toString('utf8'); });
    child.stderr?.on('data', (d) => { if (rec.output.length < MAX_BUFFER) rec.output += d.toString('utf8'); });
    child.on('error', (e) => {
      rec.running = false;
      rec.spawnError = e.code === 'ENOENT' ? `command not found: ${cmd}` : e.message;
    });
    child.on('close', (code, signal) => {
      rec.running = false;
      rec.exitCode = code;
      rec.signal = signal;
    });
    if (maxDurationMs > 0 && maxDurationMs <= 3_600_000) {
      rec.timer = setTimeout(() => {
        if (rec.running) { rec.timedOut = true; try { child.kill('SIGKILL'); } catch { /* dead */ } }
      }, maxDurationMs);
    }
    handles.set(handleId, rec);
    return { handleId, pid: rec.pid, startedAt: new Date(rec.startedAt).toISOString() };
  };

  const status = ({ handleId }) => {
    const rec = handles.get(handleId);
    if (!rec) throw new ToolError('ERR_UNKNOWN_HANDLE', `no such process handle: ${handleId}`, { known: [...handles.keys()] });
    return {
      handleId, pid: rec.pid, cmd: rec.cmd, args: rec.args,
      running: rec.running, exitCode: rec.exitCode, signal: rec.signal,
      timedOut: rec.timedOut, spawnError: rec.spawnError,
      outputBytes: rec.output.length,
      uptimeMs: rec.running ? Date.now() - rec.startedAt : null,
    };
  };

  const readOutput = ({ handleId, fromEnd = 4000 }) => {
    const rec = handles.get(handleId);
    if (!rec) throw new ToolError('ERR_UNKNOWN_HANDLE', `no such process handle: ${handleId}`, { known: [...handles.keys()] });
    const total = rec.output.length;
    const output = total <= fromEnd ? rec.output : rec.output.slice(-fromEnd);
    return { handleId, output, totalBytes: total, truncated: total > fromEnd, running: rec.running };
  };

  const stop = ({ handleId, force = true }) => {
    const rec = handles.get(handleId);
    if (!rec) throw new ToolError('ERR_UNKNOWN_HANDLE', `no such process handle: ${handleId}`, { known: [...handles.keys()] });
    if (rec.timer) clearTimeout(rec.timer);
    if (rec.running) {
      try { rec.child.kill(force ? 'SIGKILL' : 'SIGTERM'); } catch { /* dead already */ }
    }
    // On Windows kill() is async-ish; report current knowledge, caller can re-status.
    return { handleId, requested: true, wasRunning: rec.running };
  };

  return {
    'proc.spawn': { handler: spawnTool },
    'proc.start': { handler: start },
    'proc.status': { handler: status },
    'proc.readOutput': { handler: readOutput },
    'proc.stop': { handler: stop },
  };
}
