# @kimetsu-ai/sdk

A typed TypeScript client for [Kimetsu](https://github.com/RodCor/kimetsu) Remote —
the counterpart to the [Python SDK](https://github.com/RodCor/kimetsu-py).

## Why

`npm/kimetsu` and `npm/kimetsu-remote` are binary-download shims: they install
the native `kimetsu` executable and nothing else. Every TypeScript integration
Kimetsu targets — Pi extensions, OpenClaw plugins, Cursor, VS Code, MCP clients —
was therefore shelling out to that binary and parsing its text output. That is
how the Pi extension and its published npm copy drifted apart without anyone
noticing: there was no shared typed surface for them to share.

This is that surface.

## Install

```bash
npm install @kimetsu-ai/sdk
```

Node 18 or newer. **Zero runtime dependencies** — it uses the platform `fetch`.
An embedded client's dependencies become its host's dependencies, so it has none.

## Use

```ts
import { KimetsuClient } from '@kimetsu-ai/sdk';

// Reads KIMETSU_REMOTE_URL / _TOKEN / _REPO — the same variables the Python
// SDK reads, so one environment configures both.
const kimetsu = new KimetsuClient();

const bundle = await kimetsu.context('why does the migration test fail');
if (bundle.skipped) {
  // The brain had nothing relevant. Zero tokens were spent saying so.
} else {
  for (const capsule of bundle.capsules) {
    console.log(capsule.summary);
  }
}
```

Or pass credentials directly:

```ts
const kimetsu = new KimetsuClient({
  baseUrl: 'https://brain.example.com',
  token: process.env.MY_TOKEN,
  repo: 'acme/backend',
  timeoutMs: 10_000,
});
```

### Read the bundle before you trust it

A bundle that covers half the question looks exactly like one that answers it.
Two fields tell them apart, and using them is the difference between a memory
system that helps and one that confabulates:

```ts
const bundle = await kimetsu.context(question);

if (bundle.skipped) return null;                     // nothing relevant at all
if (bundle.partial_evidence_notice) {
  // A sentence written for a reader, naming what none of the capsules mention.
  console.log(bundle.partial_evidence_notice);
}
if ((bundle.evidence_coverage ?? 1) < 0.5) {
  // Memory has part of this and knows it. Prefer abstaining to inferring.
}
```

When the question was about order, `bundle.chronological` is true: the capsules
are oldest-first and each carries the date it was recorded.

### Capture

```ts
await kimetsu.record('SQLITE_BUSY under concurrent writers needs busy_timeout, not app retries', {
  tags: ['sqlite', 'concurrency'],
  kind: 'failure_pattern',
});

// Tell the brain a memory actually helped. This is the signal its ranking learns from.
await kimetsu.cite(bundle.capsules[0]?.expansion_handle?.replace('memory:', '') ?? '');
```

### Namespaces

`client.memory` (search, add, list, top, accept, reject, invalidate, blame,
proposals, conflicts, conflictResolve, prune), `client.config.show()`,
`client.models.list()`, and `client.benchmark` (context, recordOutcome) mirror
the Python SDK's layout method-for-method.

TypeScript is camelCase, the wire is snake_case; the SDK translates
(`memory.blame(runId)` sends `run_id`) so you never write the wire form.

### Tools newer than this SDK

The brain grows tools faster than the SDK is republished. `client.call()`
reaches any of them, so a version mismatch is never a reason to go back to
parsing text:

```ts
await kimetsu.call('kimetsu_brain_some_new_tool', { whatever: 'it takes' });
```

## Errors

Four classes, because a caller does something different with each. Retry a
`KimetsuRateLimitError` after `retryAfter`; re-authenticate on a
`KimetsuAuthError`; do neither on a `KimetsuToolError` — the request was
understood and the answer is no.

```ts
import { KimetsuRateLimitError, KimetsuToolError } from '@kimetsu-ai/sdk';

try {
  await kimetsu.context(question);
} catch (err) {
  if (err instanceof KimetsuRateLimitError) {
    await sleep((err.retryAfter ?? 5) * 1000);
  } else if (err instanceof KimetsuToolError) {
    console.error(`the brain refused: ${err.message}`);
  } else {
    throw err;
  }
}
```

`retryAfter` is `undefined` when the server sent an HTTP-date rather than
seconds. That is deliberate: an absent hint is honest, and a guessed one causes
a retry storm.

## Testing against it

Pass your own transport and no server is needed:

```ts
const calls: unknown[] = [];
const kimetsu = new KimetsuClient({
  transport: {
    async call(name, args) {
      calls.push({ name, args });
      return { capsules: [{ summary: 'project:fact - the thing' }] };
    },
  },
});
```

## Sync vs async

There is only one client. Python needs both `KimetsuClient` and
`AsyncKimetsuClient` because its HTTP libraries come in two flavours; in
JavaScript every call is already a promise, and a second class would be a second
thing to keep in step for no gain.

## License

MIT OR Apache-2.0
