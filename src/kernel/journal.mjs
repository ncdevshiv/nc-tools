// Append-only JSONL journal. Every tool call + result lands here.
// Cross-process safe: multiple kernel instances (parallel agents) on the same
// workspace share one journal file; writes are serialized with an exclusive
// lock file so no line is ever torn interleaved.
import { mkdirSync, appendFileSync, readFileSync, existsSync, openSync, closeSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';

function withLock(lockPath, fn) {
  // exclusive-create lock; retry briefly; guarantees one writer at a time
  const deadline = Date.now() + 5000;
  let fd = null;
  while (fd === null) {
    try {
      fd = openSync(lockPath, 'wx');
    } catch {
      if (Date.now() > deadline) {
        throw new Error(`journal lock timeout: ${lockPath} held too long`);
      }
      // bounded spin (5ms) — atomic appends are short
      const spin = new Date().getTime() % 5 + 1;
      const end = Date.now() + spin;
      while (Date.now() < end) { /* busy wait */ }
    }
  }
  try {
    return fn();
  } finally {
    try { closeSync(fd); rmSync(lockPath, { force: true }); } catch { /* best effort */ }
  }
}

export class Journal {
  constructor(filePath) {
    this.filePath = filePath;
    this.seq = 0;
    this.lockPath = join(dirname(filePath), '.journal.lock');
    mkdirSync(dirname(filePath), { recursive: true });
    // Resume sequence numbering if a journal already exists (continuity).
    if (existsSync(filePath)) {
      try {
        const lines = readFileSync(filePath, 'utf8').split('\n').filter(Boolean);
        const last = lines.length ? JSON.parse(lines[lines.length - 1]) : null;
        this.seq = last?.seq ?? 0;
      } catch { this.seq = 0; }
    }
  }

  /** @returns {object} the event that was written */
  append(kind, fields) {
    this.seq += 1;
    const event = { ts: new Date().toISOString(), seq: this.seq, kind, ...fields };
    const line = JSON.stringify(event) + '\n';
    // single atomic line write under lock — no torn lines across processes
    withLock(this.lockPath, () => appendFileSync(this.filePath, line, 'utf8'));
    return event;
  }

  readAll() {
    if (!existsSync(this.filePath)) return [];
    return readFileSync(this.filePath, 'utf8')
      .split('\n').filter(Boolean)
      .map((line) => { try { return JSON.parse(line); } catch { return null; } })
      .filter(Boolean);
  }

  lastN(n) {
    const all = this.readAll();
    return n === undefined ? all : all.slice(-n);
  }
}
