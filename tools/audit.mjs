// Repository audit: scans source for stub/fake/mock/placeholder patterns and
// TODO markers. Exit code 1 if findings, 0 if clean. This is the tool the
// project uses to prove "no stubs, no fakes".
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const SKIP_DIRS = new Set(['node_modules', '.git', '.nc-tools', 'benchmark', 'coverage', 'target']);
const SCAN_EXT = new Set(['.mjs', '.js', '.cjs', '.ts', '.json', '.md', '.rs', '.toml']);

// Patterns that indicate unfinished/fake work in shipped source.
const PATTERNS = [
  { re: /\bTODO\b/, label: 'TODO marker' },
  { re: /\bFIXME\b/, label: 'FIXME marker' },
  { re: /\bHACK\b/, label: 'HACK marker' },
  { re: /\bXXX\b/, label: 'XXX marker' },
  { re: /\bnot implemented\b/i, label: '"not implemented"' },
  { re: /\bunimplemented\b/i, label: '"unimplemented"' },
  { re: /\btodo!\s*\(/, label: 'rust todo! macro' },
  { re: /\bunimplemented!\s*\(/, label: 'rust unimplemented! macro' },
  { re: /throw new Error\(\s*['"`](STUB|PLACEHOLDER)/i, label: 'stub error throw' },
  { re: /\bstub(s)?\b/i, label: '"stub"' },
  { re: /\bplaceholder\b/i, label: '"placeholder"' },
  { re: /\bdummy\b/i, label: '"dummy"' },
  { re: /\bcoming soon\b/i, label: '"coming soon"' },
  { re: /\bfuture work\b/i, label: '"future work"' },
  { re: /\bfor brevity\b/i, label: '"for brevity"' },
  { re: /\bleft as an exercise\b/i, label: '"left as an exercise"' },
];

// Whitelisted substrings: lines matching these are allowed even if a pattern hits.
// (e.g. the audit tool's own pattern table, or prose *describing* the policy)
const ALLOW = [
  /tools[\\/]audit\.mjs$/,          // the auditor itself
  /tools[\\/]crossaudit\.mjs$/,     // auditor #2 — it searches FOR these markers
  /AUDIT\.md$/,                     // audit report — lists what was searched for
  /SPEC\.md$/,                      // spec deliberately documents exclusions
  /README\.md$/,                    // policy statement section
];

function* walk(dir) {
  for (const name of readdirSync(dir).sort()) {
    if (SKIP_DIRS.has(name)) continue;
    const full = join(dir, name);
    let st;
    try { st = statSync(full); } catch { continue; }
    if (st.isDirectory()) yield* walk(full);
    else if (SCAN_EXT.has(name.slice(name.lastIndexOf('.')))) yield full;
  }
}

const findings = [];
let filesScanned = 0;
for (const file of walk(repoRoot)) {
  const rel = relative(repoRoot, file).replaceAll('\\', '/');
  filesScanned += 1;
  if (ALLOW.some((a) => a.test(rel))) continue;
  const lines = readFileSync(file, 'utf8').split('\n');
  lines.forEach((line, i) => {
    for (const { re, label } of PATTERNS) {
      if (re.test(line)) {
        findings.push({ file: rel, line: i + 1, label, text: line.trim().slice(0, 160) });
        break; // one finding per line
      }
    }
  });
}

console.log(`Scanned ${filesScanned} files under ${repoRoot}`);
if (findings.length === 0) {
  console.log('AUDIT CLEAN: no stubs, TODOs, placeholders, fakes, or unfinished markers found.');
  process.exit(0);
} else {
  console.log(`AUDIT FAILED: ${findings.length} finding(s):`);
  for (const f of findings) console.log(`  ${f.file}:${f.line} [${f.label}] ${f.text}`);
  process.exit(1);
}
