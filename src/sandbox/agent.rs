//! What an agent declares in order to survive being sandboxed.
//!
//! friring is agent-neutral: it knows nothing about any agent's flags, and
//! nothing here special-cases one. Everything an agent needs is **declared
//! data** in the registry, under `[agents.<name>.sandbox]`, and this module
//! turns that declaration plus the chosen backend into the two things the
//! launch path applies — extra argv and extra writable paths.
//!
//! The declaration itself is [`crate::session::AgentSandboxDef`],
//! which lives on [`AgentDef`](crate::session::AgentDef) in the `session` layer:
//! `session` is the dependency sink and may not reference `sandbox`, so the type
//! has to be defined there for `AgentDef` to embed it.

use std::collections::BTreeMap;

use crate::sandbox::backend::InnerSandboxVerdict;
use crate::session::{AgentSandboxDef, SandboxPath, SandboxPolicy};

/// What a sandbox does to the agent's own sandbox, as data the launch path
/// applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerSandboxPlan {
    /// Why the inner sandbox is off.
    pub verdict: InnerSandboxVerdict,
    /// Whether the agent declared the flags that turn it off. When `false`
    /// friring cannot disable anything, and under seatbelt that means the agent
    /// will fail the moment it tries to sandbox a tool call.
    pub declared: bool,
    /// Tokens appended to the agent's own arguments.
    pub extra_args: Vec<String>,
    /// Environment applied alongside [`SandboxPolicy::env`].
    pub env: BTreeMap<String, String>,
    /// What became of the agent's own sandbox, e.g. `off — Friring is the
    /// boundary`. The half of [`label`](Self::label) that is not already in the
    /// profile name and the backend, so the info panel can render it beside a
    /// row that names both.
    pub state: String,
    /// The composition line the UI shows, e.g.
    /// `sandbox: dev (seatbelt) · inner agent sandbox: off — Friring is the
    /// boundary`.
    pub label: String,
}

/// Decide the inner-sandbox composition for one launch.
///
/// `verdict` comes from the chosen backend's
/// [`Caps`](crate::sandbox::backend::Caps): under seatbelt it is
/// [`InnerSandboxVerdict::Denied`], because an inner `sandbox_apply` under a
/// profile containing a deny rule returns `Operation not permitted` — the inner
/// sandbox does not merely duplicate the outer one, it cannot start.
///
/// The tradeoff is worth stating where a user will read it: with the inner
/// sandbox off, everything inside the boundary — including the agent's own
/// credentials — is reachable by whatever the agent runs. That is an argument
/// for narrow profiles, not for double sandboxing that does not work.
pub fn compose_inner_sandbox(
    policy: &SandboxPolicy,
    verdict: InnerSandboxVerdict,
    agent: Option<&AgentSandboxDef>,
) -> InnerSandboxPlan {
    let declared = agent.is_some_and(|a| !a.bypass.is_empty());
    let state = if declared {
        format!("off — {}", verdict.reason())
    } else {
        "unknown — this agent declares no bypass flags".to_string()
    };
    InnerSandboxPlan {
        verdict,
        declared,
        extra_args: agent.map(|a| a.bypass.clone()).unwrap_or_default(),
        env: agent.map(|a| a.env.clone()).unwrap_or_default(),
        label: format!(
            "sandbox: {} ({}) · inner agent sandbox: {state}",
            policy.profile, policy.backend
        ),
        state,
    }
}

/// Fold an agent's declared requirements into a resolved policy.
///
/// Adds `state_rw` to the writable set — expanded against the *target's* home,
/// like every other path — and merges the declared static environment. The
/// invariants [`SandboxPolicy`] guarantees are preserved: both path lists stay
/// sorted and de-duplicated, and a path that becomes writable leaves the
/// read-only list rather than appearing in both.
///
/// `config_dir_env` is deliberately not applied here. Relocating an agent's
/// state directory is a place backend's job; doing it under a policy backend
/// would hide the login the agent already has, which is the opposite of host
/// passthrough.
pub fn apply_agent_requirements(
    policy: &mut SandboxPolicy,
    agent: Option<&AgentSandboxDef>,
    home: &str,
) {
    let Some(agent) = agent else {
        return;
    };
    for raw in &agent.state_rw {
        let expanded = SandboxPath::workspace(raw).expanded(home);
        if expanded.is_empty() {
            continue;
        }
        policy.rw_paths.push(expanded);
    }
    policy.rw_paths.sort();
    policy.rw_paths.dedup();
    let mut readable = std::mem::take(&mut policy.ro_paths);
    readable.retain(|p| !policy.rw_paths.contains(p));
    policy.ro_paths = readable;

    for (key, value) in &agent.env {
        policy.insert_env(key.clone(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SandboxAuth, SandboxBackendKind, SandboxProfile};

    fn policy() -> SandboxPolicy {
        SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("~/.claude"),
            ],
        )
        .resolve(SandboxBackendKind::Seatbelt, "/home/u")
        .unwrap()
    }

    fn claude() -> AgentSandboxDef {
        AgentSandboxDef {
            auth: SandboxAuth::HostPassthrough,
            state_rw: vec!["~/.claude".into(), "~/.claude.json".into()],
            env: BTreeMap::from([("DISABLE_AUTOUPDATER".to_string(), "1".to_string())]),
            bypass: vec!["--dangerously-skip-permissions".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_declared_bypass_turns_the_inner_sandbox_off_with_a_reason() {
        let policy = policy();
        let agent = claude();
        let plan = compose_inner_sandbox(&policy, InnerSandboxVerdict::Denied, Some(&agent));
        assert!(plan.declared);
        assert_eq!(plan.extra_args, ["--dangerously-skip-permissions"]);
        assert_eq!(
            plan.label,
            "sandbox: dev (seatbelt) · inner agent sandbox: off — nested sandbox policies are \
             denied by the kernel"
        );
        // A backend where nesting would merely be redundant says so instead.
        let bwrap = compose_inner_sandbox(&policy, InnerSandboxVerdict::Redundant, Some(&agent));
        assert!(bwrap.label.ends_with("off — Friring is the boundary"));
    }

    #[test]
    fn an_agent_that_declares_nothing_is_reported_as_unknown() {
        let policy = policy();
        let plan = compose_inner_sandbox(&policy, InnerSandboxVerdict::Denied, None);
        assert!(!plan.declared);
        assert!(plan.extra_args.is_empty());
        assert!(plan.label.contains("declares no bypass flags"));

        // Declared, but empty, is the same thing: there is nothing to apply.
        let silent = AgentSandboxDef::default();
        let plan = compose_inner_sandbox(&policy, InnerSandboxVerdict::Denied, Some(&silent));
        assert!(!plan.declared);
    }

    #[test]
    fn state_directories_become_writable_and_leave_the_read_only_set() {
        let mut policy = policy();
        assert_eq!(policy.ro_paths, ["/home/u/.claude"]);
        apply_agent_requirements(&mut policy, Some(&claude()), "/home/u");

        assert_eq!(
            policy.rw_paths,
            ["/home/u/.claude", "/home/u/.claude.json", "/home/u/dev/app"]
        );
        // The wider grant wins; nothing may appear in both lists.
        assert!(policy.ro_paths.is_empty());
        assert_eq!(
            policy.env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn requirements_expand_against_the_targets_home_not_this_one() {
        let mut policy = policy();
        apply_agent_requirements(&mut policy, Some(&claude()), "/Users/other");
        assert!(policy
            .rw_paths
            .contains(&"/Users/other/.claude".to_string()));
        // The profile's own paths were expanded earlier, against their own home.
        assert!(policy.rw_paths.contains(&"/home/u/dev/app".to_string()));
    }

    #[test]
    fn no_declaration_leaves_the_policy_untouched() {
        let mut policy = policy();
        let before = policy.clone();
        apply_agent_requirements(&mut policy, None, "/home/u");
        assert_eq!(policy, before);
    }
}
