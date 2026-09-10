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
// Every failure mode is a silent no-op: a missing binary, a hung binary, a
// crash, unparseable output. Kimetsu is a sidecar — it must never break Pi.

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

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
    const request = { id: randomUUID(), sessionId: sessionIdOf(ctx) };
    // Expire the last task immediately, even if retrieval is empty or fails.
    activeContext = request;
    const payload = JSON.stringify({
      session_id: request.sessionId,
      prompt: typeof event?.prompt === "string" ? event.prompt : "",
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
}
