// Verifier validator: proves every benchmark task's verifier is correct.
// For each task it runs three checks on a FRESH workspace:
//   1. untouched           → verifier must say pass=false
//   2. correct solution    → verifier must say pass=true
//   3. wrong solution      → verifier must say pass=false (for tasks with wrongSols)
// A task whose verifier fails any check is reported; the gate is ALL tasks pass.
import { mkdtempSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { Kernel } from '../src/kernel/kernel.mjs';
import { tasks } from './tasks.mjs';
import { solutions, wrongSolutions, canonicalAnswers } from './solutions.mjs';

function setupWorkspace(root, task) {
  const files = task.setup(root) || {};
  for (const [path, content] of Object.entries(files)) {
    const abs = join(root, path);
    mkdirSync(join(abs, '..'), { recursive: true });
    writeFileSync(abs, content, 'utf8');
  }
}
function gitInit(root) {
  spawnSync('git', ['init'], { cwd: root });
  spawnSync('git', ['config', 'user.email', 'bench@nc-tools.local'], { cwd: root });
  spawnSync('git', ['config', 'user.name', 'nc-tools bench']);
}

const results = [];
for (const task of tasks) {
  const row = { id: task.id, difficulty: task.difficulty ?? '?', language: task.language ?? '?' };
  try {
    // 1. untouched
    let root = mkdtempSync(join(tmpdir(), `val-${task.id}-a-`));
    gitInit(root);
    setupWorkspace(root, task);
    let kernel = new Kernel(root);
    let v = await task.verify(root, kernel, '', 'kernel');
    row.untouched = { pass: v.pass === true, evidence: String(v.evidence || '').slice(0, 80) };
    rmSync(root, { recursive: true, force: true });

    if (solutions[task.id]) {
      // 2. correct solution
      root = mkdtempSync(join(tmpdir(), `val-${task.id}-b-`));
      gitInit(root);
      setupWorkspace(root, task);
      kernel = new Kernel(root);
      await solutions[task.id](root, kernel);
      const answerText = canonicalAnswers[task.id] || `${task.id} done with correct impl`;
      v = await task.verify(root, kernel, answerText, 'kernel');
      row.solved = { pass: v.pass === true, evidence: String(v.evidence || '').slice(0, 80) };
      rmSync(root, { recursive: true, force: true });

      // 3. wrong solution (if provided)
      if (wrongSolutions[task.id]) {
        root = mkdtempSync(join(tmpdir(), `val-${task.id}-c-`));
        gitInit(root);
        setupWorkspace(root, task);
        kernel = new Kernel(root);
        await wrongSolutions[task.id](root, kernel);
        v = await task.verify(root, kernel, `${task.id} done with WRONG impl`, 'kernel');
        row.wrongRejected = { pass: v.pass !== true, evidence: String(v.evidence || '').slice(0, 80) };
        rmSync(root, { recursive: true, force: true });
      }
    } else {
      row.solved = { pass: false, note: 'no canonical solution defined' };
    }
  } catch (e) {
    row.error = String(e.message).slice(0, 200);
  }
  results.push(row);
  // report now so failures are visible even mid-run
  const flat = `${row.error ? 'ERR: ' + row.error : ''}${row.untouched?.pass === undefined ? '' : row.untouched.pass ? ' [untouched!should-be-false]' : ''}${row.solved?.pass === undefined ? '' : row.solved.pass ? '' : ' [solved!FAIL]'}${row.wrongRejected?.pass === undefined ? '' : row.wrongRejected.pass ? '' : ' [wrongRejected!FAIL]'}`;
  console.log(`${task.id.padEnd(24)} untouched=${row.untouched?.pass ?? '?'} solved=${row.solved?.pass ?? '?'} wrongRejected=${row.wrongRejected?.pass ?? 'n/a'}${flat ? ' ' + flat : ''}`);
}

const failed = results.filter((r) => r.error || r.untouched?.pass === true || r.solved?.pass !== true || (r.wrongRejected !== undefined && r.wrongRejected.pass !== true));
console.log(`\nVALIDATOR: ${results.length - failed.length}/${results.length} verifiers proven correct`);
if (failed.length) {
  console.log('FAILING:', failed.map((f) => f.id).join(', '));
  process.exit(1);
}
