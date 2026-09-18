// Assert the MACHINE-WIDE installed binary is the one just built.
//
// Every other harness in this repo drives target/release (or builds the kernel
// in-process), so a rebuild followed by `npm test && npm run conform &&
// npm run crossaudit` passes fully green even when the installed binary at
// ~/.local/bin — the path client configs point at — is still the old one.
// That is exactly how a stale-server session gets produced, so this is a gate.
//
// Run:  npm run deployed
import { createHash } from 'node:crypto';
import { readFileSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');
const exe = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const built = join(repoRoot, 'target', 'release', exe);
const destDir = join(process.env.USERPROFILE || process.env.HOME, '.local', 'bin');
const deployed = join(destDir, exe);

function sha256(p) {
  return createHash('sha256').update(readFileSync(p)).digest('hex');
}

function toolCount(p) {
  const probe = join(tmpdir(), `nc-deployed-probe-${process.pid}.json`);
  try {
    execFileSync(p, ['--dump-tools', probe], { stdio: ['ignore', 'ignore', 'ignore'] });
    return JSON.parse(readFileSync(probe, 'utf8')).toolCount;
  } catch {
    return null;
  }
}

let failed = false;
const note = (s) => console.log(s);
const bad = (s) => { failed = true; console.error(s); };

if (!statSync(built, { throwIfNoEntry: false })) {
  bad(`built binary missing: ${built} (run: npm run build)`);
  process.exit(1);
}

const builtStat = statSync(built);
if (!statSync(deployed, { throwIfNoEntry: false })) {
  bad(`installed binary missing: ${deployed}
  nothing in this repo installs it, so every test above passed against
  target/release while clients would serve the wrong thing. Run:
    npm run install:bin   # EBUSY means a server is running — kill it first`);
  process.exit(1);
}

const builtHash = sha256(built);
const deployedStat = statSync(deployed);
note(`built:    ${built}  ${builtHash.slice(0, 16)}…  ${builtStat.size}B  ${new Date(builtStat.mtimeMs).toISOString()}`);

if (builtHash === sha256(deployed)) {
  note(`deployed: ${deployed}  ${builtHash.slice(0, 16)}…  matches the build`);
  const n = toolCount(deployed);
  if (n !== null) note(`verified: ${n} tools served by the installed binary`);
  process.exit(0);
}

bad(`STALE: installed binary does not match the build.
  built:    ${builtHash}
  deployed: ${sha256(deployed)}
  deployed mtime: ${new Date(deployedStat.mtimeMs).toISOString()} (built: ${new Date(builtStat.mtimeMs).toISOString()})
  clients pointed at ~/.local/bin are serving the older binary while every
  gate in this repo passed green. Fix: npm run install:bin`);
process.exit(1);
