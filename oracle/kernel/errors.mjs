// Kernel error type: structured, machine-readable, with remediation hints.
export class ToolError extends Error {
  /**
   * @param {string} code stable machine-readable code, e.g. ERR_NOT_FOUND
   * @param {string} message human-readable
   * @param {object} [hint] structured remediation data for the agent
   */
  constructor(code, message, hint = undefined) {
    super(message);
    this.name = 'ToolError';
    this.code = code;
    this.hint = hint;
  }
  toJSON() {
    return { code: this.code, message: this.message, hint: this.hint };
  }
}
