// Install the static Rust kernel binary to the machine-wide stable path:
//   <user>/.local/bin/nc-tools-mcp.exe
// That dir is on PATH and survives `cargo clean`, so client configs can point
// at it permanently (ZCode user config, nc-cli workspace config). Run after a
// release rebuild:  npm run install:bin
// Verifies the install by dumping the tool surface from the INSTALLED copy.
import { copyFileSync, mkdirSync, readFileSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');
const exe = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const src = join(repoRoot, 'rust', 'target', 'release', exe);
const destDir = join(process.env.USERPROFILE || process.env.HOME, '.local', 'bin');
const dest = join(destDir, exe);

const srcStat = statSync(src); // throws if the release build is missing
let stale = false;
try {
  stale = statSync(dest).mtimeMs < srcStat.mtimeMs;
} catch { /* not installed yet */ }

try {
  mkdirSync(destDir, { recursive: true });
  copyFileSync(src, dest);
} catch (e) {
  if (e.code === 'EBUSY') {
    console.error(`dest is locked by a running server — kill it and retry:
  taskkill //F //IM ${exe}`);
    process.exit(1);
  }
  throw e;
}

const probe = join(tmpdir(), `nc-install-probe-${process.pid}.json`);
execFileSync(dest, ['--dump-tools', probe], { stdio: ['ignore', 'ignore', 'inherit'] });
const { toolCount } = JSON.parse(readFileSync(probe, 'utf8'));

console.log(`installed: ${dest}${stale ? ' (refreshed)' : ''}`);
console.log(`verified: ${toolCount} tools served by the installed binary`);
