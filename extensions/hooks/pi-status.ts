// Managed by friring `extension install` (the built-in "hooks" extension).
// Reinstalling or updating overwrites this file — do not edit; uninstalling
// removes it. Reports pi's lifecycle state to friring. Identity comes from the
// inherited $FRIRING_SESSION env var; every call is best-effort so it can never
// break a session running outside friring.
//
// This is a pi extension (TypeScript), auto-discovered from
// ~/.pi/agent/extensions/*.ts by the pi.dev CLI. It subscribes to pi's
// lifecycle events and reports them to friring's status reporter:
//   session_start → idle, agent_start + tool_execution_start → working
//   (a tool call to ask_user_question → blocked), agent_end → done.
import { exec } from "node:child_process";
import { appendFileSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

// Exact marker prefix kept on one line so the remote (SSH/WSL) rewrite can swap
// this command for a tmux pane-option setter (there is no friring-cli on a
// remote host). Do not split the words across lines or reorder the flags.
const SIGNAL = "friring-cli session signal --state ";

// Fire-and-forget; errors are swallowed so a hook never surfaces into the
// agent. exec inherits the pi process env, so $FRIRING_SESSION travels.
//
// Inside a sandbox that binary need not exist, and the database it writes is
// out of reach by design (docs/SANDBOX.md ADR-29). friring then exports
// $FRIRING_SIGNAL_FILE and polls it, so the state is appended there instead —
// one O_APPEND write per event, so two events never tear each other.
const report = (state: string): void => {
  const file = process.env.FRIRING_SIGNAL_FILE;
  if (file) {
    try {
      appendFileSync(file, `${state}\n`);
    } catch {
      // best-effort: never surface hook errors into the agent
    }
    return;
  }
  exec(SIGNAL + state, () => {});
};

// `pi` is injected by the pi runtime; the `import type` above is erased at
// runtime, so this file has no hard dependency beyond Node's built-ins.
export default function (pi: ExtensionAPI): void {
  pi.on("session_start", () => report("idle"));
  pi.on("agent_start", () => report("working"));
  pi.on("tool_execution_start", (event?: { toolName?: string }) => {
    // A structured question to the user blocks the turn until it is answered;
    // any other tool call means the agent is actively working.
    report(event?.toolName === "ask_user_question" ? "blocked" : "working");
  });
  pi.on("agent_end", () => report("done"));
}
