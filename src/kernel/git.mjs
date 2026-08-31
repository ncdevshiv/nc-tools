// git.* tools — thin typed wrappers over the git CLI (porcelain formats only).
// Git is an external program; the kernel's job is to turn its output into data.
// Each tool accepts `repo` (default: the base dir) so remote agents can work
// on ANY repository on the machine, not just the server's own.
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { ToolError } from './errors.mjs';
import { resolvePath } from './paths.mjs';

function git(dir, args, { input } = {}) {
  const r = spawnSync('git', args, {
    cwd: dir,
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
  /** Resolve the repo dir for a call; must actually be a git repository. */
  const inRepo = (repo) => {
    const r = resolvePath(root, repo ?? '.');
    if (!existsSync(`${r}/.git`)) {
      throw new ToolError('ERR_NOT_A_REPO', `not a git repository: ${r}`, { repo: r });
    }
    return r;
  };

  const status = ({ repo } = {}) => {
    const r = inRepo(repo);
    const out = git(r, ['status', '--porcelain=v1', '-b']);
    const lines = out.split('\n').filter(Boolean);
    const branch = lines[0]?.startsWith('## ') ? lines[0].slice(3).split('...')[0].trim() : null;
    const files = lines.slice(1).map((l) => ({
      status: l.slice(0, 2).trim() || '?',
      path: l.slice(3).trim(),
    })).filter((f) => f.path !== '.nc-tools' && !f.path.startsWith('.nc-tools/'));
    let head = null;
    try { head = git(r, ['rev-parse', '--short', 'HEAD']).trim(); } catch { /* empty repo */ }
    return { repo: r, branch, head, files };
  };

  const diff = ({ path, repo } = {}) => {
    const r = inRepo(repo);
    const args = ['diff', '--no-color'];
    if (path) args.push('--', path);
    return { repo: r, diff: git(r, args) };
  };

  const add = ({ paths, repo } = {}) => {
    const r = inRepo(repo);
    if (!Array.isArray(paths) || paths.length === 0) throw new ToolError('ERR_BAD_INPUT', 'paths must be a non-empty array');
    git(r, ['add', '--', ...paths]);
    return { repo: r, added: paths };
  };

  const commit = ({ message, repo } = {}) => {
    const r = inRepo(repo);
    if (typeof message !== 'string' || !message.trim()) throw new ToolError('ERR_BAD_INPUT', 'message required');
    git(r, ['commit', '-m', message]);
    const sha = git(r, ['rev-parse', '--short', 'HEAD']).trim();
    return { repo: r, sha, message };
  };

  const log = ({ maxCount = 20, repo } = {}) => {
    const r = inRepo(repo);
    const out = git(r, ['log', `--max-count=${maxCount}`, '--pretty=format:%H%x1f%an%x1f%aI%x1f%s']);
    const commits = out.split('\n').filter(Boolean).map((l) => {
      const [sha, author, date, subject] = l.split('\x1f');
      return { sha, author, date, message: subject };
    });
    return { repo: r, commits };
  };

  const branch = ({ name, repo } = {}) => {
    const r = inRepo(repo);
    if (name !== undefined) {
      if (typeof name !== 'string' || !name.trim()) throw new ToolError('ERR_BAD_INPUT', 'branch name required');
      git(r, ['branch', name.trim()]);
      return { repo: r, branch: name.trim(), created: true };
    }
    const out = git(r, ['branch', '--list']);
    const lines = out.split('\n').filter(Boolean);
    const branches = lines.map((l) => ({
      name: l.replace(/^\*\s+/, '').trim(),
      current: l.trim().startsWith('*'),
    }));
    return {
      repo: r,
      branches,
      current: branches.find((b) => b.current)?.name ?? null,
    };
  };

  const checkout = ({ branch: name, create = false, repo } = {}) => {
    const r = inRepo(repo);
    if (typeof name !== 'string' || !name.trim()) throw new ToolError('ERR_BAD_INPUT', 'branch name required');
    git(r, create ? ['checkout', '-b', name.trim()] : ['checkout', name.trim()]);
    return { repo: r, branch: name.trim(), created: create };
  };

  const push = ({ remote = 'origin', branch: name, setUpstream = true, repo } = {}) => {
    const r = inRepo(repo);
    const args = ['push', ...(name && setUpstream ? ['-u'] : []), remote, ...(name ? [name] : [])];
    const out = git(r, args);
    return { repo: r, remote, branch: name ?? null, upstream: !!(name && setUpstream), output: out.trim().split('\n').filter(Boolean) };
  };

  const pull = ({ remote = 'origin', branch: name, ffOnly = true, repo } = {}) => {
    const r = inRepo(repo);
    const args = ['pull', '--no-edit', ...(ffOnly ? ['--ff-only'] : []), remote, ...(name ? [name] : [])];
    const out = git(r, args);
    return { repo: r, remote, branch: name ?? null, output: out.trim().split('\n').filter(Boolean) };
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
