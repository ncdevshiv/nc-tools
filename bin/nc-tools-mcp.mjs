#!/usr/bin/env node
// npx / bun-x entry for nc-tools-mcp (this is what package.json "bin" points
// at). Qwen-class MCP clients only accept npx/uvx as the launch command, so
// this shim routes those clients at the STATIC Rust kernel: it execs the
// release build from this checkout, then the machine-wide install
// (~/.local/bin), and fails with build instructions if neither exists.
import { existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..');
const exe = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const candidates = [
  join(repoRoot, 'target', 'release', exe),
  join(process.env.USERPROFILE || process.env.HOME || '', '.local', 'bin', exe),
];
const bin = candidates.find(existsSync);
if (!bin) {
  console.error(`nc-tools-mcp binary not found (looked in ${candidates.join(', ')})`);
  console.error('Build it: cargo build --release -p nct-mcp   (from the repo root)');
  process.exit(1);
}
const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
process.exit(r.status ?? 1);
