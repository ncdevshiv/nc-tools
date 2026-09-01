// Golden-spec export: freezes the CURRENT protocol surface (tool descriptors)
// as JSON so any reimplementation (Rust/Go/...) can be diffed against it
// structurally. Run: node tools/golden.mjs   → conformance/golden/tools.json
// The golden file is committed: a change to the surface must be a deliberate,
// reviewed golden update, never silent drift.
// Phase 2: the golden is regenerated from the trusted RUST binary (the JS
// oracle is archived under oracle/), so the Rust kernel is the source of truth.
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');
const outDir = join(repoRoot, 'conformance', 'golden');
mkdirSync(outDir, { recursive: true });

const binName = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
// Pick whichever binary is NEWER: a stale debug binary silently regenerating
// the golden from old code is worse than a stale release one (the release
// rebuild is what `npm run build:rust` refreshes). Report which one was used.
const candidates = [
  join(repoRoot, 'rust', 'target', 'debug', binName),
  join(repoRoot, 'rust', 'target', 'release', binName),
];
const bin = candidates
  .filter((p) => existsSync(p))
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
if (!bin) {
  throw new Error('Rust binary not found — run `npm run build:rust` or `cargo build -p nct-mcp` in rust/ first');
}
console.log(`golden source binary: ${bin}`);

const tmp = join(outDir, '.golden.tmp.json');
execFileSync(bin, ['--dump-tools', tmp], { cwd: repoRoot, stdio: ['ignore', 'ignore', 'inherit'] });
const golden = JSON.parse(readFileSync(tmp, 'utf8'));
writeFileSync(join(outDir, 'tools.json'), JSON.stringify(golden, null, 2) + '\n');
console.log(`golden spec written: ${outDir}/tools.json (${golden.toolCount} tools from ${golden.generatedFrom})`);
