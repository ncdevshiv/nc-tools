// Golden-spec export: freezes the CURRENT protocol surface (tool descriptors)
// as JSON so any reimplementation (Rust/Go/...) can be diffed against it
// structurally. Run: node tools/golden.mjs   → conformance/golden/tools.json
// The golden file is committed: a change to the surface must be a deliberate,
// reviewed golden update, never silent drift.
import { mkdirSync, writeFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { toolDescriptors } from '../oracle/kernel/descriptors.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, '..', 'conformance', 'golden');
mkdirSync(outDir, { recursive: true });

const descriptors = toolDescriptors();
const golden = {
  generatedFrom: 'src/kernel/descriptors.mjs',
  toolCount: descriptors.length,
  tools: descriptors.map((t) => ({ name: t.name, description: t.description, inputSchema: t.inputSchema })),
};

const outPath = join(outDir, 'tools.json');
writeFileSync(outPath, JSON.stringify(golden, null, 2) + '\n');
console.log(`golden spec written: ${outPath} (${golden.toolCount} tools)`);
