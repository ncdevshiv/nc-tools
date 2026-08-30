// git.* tools — thin typed wrappers over the git CLI (porcelain formats only).
// Git is an external program; the kernel's job is to turn its output into data.
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { ToolError } from './errors.mjs';

function git(root, args, { input } = {}) {
  const r = spawnSync('git', args, {
    cwd: root,
    input,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    timeout: 60_000,
  });
  if (r.error) throw new ToolError('ERR_GIT_SPAWN', `git failed to start: ${r.error.message}`);
  if (r.status !== 0) {
    throw new ToolError('ERR_GIT', `git ${args[0]} failed (exit ${r.status}): ${(r.stderr || r.stdout || '').trim().slice(0, 500)}`,
      { args, stderr: (r.stderr || '').slice(0, 2000) });
  }
  return r.stdout;
}

export function makeGitTools(root) {
  const available = () => existsSync(`${root}/.git`);

  const status = () => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    const out = git(root, ['status', '--porcelain=v1', '-b']);
    const lines = out.split('\n').filter(Boolean);
    const branch = lines[0]?.startsWith('## ') ? lines[0].slice(3).split('...')[0].trim() : null;
    const files = lines.slice(1).map((l) => ({
      status: l.slice(0, 2).trim() || '?',
      path: l.slice(3).trim(),
    })).filter((f) => !f.path.startsWith('.nc-tools'));
    let head = null;
    try { head = git(root, ['rev-parse', '--short', 'HEAD']).trim(); } catch { /* empty repo */ }
    return { branch, head, files };
  };

  const diff = ({ path } = {}) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    const args = ['diff', '--no-color'];
    if (path) args.push('--', path);
    return { diff: git(root, args) };
  };

  const add = ({ paths }) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    if (!Array.isArray(paths) || paths.length === 0) throw new ToolError('ERR_BAD_INPUT', 'paths must be a non-empty array');
    git(root, ['add', '--', ...paths]);
    return { added: paths };
  };

  const commit = ({ message }) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    if (typeof message !== 'string' || !message.trim()) throw new ToolError('ERR_BAD_INPUT', 'message required');
    git(root, ['commit', '-m', message]);
    const sha = git(root, ['rev-parse', '--short', 'HEAD']).trim();
    return { sha, message };
  };

  const log = ({ maxCount = 20 } = {}) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    const out = git(root, ['log', `--max-count=${maxCount}`, '--pretty=format:%H%x1f%an%x1f%aI%x1f%s']);
    const commits = out.split('\n').filter(Boolean).map((l) => {
      const [sha, author, date, subject] = l.split('\x1f');
      return { sha, author, date, message: subject };
    });
    return { commits };
  };

  return {
    'git.status': { handler: status },
    'git.diff': { handler: diff },
    'git.add': { handler: add },
    'git.commit': { handler: commit },
    'git.log': { handler: log },
  };
}
