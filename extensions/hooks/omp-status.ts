// Managed by friring `extension install` (the built-in "hooks" extension).
// Reinstalling or updating overwrites this file — do not edit; uninstalling
// removes it. Reports Oh My Pi's lifecycle state to friring. Identity comes
// from the inherited $FRIRING_SESSION env var; every call is best-effort so it
// can never break a session running outside friring.
//
// This is an OMP (Oh My Pi, https://github.com/can1357/oh-my-pi) extension
// (TypeScript), auto-discovered from ~/.omp/agent/extensions/*.ts by the omp
// CLI. It subscribes to OMP's lifecycle events and reports them to friring's
// status reporter:
//   session_start → idle, agent_start + tool_execution_start → working
//   (a structured user-question tool call → blocked), agent_end → done.
//
// OMP is Pi-compatible, but its structured user-question tool is named `ask`
// (upstream pi uses `ask_user_question`). We treat BOTH as blocking so the dot
// turns red while OMP waits for the user instead of staying yellow (working).
import { exec } from "node:child_process";
import { appendFileSync } from "node:fs";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

// Tool names that block the turn until the user answers.
const BLOCKING_TOOLS = new Set(["ask", "ask_user_question"]);

// Exact marker prefix kept on one line so the remote (SSH/WSL) rewrite can swap
// this command for a tmux pane-option setter (there is no friring-cli on a
// remote host). Do not split the words across lines or reorder the flags.
const SIGNAL = "friring-cli session signal --state ";

// Fire-and-forget; errors are swallowed so a hook never surfaces into the
// agent. exec inherits the OMP process env, so $FRIRING_SESSION travels.
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

// `pi` is injected by the OMP runtime; the `import type` above is erased at
// runtime, so this file has no hard dependency beyond Node's built-ins.
export default function (pi: ExtensionAPI): void {
  pi.on("session_start", () => report("idle"));
  pi.on("agent_start", () => report("working"));
  pi.on("tool_execution_start", (event?: { toolName?: string }) => {
    // A structured question to the user blocks the turn until it is answered;
    // any other tool call means the agent is actively working.
    report(BLOCKING_TOOLS.has(event?.toolName ?? "") ? "blocked" : "working");
  });
  pi.on("agent_end", () => report("done"));
}
