// Append-only JSONL journal. Every tool call + result lands here.
import { mkdirSync, appendFileSync, readFileSync, existsSync } from 'node:fs';
import { dirname } from 'node:path';

export class Journal {
  constructor(filePath) {
    this.filePath = filePath;
    this.seq = 0;
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
    appendFileSync(this.filePath, JSON.stringify(event) + '\n', 'utf8');
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
