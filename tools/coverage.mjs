// Terminal-taxonomy coverage tracker. This is the metric for the project's
// core claim: the terminal can be replaced entirely by typed tools. Each row
// maps a class of terminal work to the nc-tools driver that covers it.
// Run: node tools/coverage.mjs [--json]
//
// "covered"   = a typed driver returns structured data for this class
// "partial"   = a driver exists but with real limitations (documented)
// "gap"       = no driver yet; a harness would still need bash here

const TAXONOMY = [
  { category: 'Read files', commands: ['cat', 'head', 'tail', 'less'], driver: 'fs.read / fs.readMany', status: 'covered', note: 'line-numbered, paginated, digest to prevent re-reads' },
  { category: 'Write files', commands: ['echo >', 'tee'], driver: 'fs.write / fs.writeMany', status: 'covered', note: 'created/overwrote reported' },
  { category: 'List / find files', commands: ['ls', 'find', 'fd'], driver: 'fs.list / fs.stat / search.files', status: 'covered' },
  { category: 'Search content', commands: ['grep', 'rg'], driver: 'search.grep', status: 'covered', note: 'file+line+text, result caps' },
  { category: 'Edit files', commands: ['sed -i', 'perl -pi', 'manual rewrites'], driver: 'patch.apply / patch.applyMany', status: 'covered', note: 'exact-match with ambiguity guard + nearest-line hints' },
  { category: 'Move / copy / delete', commands: ['mv', 'cp', 'rm'], driver: 'fs.move / fs.delete', status: 'covered', note: 'workspace-jail enforced' },
  { category: 'Directories', commands: ['mkdir', 'rmdir'], driver: 'fs.mkdir', status: 'covered' },
  { category: 'Version control (git)', commands: ['git status/add/commit/log/diff'], driver: 'git.*', status: 'covered', note: 'porcelain formats parsed; branch/push/pull/merge not yet typed' },
  { category: 'One-shot programs', commands: ['any program invocation'], driver: 'proc.spawn', status: 'covered', note: 'typed argv, no shell, timeout, captured output' },
  { category: 'Long-running processes', commands: ['server in a tab', '& background jobs', 'watch'], driver: 'proc.start/status/readOutput/stop', status: 'covered', note: 'managed handles with streamed output' },
  { category: 'Test runners', commands: ['node --test', 'pytest'], driver: 'test.run', status: 'covered', note: 'structured counts + failing test identities (junit-based)' },
  { category: 'Package managers', commands: ['npm install/ls/run', 'pip install/list'], driver: 'pkg.add/list/scripts/runScript', status: 'partial', note: 'npm+pip typed; cargo/pnpm/uv/gem not yet' },
  { category: 'HTTP requests', commands: ['curl', 'wget', 'httpie'], driver: 'net.http', status: 'covered', note: 'status/headers/body capped; no multipart/streaming yet' },
  { category: 'Port / connectivity checks', commands: ['nc -z', 'netstat', 'ss'], driver: 'net.probePort', status: 'covered' },
  { category: 'Environment variables', commands: ['export', 'env'], driver: 'env.get/set/list', status: 'covered', note: 'session-scoped, inherited by all proc calls' },
  { category: 'Batching / scripting glue', commands: ['&&', '|', ';'], driver: 'batch.execute', status: 'covered', note: 'sequential batch with per-item results; no pipes by design' },
  { category: 'Build systems', commands: ['make', 'tsc', 'cargo build', 'gradle'], driver: 'proc.spawn (generic)', status: 'partial', note: 'runnable but no typed build-graph driver yet' },
  { category: 'Archives', commands: ['tar', 'zip', 'unzip'], driver: null, status: 'gap' },
  { category: 'File permissions / ownership', commands: ['chmod', 'chown', 'attrib'], driver: null, status: 'gap' },
  { category: 'Users / processes admin', commands: ['ps', 'kill -9 <pid>', 'id'], driver: 'proc.status/stop (own handles only)', status: 'partial', note: 'cannot inspect processes the kernel did not start' },
  { category: 'Scheduling / daemons', commands: ['cron', 'systemd', 'at'], driver: null, status: 'gap' },
  { category: 'Encryption / SSH', commands: ['ssh', 'gpg', 'openssl'], driver: null, status: 'gap' },
  { category: 'Containers / VMs', commands: ['docker', 'podman'], driver: null, status: 'gap' },
  { category: 'Cloud CLIs', commands: ['aws', 'gcloud', 'az'], driver: null, status: 'gap' },
  { category: 'Interactive / TUI programs', commands: ['vim', 'htop', 'debuggers'], driver: null, status: 'gap', note: 'deliberately out of scope for non-interactive agents' },
  { category: 'Watch files for changes', commands: ['watch', 'inotifywait', 'nodemon'], driver: null, status: 'gap', note: 'poll via fs.stat digests today; event stream planned' },
];

const rows = TAXONOMY.map((r) => ({ ...r }));
const covered = rows.filter((r) => r.status === 'covered').length;
const partial = rows.filter((r) => r.status === 'partial').length;
const gaps = rows.filter((r) => r.status === 'gap').length;
const total = rows.length;

function report() {
  console.log(`nc-tools terminal-replacement coverage: ${covered} covered + ${partial} partial of ${total} command classes`);
  console.log(`weighted coverage: ${Math.round(((covered + partial * 0.5) / total) * 100)}% | pure: ${Math.round((covered / total) * 100)}% | open gaps: ${gaps}\n`);
  for (const r of rows) {
    const mark = r.status === 'covered' ? '[x]' : r.status === 'partial' ? '[~]' : '[ ]';
    console.log(`${mark} ${r.category.padEnd(34)} ${r.driver ?? '—'} ${r.note ? '— ' + r.note : ''}`);
  }
}

if (process.argv.includes('--json')) {
  console.log(JSON.stringify({
    total, covered, partial, gaps,
    weightedCoverage: Math.round(((covered + partial * 0.5) / total) * 100),
    pureCoverage: Math.round((covered / total) * 100),
    rows,
  }, null, 2));
} else {
  report();
}
