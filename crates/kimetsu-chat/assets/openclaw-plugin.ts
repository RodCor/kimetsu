// Kimetsu brain plugin for OpenClaw (openclaw/openclaw).
//
// CANONICAL SOURCE: kimetsu/crates/kimetsu-chat/assets/openclaw-plugin.ts
// `kimetsu plugin install openclaw` writes this file verbatim. Edit it here.
//
// `agent_turn_prepare` is the injection point: it receives the turn's prompt
// and accepts `prependContext`, so brain context lands ahead of the user's
// message. The hook payload goes in on the CLI's stdin and the
// `additionalContext` block comes back on its stdout.
//
// Every failure mode is a silent no-op: a missing binary, a hung binary, a
// crash, unparseable output. Kimetsu is a sidecar — it must never break
// OpenClaw.

import { spawn } from "node:child_process";
import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";

/** Hard cap on any single kimetsu invocation. A hung binary must not stall a turn. */
const EXEC_TIMEOUT_MS = 10000;

/** Fallback session id when the hook context does not expose one. Stable per
 *  process, which is what per-session dedupe and refractory windows need. */
const FALLBACK_SESSION_ID = `openclaw-${process.pid}`;

/**
 * Run `kimetsu <args>`, optionally writing `input` to its stdin, and resolve
 * with whatever it printed to stdout ("" on any failure).
 *
 * stdout is PIPED, not ignored: the context hook communicates entirely through
 * it. stderr stays ignored so diagnostics never mix into the parsed payload.
 */
function kimetsuRun(args: string[], input?: string): Promise<string> {
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
      }, EXEC_TIMEOUT_MS);
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

/** Best-effort session id from the hook context, across naming variants. */
function sessionIdOf(ctx: any): string {
  const candidates = [ctx?.sessionId, ctx?.sessionKey, ctx?.session_id, ctx?.session?.id];
  for (const candidate of candidates) {
    if (typeof candidate === "string" && candidate.trim() !== "") return candidate;
  }
  return FALLBACK_SESSION_ID;
}

/** The turn's prompt, across the naming variants OpenClaw has used. */
function promptOf(ctx: any): string {
  const candidates = [ctx?.prompt, ctx?.message, ctx?.userPrompt, ctx?.text];
  for (const candidate of candidates) {
    if (typeof candidate === "string" && candidate.trim() !== "") return candidate;
  }
  return "";
}

export default definePluginEntry({
  register(api: any) {
    // Warm the embedder daemon at plugin registration (startup), so the first
    // retrieval is semantic rather than falling back to lexical FTS.
    kimetsuRun(["brain", "warm"]);

    // agent_turn_prepare fires before each turn with the prompt in hand and
    // accepts prependContext. OpenClaw has no session-start context surface,
    // so --warm-on-first-prompt folds the repo digest and episodic resume into
    // the first turn of each session.
    api.on("agent_turn_prepare", async (ctx: any) => {
      const payload = JSON.stringify({
        session_id: sessionIdOf(ctx),
        prompt: promptOf(ctx),
      });
      const stdout = await kimetsuRun(
        ["brain", "context-hook", "--warm-on-first-prompt"],
        payload,
      );
      const context = parseAdditionalContext(stdout);
      if (context === undefined) return; // nothing relevant — zero tokens
      return { prependContext: context };
    });

    // agent_end fires after each turn: record audit marker / nudge memory.
    api.on("agent_end", async (_ctx: any) => {
      await kimetsuRun(["brain", "stop-hook"]);
    });

    // session_end fires on clean session close.
    api.on("session_end", async (_ctx: any) => {
      await kimetsuRun(["brain", "session-end-hook"]);
    });
  },
});
