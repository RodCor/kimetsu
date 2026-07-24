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

/** Hard cap on any single kimetsu invocation. A hung binary must not stall a turn. */
const EXEC_TIMEOUT_MS = 10000;

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

/** Best-effort session id from Pi's handler context, across naming variants. */
function sessionIdOf(ctx: any): string {
  const candidates = [ctx?.sessionId, ctx?.sessionID, ctx?.session_id, ctx?.session?.id];
  for (const candidate of candidates) {
    if (typeof candidate === "string" && candidate.trim() !== "") return candidate;
  }
  return FALLBACK_SESSION_ID;
}

/** `--workspace <cwd>` when Pi tells us the working directory, else nothing
 *  (the CLI then defaults to its own cwd). */
function workspaceArgs(ctx: any): string[] {
  const cwd = ctx?.cwd;
  return typeof cwd === "string" && cwd.trim() !== "" ? ["--workspace", cwd] : [];
}

export default function (pi: any) {
  // session_start fires once when Pi starts up or a new session begins.
  // Warming spawns the embedder daemon so the first real retrieval is semantic
  // rather than falling back to lexical FTS.
  // (`brain warm` takes no --workspace: it resolves the project from its cwd.)
  pi.on("session_start", async (_event: any, _ctx: any) => {
    await kimetsuRun(["brain", "warm"]);
  });

  // before_agent_start fires with the user's prompt, before the model is
  // called, and can return a message that joins the turn. This is where brain
  // context is injected. Pi has no session-start context surface, so
  // --warm-on-first-prompt folds the repo digest and episodic resume into the
  // first turn of each session.
  pi.on("before_agent_start", async (event: any, ctx: any) => {
    const payload = JSON.stringify({
      session_id: sessionIdOf(ctx),
      prompt: typeof event?.prompt === "string" ? event.prompt : "",
    });
    const stdout = await kimetsuRun(
      ["brain", "context-hook", "--warm-on-first-prompt", ...workspaceArgs(ctx)],
      payload,
    );
    const content = parseAdditionalContext(stdout);
    if (content === undefined) return; // nothing relevant — zero tokens
    return {
      message: {
        customType: "kimetsu-brain",
        content,
        display: false,
      },
    };
  });

  // agent_end fires after the LLM turn completes (maps to Kimetsu stop-hook).
  pi.on("agent_end", async (_event: any, ctx: any) => {
    await kimetsuRun(["brain", "stop-hook", ...workspaceArgs(ctx)]);
  });

  // session_shutdown fires on clean session close (maps to session-end-hook).
  pi.on("session_shutdown", async (_event: any, ctx: any) => {
    await kimetsuRun(["brain", "session-end-hook", ...workspaceArgs(ctx)]);
  });
}
