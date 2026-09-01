#!/usr/bin/env node
// npx / bun-x entry for nc-tools-mcp (this is what package.json "bin" points
// at). Qwen-class MCP clients only accept npx/uvx as the launch command, so
// this shim is the seam that routes those clients at the STATIC Rust kernel:
// it execs the release binary when present and only falls back to the archived
// JS oracle on a fresh checkout where cargo has never built.
import { existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..');
const exe = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const candidates = [
  join(repoRoot, 'rust', 'target', 'release', exe),
  join(process.env.USERPROFILE || process.env.HOME || '', '.local', 'bin', exe),
];
const bin = candidates.find(existsSync);
if (bin) {
  const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
  process.exit(r.status ?? 1);
}
await import(join(repoRoot, 'oracle', 'mcp', 'server.mjs'));
