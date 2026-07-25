/**
 * Failure modes of a Kimetsu Remote call, as distinct classes.
 *
 * The distinction is not cosmetic. A caller retries a {@link KimetsuRateLimitError}
 * after `retryAfter`, re-authenticates on a {@link KimetsuAuthError}, and should
 * do neither on a {@link KimetsuToolError} — the request was understood and the
 * answer is no. Collapsing all three into one error type, which is what a
 * `fetch` wrapper does by default, is what makes every integration re-derive
 * this logic from HTTP status codes.
 *
 * Mirrors `kimetsu.errors` in the Python SDK exactly, so a team using both
 * reads one set of names.
 */

/** Base class for every error this SDK raises. */
export class KimetsuError extends Error {
  constructor(message: string) {
    super(message);
    this.name = new.target.name;
  }
}

/** HTTP 401 (missing or invalid token) or 403 (wrong repo, or token out of scope). */
export class KimetsuAuthError extends KimetsuError {}

/**
 * HTTP 429.
 *
 * `retryAfter` is the server's `Retry-After` header in seconds when it sent a
 * numeric one, and `undefined` otherwise — including when the header held an
 * HTTP-date, which this deliberately does not guess at.
 */
export class KimetsuRateLimitError extends KimetsuError {
  readonly retryAfter: number | undefined;

  constructor(message: string, retryAfter?: number) {
    super(message);
    this.retryAfter = retryAfter;
  }
}

/**
 * A JSON-RPC `error` object came back inside an otherwise well-formed envelope.
 *
 * The transport worked and the server understood the call. Retrying it
 * unchanged will fail the same way.
 */
export class KimetsuToolError extends KimetsuError {
  readonly code: number | undefined;

  constructor(message: string, code?: number) {
    super(message);
    this.code = code;
  }
}

/** Handshake failure, unexpected HTTP status, or a malformed JSON-RPC envelope. */
export class KimetsuProtocolError extends KimetsuError {}
