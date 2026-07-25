/**
 * `@kimetsu-ai/sdk` — a typed TypeScript client for Kimetsu Remote.
 *
 * ```ts
 * import { KimetsuClient } from '@kimetsu-ai/sdk';
 *
 * const kimetsu = new KimetsuClient(); // reads KIMETSU_REMOTE_* from the env
 * const bundle = await kimetsu.context('why does the migration test fail');
 * if (!bundle.skipped) {
 *   for (const capsule of bundle.capsules) console.log(capsule.summary);
 * }
 * ```
 */

export { KimetsuClient, type KimetsuClientOptions } from './client.js';
export {
  KimetsuError,
  KimetsuAuthError,
  KimetsuProtocolError,
  KimetsuRateLimitError,
  KimetsuToolError,
} from './errors.js';
export {
  HttpTransport,
  PROTOCOL_VERSION,
  buildCall,
  buildInitialize,
  clean,
  parseEnvelope,
  type Args,
  type HttpTransportOptions,
  type Transport,
} from './transport.js';
export type {
  BrainStatus,
  ContextBundle,
  ContextCapsule,
  Memory,
  MemoryKind,
  MemoryScope,
  ToolResult,
} from './types.js';
