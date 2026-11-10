// Kimetsu brain extension for Pi (earendil-works/pi).
//
// CANONICAL SOURCE: kimetsu/crates/kimetsu-chat/assets/pi-extension.ts
// `kimetsu plugin install pi` writes this file verbatim, and the published
// `kimetsu-pi` npm package vendors a byte-identical copy (CI diffs the two).
// Edit it here — never in the installed or published copy.
//
// Pi exposes no MCP surface, so Kimetsu integrates by shelling out to the
// binary on lifecycle events. `before_agent_start` is the injection point:
// the hook payload goes in on stdin, the `additionalContext` block comes back
// on stdout, and Pi carries it into the turn as a context message.
//
// Every failure mode of a lifecycle hook is a silent no-op: a missing binary,
// a hung binary, a crash, unparseable output. Kimetsu is a sidecar — it must
// never break Pi.
//
// The peer network is separate. Pi has no MCP, so `kimetsu_network_status`
// and `kimetsu_network_request` shell out to the local binary. They require
// Kimetsu 2.11.0 and a build with the `network` feature. Anything older
// degrades with an explanation. Sharing is never started from here: it
// listens until the user stops it, and that stays a terminal command.

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

/** Interactive hooks must not leave a turn waiting on a hung binary. */
const EXEC_TIMEOUT_MS = 10000;

/** Session saving may distill lessons and an episode in two model calls
 *  (120s each by default). Leave time for both plus local persistence. */
const SESSION_SAVE_TIMEOUT_MS = 300000;

/** Fallback session id when Pi's context does not expose one. Stable per process,
 *  which is what the brain's per-session dedupe and refractory windows need. */
const FALLBACK_SESSION_ID = `pi-${process.pid}`;

/**
 * Run `kimetsu <args>`, optionally writing `input` to its stdin, and resolve
 * with whatever it printed to stdout ("" on any failure).
 *
 * stdout is PIPED, not ignored: the context hook communicates entirely through
 * it. stderr stays ignored so diagnostics never mix into the parsed payload.
 */
function kimetsuRun(args: string[], input?: string, timeoutMs = EXEC_TIMEOUT_MS): Promise<string> {
  return new Promise((resolve) => {
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stdout = "";
    const done = () => {
      if (settled) return;
      settled = true;
      if (timer !== undefined) clearTimeout(timer);
      resolve(stdout);
    };
    try {
      const child = spawn("kimetsu", args, {
        stdio: ["pipe", "pipe", "ignore"],
        shell: false,
        windowsHide: true,
      });
      // Cap the wait and kill the child if it overruns. unref() so the timer
      // alone can never keep the host process alive.
      timer = setTimeout(() => {
        child.kill();
        done();
      }, timeoutMs);
      if (typeof timer.unref === "function") timer.unref();

      child.stdout?.setEncoding("utf8");
      child.stdout?.on("data", (chunk: string) => {
        stdout += chunk;
      });
      child.stdout?.on("error", () => {}); // torn pipe — resolve with what we have
      child.stdin?.on("error", () => {}); // EPIPE when the child exits early

      child.on("error", done); // binary not on PATH — silent no-op
      child.on("close", done); // 'close' (not 'exit') so stdout is complete

      child.stdin?.end(input ?? "");
    } catch {
      done(); // any unexpected error — silent no-op
    }
  });
}

/**
 * Pull `hookSpecificOutput.additionalContext` out of a hook's stdout.
 *
 * The hook prints a single JSON line, but scanning from the end tolerates any
 * stray output ahead of it. Anything unparseable yields `undefined`, which the
 * callers treat as "nothing to inject".
 */
function parseAdditionalContext(stdout: string): string | undefined {
  const lines = stdout
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "")
    .reverse();
  for (const line of lines) {
    try {
      const parsed = JSON.parse(line);
      const context = parsed?.hookSpecificOutput?.additionalContext;
      if (typeof context === "string" && context.trim() !== "") return context;
    } catch {
      // Not JSON — keep looking at earlier lines.
    }
  }
  return undefined;
}

/** Best-effort session id from Pi's handler context, across naming variants. */
function sessionIdOf(ctx: any): string {
  // Current Pi exposes the durable id through SessionManager. Prefer it over
  // historical context-field variants so /new, /resume, and /fork each get a
  // distinct Kimetsu session even when they happen in the same Pi process.
  const getSessionId = ctx?.sessionManager?.getSessionId;
  if (typeof getSessionId === "function") {
    try {
      const id = getSessionId.call(ctx.sessionManager);
      if (typeof id === "string" && id.trim() !== "") return id;
    } catch {
      // A third-party/legacy SessionManager must not break the host.
    }
  }
  const candidates = [ctx?.sessionId, ctx?.sessionID, ctx?.session_id, ctx?.session?.id];
  for (const candidate of candidates) {
    if (typeof candidate === "string" && candidate.trim() !== "") return candidate;
  }
  return FALLBACK_SESSION_ID;
}

/** Current Pi's persisted JSONL transcript, when the session is not ephemeral. */
function transcriptPathOf(ctx: any): string | undefined {
  const getSessionFile = ctx?.sessionManager?.getSessionFile;
  if (typeof getSessionFile !== "function") return undefined;
  try {
    const path = getSessionFile.call(ctx.sessionManager);
    return typeof path === "string" && path.trim() !== "" ? path : undefined;
  } catch {
    return undefined;
  }
}

/** Host-neutral hook payload understood by Kimetsu v2.7. */
function lifecyclePayload(ctx: any, transcript?: unknown[]): string {
  const payload: Record<string, unknown> = { session_id: sessionIdOf(ctx) };
  const transcriptPath = transcriptPathOf(ctx);
  if (transcriptPath !== undefined) payload.transcript_path = transcriptPath;
  else if (Array.isArray(transcript)) payload.transcript = transcript;
  return JSON.stringify(payload);
}

/** `--workspace <cwd>` when Pi tells us the working directory, else nothing
 *  (the CLI then defaults to its own cwd). */
function workspaceArgs(ctx: any): string[] {
  const cwd = ctx?.cwd;
  return typeof cwd === "string" && cwd.trim() !== "" ? ["--workspace", cwd] : [];
}

/** First Kimetsu release that ships `kimetsu network` in release builds. */
const NETWORK_MIN_VERSION: readonly [number, number, number] = [2, 11, 0];

/** `request` exchanges announcements for up to 20s, then transfers one pack. */
const NETWORK_REQUEST_TIMEOUT_MS = 60_000;

const SHARE_INSTRUCTIONS = [
  "Sharing stays off until you run it yourself. In this project directory, run: kimetsu network share",
  "That command listens until you press Ctrl-C. The relay stays off. Only this repo's memories are sent, after they are signed and scrubbed. Do not add --map-ports unless you want the router to open a UDP port. Nothing is listening now.",
].join("\n");

type Capture = {
  stdout: string;
  stderr: string;
  code: number | null;
  timedOut: boolean;
  aborted: boolean;
};

type StatusDetails = {
  available: boolean;
  compiled: boolean | null;
  joined: boolean;
  listening: boolean;
  group: string | null;
  role: string | null;
  generation: number | null;
  endpoint_id: string | null;
  message: string;
};

type RequestDetails = {
  available: boolean;
  listening: boolean;
  verified: boolean;
  imported: number;
  deduped: number;
  quarantined: number;
  superseded: number;
  digest: string | null;
  message: string;
};

type ToolResult<T> = {
  content: [{ type: "text"; text: string }];
  details: T;
  structuredContent: T;
  isError?: boolean;
};

/**
 * Run `kimetsu <args>` and keep stdout, stderr, and the exit code.
 * stdin is ignored. A timeout or abort kills the child so a request
 * cannot keep a socket open after Pi has moved on.
 */
function kimetsuCapture(args: string[], timeoutMs: number, signal?: AbortSignal): Promise<Capture> {
  if (signal?.aborted) {
    return Promise.resolve({ stdout: "", stderr: "", code: null, timedOut: false, aborted: true });
  }
  return new Promise((resolve) => {
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let aborted = false;
    const finish = (code: number | null) => {
      if (settled) return;
      settled = true;
      if (timer !== undefined) clearTimeout(timer);
      if (signal !== undefined) signal.removeEventListener("abort", onAbort);
      resolve({ stdout, stderr, code, timedOut, aborted });
    };
    const onAbort = () => {
      aborted = true;
      child.kill();
      finish(null);
    };
    let child: ReturnType<typeof spawn>;
    try {
      child = spawn("kimetsu", args, {
        stdio: ["ignore", "pipe", "pipe"],
        shell: false,
        windowsHide: true,
      });
      timer = setTimeout(() => {
        timedOut = true;
        child.kill();
        finish(null);
      }, timeoutMs);
      if (typeof timer.unref === "function") timer.unref();
      signal?.addEventListener("abort", onAbort, { once: true });
      child.stdout?.setEncoding("utf8");
      child.stderr?.setEncoding("utf8");
      child.stdout?.on("data", (chunk: string) => { stdout += chunk; });
      child.stderr?.on("data", (chunk: string) => { stderr += chunk; });
      child.stdout?.on("error", () => {});
      child.stderr?.on("error", () => {});
      child.on("error", () => finish(null));
      child.on("close", (code) => finish(typeof code === "number" ? code : 0));
    } catch {
      finish(null);
    }
  });
}

function parseVersion(output: string): { numbers: [number, number, number]; label: string } | undefined {
  const match = output.match(/(\d+)\.(\d+)\.(\d+)(\s*\([^)]*\))?/);
  if (match === null) return undefined;
  return {
    numbers: [Number(match[1]), Number(match[2]), Number(match[3])],
    label: `${match[1]}.${match[2]}.${match[3]}${match[4] ?? ""}`,
  };
}

function versionAtLeast(version: readonly [number, number, number], min: readonly [number, number, number]): boolean {
  for (let index = 0; index < 3; index += 1) {
    if (version[index] !== min[index]) return version[index] > min[index];
  }
  return true;
}

function parseLastJson(stdout: string): Record<string, unknown> | undefined {
  const lines = stdout.split("\n").map((line) => line.trim()).filter((line) => line !== "").reverse();
  for (const line of lines) {
    try {
      const parsed = JSON.parse(line);
      if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)) return parsed;
    } catch {
      // Keep looking at earlier lines.
    }
  }
  return undefined;
}

function textResult<T extends { message: string }>(details: T, isError = false): ToolResult<T> {
  return {
    content: [{ type: "text", text: details.message }],
    details,
    structuredContent: details,
    ...(isError ? { isError: true } : {}),
  };
}

function emptyStatus(message: string, compiled: boolean | null = null): StatusDetails {
  return {
    available: false,
    compiled,
    joined: false,
    listening: false,
    group: null,
    role: null,
    generation: null,
    endpoint_id: null,
    message,
  };
}

function emptyRequest(message: string): RequestDetails {
  return {
    available: false,
    listening: false,
    verified: false,
    imported: 0,
    deduped: 0,
    quarantined: 0,
    superseded: 0,
    digest: null,
    message,
  };
}

function commandFailure(captured: Capture, fallback: string): string {
  const text = `${captured.stderr}\n${captured.stdout}`
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "")
    .join(" ");
  if (text === "") return fallback;
  if (/nothing is listening|nothing was contacted/i.test(text)) return text;
  return `${text} Nothing is listening.`;
}

function normalizePeer(peer: string): string | undefined {
  const value = peer.trim();
  const at = value.indexOf("@");
  if (at <= 0 || at !== value.lastIndexOf("@") || at === value.length - 1) return undefined;
  if (/\s/.test(value)) return undefined;
  const id = value.slice(0, at);
  const address = value.slice(at + 1);
  if (id.startsWith("-") || address.startsWith("-")) return undefined;
  return value;
}

function asNullableString(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value : null;
}

function asCount(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

type Gate =
  | { ok: true; status: StatusDetails }
  | { ok: false; status: StatusDetails };

/** Version gate, then `network status --json`. Status never opens a socket. */
async function networkGate(signal?: AbortSignal): Promise<Gate> {
  const version = await kimetsuCapture(["--version"], EXEC_TIMEOUT_MS, signal);
  if (version.aborted) {
    return { ok: false, status: emptyStatus("The network check was cancelled. Nothing is listening.") };
  }
  if (version.timedOut) {
    return { ok: false, status: emptyStatus("Kimetsu did not respond and was stopped. Nothing is listening.") };
  }
  if (version.code === null) {
    return { ok: false, status: emptyStatus("Kimetsu is not on PATH. Nothing is listening.") };
  }
  const parsed = parseVersion(`${version.stdout}\n${version.stderr}`);
  if (parsed !== undefined && !versionAtLeast(parsed.numbers, NETWORK_MIN_VERSION)) {
    return {
      ok: false,
      status: emptyStatus(
        `Peer network needs Kimetsu ${NETWORK_MIN_VERSION.join(".")} or newer. This binary is ${parsed.label}. Nothing is listening.`,
      ),
    };
  }
  if (version.code !== 0 && parsed === undefined) {
    return { ok: false, status: emptyStatus(commandFailure(version, "Kimetsu is not on PATH. Nothing is listening.")) };
  }
  const status = await kimetsuCapture(["network", "status", "--json"], EXEC_TIMEOUT_MS, signal);
  if (status.aborted) {
    return { ok: false, status: emptyStatus("The network check was cancelled. Nothing is listening.") };
  }
  if (status.timedOut) {
    return { ok: false, status: emptyStatus("Kimetsu did not respond and was stopped. Nothing is listening.") };
  }
  if (status.code === null) {
    return { ok: false, status: emptyStatus("Kimetsu is not on PATH. Nothing is listening.") };
  }
  if (status.code !== 0) {
    return {
      ok: false,
      status: emptyStatus(commandFailure(status, "Kimetsu network status failed. Nothing is listening.")),
    };
  }
  const body = parseLastJson(status.stdout);
  if (body === undefined) {
    return { ok: false, status: emptyStatus("Kimetsu network status returned nothing readable. Nothing is listening.") };
  }
  if (body.compiled !== true) {
    return { ok: false, status: emptyStatus("Peer network is not included in this build. Nothing is listening.", false) };
  }
  const details: StatusDetails = {
    available: true,
    compiled: true,
    joined: body.joined === true,
    listening: body.listening === true,
    group: asNullableString(body.group),
    role: asNullableString(body.role),
    generation: typeof body.generation === "number" && Number.isFinite(body.generation) ? body.generation : null,
    endpoint_id: asNullableString(body.endpoint_id),
    message: "",
  };
  if (!details.joined) {
    details.message = "Peer network: not joined. Nothing is listening.";
  } else {
    const endpoint = details.endpoint_id === null ? "" : ` Endpoint ${details.endpoint_id}.`;
    details.message = `Peer network: joined "${details.group ?? "network"}" (${details.role ?? "member"}).${endpoint} Nothing is listening.`;
  }
  return { ok: true, status: details };
}

async function networkStatus(signal?: AbortSignal): Promise<ToolResult<StatusDetails>> {
  const gate = await networkGate(signal);
  return textResult(gate.status, false);
}

async function networkRequest(
  params: { peer?: string; discover?: boolean; map_ports?: boolean },
  ctx: { cwd?: string },
  signal?: AbortSignal,
): Promise<ToolResult<RequestDetails>> {
  const discover = params.discover === true;
  const mapPorts = params.map_ports === true;
  if (typeof params.peer === "string" && normalizePeer(params.peer) === undefined) {
    return textResult(emptyRequest(
      "Peer should look like <endpoint-id>@host:port, the line `kimetsu network share` prints. Nothing was contacted.",
    ), true);
  }
  const peer = typeof params.peer === "string" ? normalizePeer(params.peer) : undefined;
  if (peer === undefined && !discover) {
    return textResult(emptyRequest(
      "Pass peer (endpoint-id@host:port) or set discover to true. Nothing was contacted.",
    ), true);
  }
  const gate = await networkGate(signal);
  if (!gate.ok) return textResult(emptyRequest(gate.status.message), true);
  const args = ["network", "request", "--json"];
  if (!discover) args.push("--no-mdns");
  args.push(...workspaceArgs(ctx));
  if (peer !== undefined) args.push("--peer", peer);
  if (mapPorts) args.push("--map-ports");
  const captured = await kimetsuCapture(args, NETWORK_REQUEST_TIMEOUT_MS, signal);
  if (captured.aborted) {
    return textResult(emptyRequest("The peer request was cancelled and the socket was closed. Nothing is listening."), true);
  }
  if (captured.timedOut) {
    return textResult(emptyRequest("The peer request timed out and the socket was closed. Nothing is listening."), true);
  }
  if (captured.code === null) {
    return textResult(emptyRequest("Kimetsu is not on PATH. Nothing is listening."), true);
  }
  if (captured.code !== 0) {
    return textResult(emptyRequest(commandFailure(captured, "Kimetsu network request failed. Nothing is listening.")), true);
  }
  const body = parseLastJson(captured.stdout);
  if (body === undefined || body.verified !== true) {
    return textResult(emptyRequest(commandFailure(captured, "Kimetsu network request returned nothing readable. Nothing is listening.")), true);
  }
  const quarantined = asCount(body.quarantined);
  let message = `Verified the signature. Imported ${asCount(body.imported)}, already had ${asCount(body.deduped)}, quarantined ${quarantined}, superseded ${asCount(body.superseded)}. Nothing is listening now.`;
  if (quarantined > 0) {
    message += " Quarantined memories stay out of retrieval until you accept them.";
  }
  const details: RequestDetails = {
    available: true,
    listening: body.listening === true,
    verified: true,
    imported: asCount(body.imported),
    deduped: asCount(body.deduped),
    quarantined,
    superseded: asCount(body.superseded),
    digest: asNullableString(body.digest),
    message,
  };
  return textResult(details, false);
}

const networkNamespace = {
  name: "kimetsu-network",
  description: "Peer memory network. Nothing listens until a request runs. Sharing is a terminal command.",
};

const statusOutput = Type.Object({
  available: Type.Boolean(),
  compiled: Type.Union([Type.Boolean(), Type.Null()]),
  joined: Type.Boolean(),
  listening: Type.Boolean(),
  group: Type.Union([Type.String(), Type.Null()]),
  role: Type.Union([Type.String(), Type.Null()]),
  generation: Type.Union([Type.Number(), Type.Null()]),
  endpoint_id: Type.Union([Type.String(), Type.Null()]),
  message: Type.String(),
});

const requestOutput = Type.Object({
  available: Type.Boolean(),
  listening: Type.Boolean(),
  verified: Type.Boolean(),
  imported: Type.Number(),
  deduped: Type.Number(),
  quarantined: Type.Number(),
  superseded: Type.Number(),
  digest: Type.Union([Type.String(), Type.Null()]),
  message: Type.String(),
});

export default function (pi: ExtensionAPI) {
  // Pi persists injected messages, including display:false messages. Only the
  // current task's injection belongs in future model calls. A unique marker
  // survives Pi's message cloning without trusting content or timestamps.
  let activeContext: { id: string; sessionId: string } | undefined;

  // session_start fires once when Pi starts up or a new session begins.
  // Warming spawns the embedder daemon so the first real retrieval is semantic
  // rather than falling back to lexical FTS.
  // (`brain warm` takes no --workspace: it resolves the project from its cwd.)
  pi.on("session_start", async (_event, _ctx) => {
    activeContext = undefined;
    await kimetsuRun(["brain", "warm"]);
  });

  // before_agent_start fires with the user's prompt, before the model is
  // called, and can return a message that joins the turn. This is where brain
  // context is injected. Pi has no session-start context surface, so
  // --warm-on-first-prompt folds the repo digest and episodic resume into the
  // first turn of each session.
  pi.on("before_agent_start", async (event, ctx) => {
    // Pi 1.x also starts turns from custom messages. Those are not user
    // prompts: expire earlier evidence without retrieving for an empty task.
    // Checking the prompt field also preserves pre-1.x user events.
    activeContext = undefined;
    if (!("prompt" in event) || typeof event.prompt !== "string") return;
    const request = { id: randomUUID(), sessionId: sessionIdOf(ctx) };
    // Expire the last task immediately, even if retrieval is empty or fails.
    activeContext = request;
    const payload = JSON.stringify({
      session_id: request.sessionId,
      prompt: event.prompt,
    });
    const stdout = await kimetsuRun(
      ["brain", "context-hook", "--warm-on-first-prompt", ...workspaceArgs(ctx)],
      payload,
    );
    // A session switch or a newer prompt can supersede an in-flight request.
    if (activeContext !== request) return;
    const content = parseAdditionalContext(stdout);
    if (content === undefined) return; // nothing relevant — zero tokens
    return {
      message: {
        customType: "kimetsu-brain",
        content,
        display: false,
        details: { kimetsuContextId: request.id },
      },
    };
  });

  pi.on("context", async (event, ctx) => {
    const context = activeContext;
    const currentIndex = context && context.sessionId === sessionIdOf(ctx)
      ? event.messages.findIndex((message) => {
        if (message.role !== "custom" || message.customType !== "kimetsu-brain") return false;
        const details = message.details as { kimetsuContextId?: unknown } | undefined;
        return details?.kimetsuContextId === context.id;
      })
      : -1;
    // Queued steering/follow-up messages bypass before_agent_start. Expire
    // the old injection when a newer user message arrives; tool results alone
    // do not end the current task's context.
    if (currentIndex >= 0 && event.messages.some((message, index) =>
      index > currentIndex && message.role === "user"
    )) activeContext = undefined;

    // Filter the model's copy only; preserve the persisted session history.
    return {
      messages: event.messages.filter((message, index) =>
        message.role !== "custom" || message.customType !== "kimetsu-brain"
        || (activeContext !== undefined && index === currentIndex)
      ),
    };
  });

  pi.on("session_before_compact", async (event) => {
    // Summarization bypasses the context event. Do not turn retrieved evidence
    // into a durable summary that could outlive a correction or invalidation.
    event.preparation.messagesToSummarize = event.preparation.messagesToSummarize.filter(
      (message) => message.role !== "custom" || message.customType !== "kimetsu-brain",
    );
    event.preparation.turnPrefixMessages = event.preparation.turnPrefixMessages.filter(
      (message) => message.role !== "custom" || message.customType !== "kimetsu-brain",
    );
  });

  pi.on("session_before_tree", async (event) => {
    // Pi retains a reference to this temporary summary input array, so filter
    // in place. The persisted session entries themselves are left untouched.
    const entries = event.preparation.entriesToSummarize;
    let kept = 0;
    for (const entry of entries) {
      if (entry.type !== "custom_message" || entry.customType !== "kimetsu-brain") {
        entries[kept++] = entry;
      }
    }
    entries.length = kept;
  });

  pi.on("session_tree", async () => {
    activeContext = undefined;
  });

  // agent_end fires after the LLM turn completes (maps to Kimetsu stop-hook).
  pi.on("agent_end", async (event, ctx) => {
    await kimetsuRun(
      ["brain", "stop-hook", ...workspaceArgs(ctx)],
      lifecyclePayload(ctx, event.messages),
    );
  });

  // session_shutdown fires on clean session close (maps to session-end-hook).
  pi.on("session_shutdown", async (_event, ctx) => {
    activeContext = undefined;
    await kimetsuRun(
      ["brain", "session-end-hook", ...workspaceArgs(ctx)],
      lifecyclePayload(ctx),
      SESSION_SAVE_TIMEOUT_MS,
    );
  });

  // Registered after the lifecycle hooks. Neither tool runs during a turn
  // unless the model or the user calls it. Share is a command, not a tool,
  // and that command does not spawn the binary.
  pi.registerTool({
    name: "kimetsu_network_status",
    label: "Kimetsu network status",
    description: "Whether this machine has joined a Kimetsu peer network. Does not listen, discover peers, or contact a relay. Sharing stays a terminal command: kimetsu network share.",
    promptSnippet: "Check Kimetsu peer-network membership without listening",
    promptGuidelines: [
      "Does not listen, discover peers, or contact a relay.",
      "Never run `kimetsu network share` unless the user explicitly asks in this turn. It listens until Ctrl-C; tell them to run it in a terminal.",
    ],
    parameters: Type.Object({}),
    outputSchema: statusOutput,
    annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false },
    namespace: networkNamespace,
    async execute(_toolCallId, _params, signal) {
      return networkStatus(signal);
    },
  });

  pi.registerTool({
    name: "kimetsu_network_request",
    label: "Kimetsu network request",
    description: "Fetch repo memories a peer is sharing. Verifies the pack signature before any write. Refuses packs that are not scoped to repo. Does nothing and opens no socket when this machine has not joined. Router port mapping stays off unless map_ports is true.",
    promptSnippet: "Fetch a signed repo memory pack from a Kimetsu peer",
    promptGuidelines: [
      "Call only when the user asks to fetch memories from a peer.",
      "Nothing listens until this call, and the socket closes when it finishes, times out, or Pi cancels it. The relay stays off.",
      "Packs are signed and scrubbed and must be the repo mount. An unknown signer is quarantined and stays out of retrieval.",
      "Leave discover false, and do not set map_ports, unless the user explicitly asks.",
      "Never run `kimetsu network share` from the agent. Tell the user to run it in a terminal; it listens until they press Ctrl-C.",
    ],
    parameters: Type.Object({
      peer: Type.Optional(Type.String({
        description: "endpoint-id@host:port from kimetsu network share",
      })),
      discover: Type.Optional(Type.Boolean({
        description: "Look for roster peers on the local network. Default false.",
      })),
      map_ports: Type.Optional(Type.Boolean({
        description: "Ask the router to open a UDP port. Default false.",
      })),
    }),
    outputSchema: requestOutput,
    annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: true, openWorldHint: true },
    executionMode: "sequential",
    namespace: networkNamespace,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      return networkRequest(params, ctx, signal);
    },
  });

  pi.registerCommand("kimetsu-network-status", {
    description: "Show whether this machine has joined a Kimetsu peer network. Does not listen.",
    handler: async (_args, ctx) => {
      const result = await networkStatus(ctx.signal);
      ctx.ui.notify(result.content[0].text, result.details.available ? "info" : "warning");
    },
  });

  pi.registerCommand("kimetsu-network-request", {
    description: "Fetch repo memories a peer is sharing. Listens only while the fetch runs.",
    handler: async (args, ctx) => {
      const peer = args.trim();
      if (peer === "") {
        ctx.ui.notify(
          "Pass the peer line from kimetsu network share: /kimetsu-network-request <endpoint-id>@host:port",
          "warning",
        );
        return;
      }
      const result = await networkRequest({ peer }, ctx, ctx.signal);
      ctx.ui.notify(result.content[0].text, result.isError ? "error" : "info");
    },
  });

  pi.registerCommand("kimetsu-network-share", {
    description: "Show the terminal command that shares repo memories. Does not open a socket.",
    handler: async (_args, ctx) => {
      ctx.ui.notify(SHARE_INSTRUCTIONS, "info");
    },
  });
}
