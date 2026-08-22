// Managed by friring `extension install` (the built-in "hooks" extension).
// Reinstalling or updating overwrites this file — do not edit; uninstalling
// removes it. Reports opencode's lifecycle state to friring via
// `friring-cli session signal`. Identity comes from the inherited
// $FRIRING_SESSION env var; every call is best-effort so it can never break a
// session running outside friring.
//
// Inside a sandbox that binary need not exist, and the database it writes is
// out of reach by design (docs/SANDBOX.md ADR-29 — an agent that can write it
// can schedule host commands). friring then exports $FRIRING_SIGNAL_FILE and
// polls it, so the state is appended there instead. The append is O_APPEND, so
// two events never tear each other; the import is lazy so a runtime without
// node:fs falls through to the CLI exactly as before.
export const FriringStatus = async ({ $ }) => {
  const signal = async (state) => {
    try {
      const file = process.env.FRIRING_SIGNAL_FILE;
      if (file) {
        const { appendFileSync } = await import("node:fs");
        appendFileSync(file, `${state}\n`);
        return;
      }
      await $`friring-cli session signal --state ${state}`.quiet().nothrow();
    } catch (_) {
      // best-effort: never surface hook errors into the agent
    }
  };
  return {
    "chat.message": async () => {
      await signal("working");
    },
    event: async ({ event }) => {
      if (!event || !event.type) return;
      if (event.type === "session.created") await signal("idle");
      else if (event.type === "permission.asked") await signal("blocked");
      else if (event.type === "session.idle") await signal("done");
    },
  };
};
