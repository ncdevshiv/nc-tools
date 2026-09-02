// Test-suite driver: black-box access to the nc-tools kernel over MCP stdio.
// The implementation lives in tools/kernel-client.mjs (shared with the bench
// harness); this module adapts it for node:test with a per-root server pool
// so `new Kernel(root)` in a beforeEach is cheap and leak-free.
import { pooledKernel } from '../tools/kernel-client.mjs';

export { SERVER_BIN, spawnServer, withKernel } from '../tools/kernel-client.mjs';

export class Kernel {
  constructor(root) {
    return pooledKernel(root);
  }
}
