// net.* — typed network operations: HTTP requests and port probing.
// Replaces curl/wget/nc one-liners with structured results.
import net from 'node:net';
import { ToolError } from './errors.mjs';

const MAX_BODY = 2_000_000;

export function makeNetTools() {
  const http = async ({ method = 'GET', url, headers = {}, body, timeoutMs = 30_000 }) => {
    if (typeof url !== 'string' || !/^https?:\/\//.test(url)) {
      throw new ToolError('ERR_BAD_INPUT', 'url must be an http(s) URL', { got: url });
    }
    const started = Date.now();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    try {
      const resp = await fetch(url, {
        method, headers, body,
        signal: controller.signal,
      });
      const raw = await resp.text();
      const respHeaders = {};
      resp.headers.forEach((v, k) => { respHeaders[k] = v; });
      return {
        url, method, status: resp.status, ok: resp.ok,
        headers: respHeaders,
        body: raw.slice(0, MAX_BODY),
        bodyTruncated: raw.length > MAX_BODY,
        durationMs: Date.now() - started,
      };
    } catch (e) {
      const isTimeout = e.name === 'AbortError';
      throw new ToolError(isTimeout ? 'ERR_TIMEOUT' : 'ERR_NET', `HTTP ${method} ${url} failed: ${e.message}`,
        { timeoutMs, hint: isTimeout ? 'increase timeoutMs or check the target is up' : 'check DNS/firewall/url' });
    } finally {
      clearTimeout(timer);
    }
  };

  const probePort = ({ port, host = '127.0.0.1', timeoutMs = 2000 }) => {
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      throw new ToolError('ERR_BAD_INPUT', 'port must be an integer 1-65535', { got: port });
    }
    return new Promise((resolvePromise) => {
      const started = Date.now();
      const socket = net.createConnection({ port, host });
      const finish = (open) => {
        socket.destroy();
        resolvePromise({ host, port, open, latencyMs: Date.now() - started });
      };
      socket.setTimeout(timeoutMs);
      socket.on('connect', () => finish(true));
      socket.on('timeout', () => finish(false));
      socket.on('error', () => finish(false));
    });
  };

  return {
    'net.http': { handler: http },
    'net.probePort': { handler: probePort },
  };
}
