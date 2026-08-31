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
    })).filter((f) => f.path !== '.nc-tools' && !f.path.startsWith('.nc-tools/'));
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

  const branch = ({ name } = {}) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    if (name !== undefined) {
      if (typeof name !== 'string' || !name.trim()) throw new ToolError('ERR_BAD_INPUT', 'branch name required');
      git(root, ['branch', name.trim()]);
      return { branch: name.trim(), created: true };
    }
    const out = git(root, ['branch', '--list']);
    const lines = out.split('\n').filter(Boolean);
    const branches = lines.map((l) => ({
      name: l.replace(/^\*\s+/, '').trim(),
      current: l.trim().startsWith('*'),
    }));
    return {
      branches,
      current: branches.find((b) => b.current)?.name ?? null,
    };
  };

  const checkout = ({ branch: name, create = false }) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    if (typeof name !== 'string' || !name.trim()) throw new ToolError('ERR_BAD_INPUT', 'branch name required');
    git(root, create ? ['checkout', '-b', name.trim()] : ['checkout', name.trim()]);
    return { branch: name.trim(), created: create };
  };

  const push = ({ remote = 'origin', branch: name, setUpstream = true }) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    const args = ['push', ...(name && setUpstream ? ['-u'] : []), remote, ...(name ? [name] : [])];
    const out = git(root, args);
    return { remote, branch: name ?? null, upstream: !!(name && setUpstream), output: out.trim().split('\n').filter(Boolean) };
  };

  const pull = ({ remote = 'origin', branch: name, ffOnly = true }) => {
    if (!available()) throw new ToolError('ERR_NOT_A_REPO', 'workspace is not a git repository');
    const args = ['pull', '--no-edit', ...(ffOnly ? ['--ff-only'] : []), remote, ...(name ? [name] : [])];
    const out = git(root, args);
    return { remote, branch: name ?? null, output: out.trim().split('\n').filter(Boolean) };
  };

  return {
    'git.status': { handler: status },
    'git.diff': { handler: diff },
    'git.add': { handler: add },
    'git.commit': { handler: commit },
    'git.log': { handler: log },
    'git.branch': { handler: branch },
    'git.checkout': { handler: checkout },
    'git.push': { handler: push },
    'git.pull': { handler: pull },
  };
}
