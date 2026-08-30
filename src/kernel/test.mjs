// test.* — typed test-runner driver. Returns structured results (counts +
// failing test identities), not stdout text. node:test uses its built-in
// junit reporter; pytest uses --junitxml (built-in). One parser for both.
import { spawnSync } from 'node:child_process';
import { readFileSync, rmSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { ToolError } from './errors.mjs';
import { inWorkspace } from './paths.mjs';

function decodeEntities(s) {
  return s
    .replaceAll('&#10;', '\n').replaceAll('&#13;', '\r').replaceAll('&#9;', '\t')
    .replaceAll('&quot;', '"').replaceAll('&apos;', "'")
    .replaceAll('&lt;', '<').replaceAll('&gt;', '>')
    .replaceAll('&amp;', '&');
}

function parseJunitXml(xml, framework) {
  let passed = 0, failed = 0, errors = 0, skipped = 0, durationMs = 0;
  const failures = [];
  const cases = xml.match(/<testcase\b[\s\S]*?(?:<\/testcase>|\/>)/g) ?? [];
  for (const c of cases) {
    // \b so we don't match inside classname="..."
    const name = decodeEntities((c.match(/\bname="([^"]*)"/) ?? [])[1] ?? 'unknown');
    const time = parseFloat((c.match(/time="([\d.]+)"/) ?? [])[1] ?? '0') * 1000;
    durationMs += time;
    const childFailure = /<failure\b/.test(c) || /<error\b/.test(c);
    const attrFailure = /\bfailure="/.test(c) && !childFailure ? (c.match(/\bfailure="([^"]*)"/) ?? [])[1] : null;
    if (childFailure || attrFailure != null) {
      failed += 1;
      let message = attrFailure;
      if (childFailure) {
        const childMsg = (c.match(/<(?:failure|error)[^>]*\bmessage="([^"]*)"/) ?? [])[1];
        message = childMsg ?? ((c.match(/<(?:failure|error)[^>]*>([\s\S]*?)<\/(?:failure|error)>/) ?? [])[1] ?? '');
      }
      failures.push({
        name,
        file: (c.match(/\bfile="([^"]*)"/) ?? [])[1]?.split(/[\\/]/).slice(-3).join('/') ?? null,
        message: decodeEntities(message ?? 'failure').trim().split('\n')[0].slice(0, 300),
      });
    } else if (/<skipped\b/.test(c)) skipped += 1;
    else passed += 1;
  }
  return { framework, passed, failed, errors, skipped, total: passed + failed + errors + skipped, durationMs: Math.round(durationMs), failures };
}

function runSuite(root, cmd, args, timeoutMs) {
  // Strip NODE_TEST_CONTEXT: node sets it for its own test children, and an
  // inherited value makes the spawned runner think it is nested and skip files.
  const env = { ...process.env };
  delete env.NODE_TEST_CONTEXT;
  const r = spawnSync(cmd, args, { cwd: root, encoding: 'utf8', timeout: timeoutMs, maxBuffer: 64 * 1024 * 1024, windowsHide: true, env });
  if (r.error) {
    if (r.error.code === 'ENOENT') throw new ToolError('ERR_CMD_NOT_FOUND', `${cmd} is not available`);
    throw new ToolError('ERR_SPAWN', `${cmd} failed: ${r.error.message}`);
  }
  return r;
}

export function makeTestTools(root) {
  const run = ({ framework = 'node', path, timeoutMs = 300_000 }) => {
    if (framework === 'node') {
      const files = path ? [inWorkspace(root, path)] : ['test/*.test.mjs', 'test/*.test.js', 'src/**/*.test.mjs', 'src/**/*.test.js', '*.test.mjs'];
      const r = runSuite(root, 'node', ['--test', '--test-reporter=junit', ...files], timeoutMs);
      const xml = (r.stdout || '') + '\n' + (r.stderr || '');
      if (!xml.includes('<testsuites') && !xml.includes('<testcase')) {
        throw new ToolError('ERR_TEST_PARSE', 'no junit report in output; tests may not exist',
          { exitCode: r.status, stderrTail: (r.stderr || '').slice(-300) });
      }
      return { ...parseJunitXml(xml, 'node'), exitCode: r.status };
    }
    if (framework === 'pytest') {
      const junit = join(root, '.nc-tools', 'junit.xml');
      const r = runSuite(root, 'python', ['-m', 'pytest', path || '.', '--junitxml', junit, '-q', '--no-header'], timeoutMs);
      if (!existsSync(junit)) {
        throw new ToolError('ERR_TEST_PARSE', 'pytest produced no junit report; are there any tests?',
          { exitCode: r.status, stderrTail: (r.stderr || '').slice(-300) });
      }
      const xml = readFileSync(junit, 'utf8');
      rmSync(junit, { force: true });
      return { ...parseJunitXml(xml, 'pytest'), exitCode: r.status };
    }
    throw new ToolError('ERR_BAD_INPUT', `unsupported framework: ${framework} (supported: node, pytest)`);
  };

  return { 'test.run': { handler: run } };
}
