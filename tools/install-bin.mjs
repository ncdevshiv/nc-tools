// Install the static Rust kernel binary to the machine-wide stable path:
//   <user>/.local/bin/nc-tools-mcp.exe
// That dir is on PATH and survives `cargo clean`, so client configs can point
// at it permanently (ZCode user config, nc-cli workspace config). Run after a
// release rebuild:  npm run install:bin
// Verifies the install two ways: a byte hash match against the build (a stale
// or partially-copied binary must fail loudly) and a tool-surface dump from
// the INSTALLED copy.
import { createHash } from 'node:crypto';
import { copyFileSync, mkdirSync, readFileSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');
const exe = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const src = join(repoRoot, 'target', 'release', exe);
const destDir = join(process.env.USERPROFILE || process.env.HOME, '.local', 'bin');
const dest = join(destDir, exe);

const sha256 = (p) => createHash('sha256').update(readFileSync(p)).digest('hex');

const srcStat = statSync(src); // throws if the release build is missing
let stale = false;
let deployedHash = null;
try {
  const destStat = statSync(dest);
  stale = destStat.mtimeMs < srcStat.mtimeMs || sha256(dest) !== sha256(src);
  deployedHash = sha256(dest);
} catch { /* not installed yet */ }

try {
  mkdirSync(destDir, { recursive: true });
  copyFileSync(src, dest);
} catch (e) {
  if (e.code === 'EBUSY') {
    console.error(`dest is locked by a running server — kill it and retry:
  cmd/powershell:   taskkill /F /IM ${exe}
  git bash:         taskkill //F //IM ${exe}
  then re-run:      npm run install:bin`);
    process.exit(1);
  }
  throw e;
}

// Byte-exact proof the copy landed, not just that it executes.
if (sha256(dest) !== sha256(src)) {
  console.error(`install corrupted: src and dest hashes differ after copy
  src:  ${sha256(src)}
  dest: ${sha256(dest)}`);
  process.exit(1);
}

const probe = join(tmpdir(), `nc-install-probe-${process.pid}.json`);
execFileSync(dest, ['--dump-tools', probe], { stdio: ['ignore', 'ignore', 'inherit'] });
const { toolCount } = JSON.parse(readFileSync(probe, 'utf8'));

console.log(`installed: ${dest}${stale ? ' (refreshed stale: was ' + (deployedHash?.slice(0, 16) || 'missing') + '…)' : ' (up to date)'}`);
console.log(`hash: ${sha256(dest)}`);
console.log(`verified: ${toolCount} tools served by the installed binary`);
