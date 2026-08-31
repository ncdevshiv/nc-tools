// proc.* — execution tools. proc.spawn for bounded runs; proc.start creates a
// MANAGED background process (handle, streamed output, status, stop) — the
// typed replacement for "run a server / watcher in a terminal tab". proc.list
// and proc.kill cover the OS process table (tasklist/ps + pid kill).
import { spawn, spawnSync } from 'node:child_process';
import { ToolError } from './errors.mjs';
import { resolvePath } from './paths.mjs';

const MAX_BUFFER = 2_000_000;

/** Parse the OS process table into [{pid, name, memKb?}]. */
function systemProcesses() {
  if (process.platform === 'win32') {
    const r = spawnSync('tasklist', ['/FO', 'CSV', '/NH'], { encoding: 'utf8', timeout: 20_000, windowsHide: true });
    if (r.error) {
      if (r.error.code === 'ENOENT') throw new ToolError('ERR_CMD_NOT_FOUND', 'tasklist is not available');
      throw new ToolError('ERR_SPAWN', `tasklist failed: ${r.error.message}`);
    }
    return (r.stdout || '')
      .split('\n').filter((l) => l.includes('","')).map((line) => {
        const cols = line.trim().replace(/^"|"$/g, '').split('","');
        const name = cols[0], pid = Number(cols[1]);
        const memKb = Number((cols[4] ?? '').replace(/[^\d]/g, ''));
        return { pid, name, memKb: Number.isFinite(memKb) ? memKb : null };
      }).filter((p) => Number.isInteger(p.pid) && p.pid > 0);
  }
  const r = spawnSync('ps', ['-A', '-o', 'pid=,comm='], { encoding: 'utf8', timeout: 20_000 });
  if (r.error) {
    if (r.error.code === 'ENOENT') throw new ToolError('ERR_CMD_NOT_FOUND', 'ps is not available');
    throw new ToolError('ERR_SPAWN', `ps failed: ${r.error.message}`);
  }
  return (r.stdout || '')
    .split('\n').map((l) => l.trim()).filter(Boolean).map((l) => {
      const m = l.match(/^(\d+)\s+(.+)$/);
      return m ? { pid: Number(m[1]), name: m[2].trim() } : null;
    }).filter((p) => p && Number.isInteger(p.pid) && p.pid > 0);
}

export function makeProcTools(root, sessionEnv) {
  /** @type {Map<string, object>} handleId -> record */
  const handles = new Map();
  let handleSeq = 0;

  const childEnv = () => ({ ...process.env, ...Object.fromEntries(sessionEnv) });

  const spawnTool = async ({ cmd, args = [], cwd = '.', timeoutMs = 120_000, maxOutputBytes = MAX_BUFFER }) => {
    if (typeof cmd !== 'string' || cmd.length === 0) throw new ToolError('ERR_BAD_INPUT', 'cmd must be a non-empty string');
    if (!Array.isArray(args) || args.some((a) => typeof a !== 'string')) {
      throw new ToolError('ERR_BAD_INPUT', 'args must be an array of strings (typed argv — no shell)');
    }
    if (!Number.isInteger(timeoutMs) || timeoutMs < 100 || timeoutMs > 600_000) {
      throw new ToolError('ERR_BAD_INPUT', 'timeoutMs must be an integer between 100 and 600000', { got: timeoutMs });
    }
    const cwdAbs = resolvePath(root, cwd);

    return await new Promise((resolvePromise) => {
      let stdout = '';
      let stderr = '';
      let timedOut = false;
      let settled = false;
      let child;
      try {
        child = spawn(cmd, args, { cwd: cwdAbs, shell: false, windowsHide: true, env: childEnv() });
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
    if (!Number.isInteger(maxDurationMs) || maxDurationMs < 1000 || maxDurationMs > 3_600_000) {
      throw new ToolError('ERR_BAD_INPUT', 'maxDurationMs must be an integer between 1000 and 3600000', { got: maxDurationMs });
    }
    const cwdAbs = resolvePath(root, cwd);
    let child;
    try {
      child = spawn(cmd, args, { cwd: cwdAbs, shell: false, windowsHide: true, env: childEnv() });
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
      if (rec.timer) { clearTimeout(rec.timer); rec.timer = null; }
    });
    child.on('close', (code, signal) => {
      rec.running = false;
      rec.exitCode = code;
      rec.signal = signal;
      // a lingering maxDuration timer would keep the event loop alive (and the
      // session/process) after the child is gone
      if (rec.timer) { clearTimeout(rec.timer); rec.timer = null; }
    });
    rec.timer = setTimeout(() => {
      if (rec.running) { rec.timedOut = true; try { child.kill('SIGKILL'); } catch { /* dead */ } }
    }, maxDurationMs);
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

  const list = ({ filter, maxResults = 500 } = {}) => {
    if (filter !== undefined && typeof filter !== 'string') throw new ToolError('ERR_BAD_INPUT', 'filter must be a string');
    if (!Number.isInteger(maxResults) || maxResults < 1 || maxResults > 2000) {
      throw new ToolError('ERR_BAD_INPUT', 'maxResults must be an integer between 1 and 2000', { got: maxResults });
    }
    let procs = systemProcesses();
    if (filter) {
      const needle = filter.toLowerCase();
      procs = procs.filter((p) => p.name.toLowerCase().includes(needle));
    }
    return { processes: procs.slice(0, maxResults), total: procs.length, truncated: procs.length > maxResults };
  };

  const kill = ({ pid, force = true }) => {
    if (!Number.isInteger(pid) || pid <= 0) throw new ToolError('ERR_BAD_INPUT', 'pid must be a positive integer', { got: pid });
    try {
      process.kill(pid, force ? 'SIGKILL' : 'SIGTERM');
      return { pid, signal: force ? 'SIGKILL' : 'SIGTERM', requested: true };
    } catch (e) {
      if (e.code === 'ESRCH') throw new ToolError('ERR_PROC_NOT_FOUND', `no process with pid ${pid}`, { pid });
      if (e.code === 'EPERM') throw new ToolError('ERR_REFUSED', `permission denied killing pid ${pid}`, { pid });
      throw new ToolError('ERR_SPAWN', `kill failed: ${e.message}`, { pid });
    }
  };

  return {
    'proc.spawn': { handler: spawnTool },
    'proc.start': { handler: start },
    'proc.status': { handler: status },
    'proc.readOutput': { handler: readOutput },
    'proc.stop': { handler: stop },
    'proc.list': { handler: list },
    'proc.kill': { handler: kill },
  };
}
