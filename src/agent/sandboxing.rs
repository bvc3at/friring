//! Applying a session's sandbox profile to the invocation it is about to run.
//!
//! `docs/SANDBOX.md` §Launch integration: a policy backend is a *decorator* on
//! the composed invocation — argv in, longer argv out — applied at the one seam
//! both the TUI and the headless paths pass through, and **before** per-transport
//! composition (transports quote and fold differently; the Windows multiplexer
//! path collapses everything into a single token, so wrapping at the shell-string
//! level would not survive).
//!
//! Everything with a policy in it lives in [`crate::sandbox`]; this module is
//! only the glue that turns a [`SessionConfig`] into the
//! [`SandboxLaunch`] that module wants, and the
//! answer back into the three things a spawn needs: a command, its arguments,
//! and the extra environment.

use std::collections::HashMap;

use crate::sandbox::{SandboxHost, SandboxLaunch};
use crate::session::{AgentDef, SessionConfig};

/// A launch with its sandbox profile applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxedInvocation {
    /// The wrapper program (`sandbox-exec`, `bwrap`, …).
    pub command: String,
    /// The wrapper's arguments, ending in the agent's own command line.
    pub args: Vec<String>,
    /// Environment the policy and the agent's declaration ask for, merged into
    /// the tmux window's environment by the caller. A policy backend applies
    /// nothing in argv — a policy is a rule on a process, so the wrapped agent
    /// inherits the window — which makes this the only channel inward.
    pub env: HashMap<String, String>,
    /// The whole composition, for the log line: `sandbox: dev (seatbelt) ·
    /// inner agent sandbox: off — Friring is the boundary`.
    pub label: String,
    /// The same composition minus the profile name, for
    /// [`SessionInfo::sandbox_state`](crate::session::SessionInfo::sandbox_state)
    /// — the info panel already labels the row with the profile.
    pub state: String,
}

/// What [`apply`] decided about one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxDecision {
    /// The session carries no profile. The overwhelmingly common case.
    Unsandboxed,
    /// The profile applied.
    Wrapped(Box<SandboxedInvocation>),
    /// The profile could **not** be applied and its
    /// `allow_unsandboxed_fallback` switch permits launching anyway. The reason
    /// belongs in front of the user: a session the user believes is sandboxed
    /// and is not is the worst of the three outcomes.
    Skipped { reason: String },
}

/// Apply `config.sandbox` to an already-composed `(command, args)`.
///
/// `def` is the agent being launched — its `[agents.<name>.sandbox]` block
/// contributes the flags that turn the agent's *own* sandbox off (nesting is
/// denied outright under seatbelt) and the state directories it must keep
/// writable, and its credential family decides whose credential files stay
/// readable. `None` is an agent friring could not resolve a definition for; the
/// profile still applies, the agent just gets no help.
///
/// # Errors
///
/// The profile named a backend that is unavailable here, resolved to a policy
/// this build cannot express, or could not write its generated profile — and
/// the profile does not permit running unsandboxed. Failing the spawn is
/// deliberate: quietly launching an agent outside the boundary the user asked
/// for is a security regression, not a degraded mode.
pub fn apply(
    def: Option<&AgentDef>,
    config: &SessionConfig,
    command: &str,
    args: &[String],
) -> Result<SandboxDecision, String> {
    let Some(profile) = config.sandbox.as_ref() else {
        return Ok(SandboxDecision::Unsandboxed);
    };
    let fallback = profile.allow_unsandboxed_fallback;
    let home = match crate::paths::home_dir() {
        Some(home) => home.to_string_lossy().into_owned(),
        None if fallback => {
            return Ok(SandboxDecision::Skipped {
                reason: NO_HOME.to_string(),
            })
        }
        None => return Err(NO_HOME.to_string()),
    };
    match build(
        SandboxHost::local_shared(),
        &home,
        def,
        config,
        command,
        args,
    ) {
        Ok(invocation) => Ok(SandboxDecision::Wrapped(Box::new(invocation))),
        Err(reason) if fallback => Ok(SandboxDecision::Skipped { reason }),
        Err(reason) => Err(reason),
    }
}

/// Every sandbox path is expanded against a home, and a profile stores `~`
/// un-expanded precisely so it can be expanded against a *different* one.
const NO_HOME: &str = "Cannot resolve a home directory to expand sandbox paths against";

/// The whole call order `docs/SANDBOX.md` prescribes, in one place: select a
/// concrete backend, resolve the profile against it, fold in what the agent
/// declares, then wrap.
///
/// `host` and `home` are injected rather than resolved here so the composition
/// is testable on a machine that has no backend installed — and so a future
/// remote host can be wrapped by handing in its own pair.
fn build(
    host: &SandboxHost,
    home: &str,
    def: Option<&AgentDef>,
    config: &SessionConfig,
    command: &str,
    args: &[String],
) -> Result<SandboxedInvocation, String> {
    let profile = config
        .sandbox
        .as_ref()
        .ok_or_else(|| "no sandbox profile on this session".to_string())?;

    // P1 ships policy backends only, and both of them generate their artefacts
    // (a `.sb` profile file, an argv referring to local paths) on the machine
    // friring runs on. Wrapping a remote invocation would build them here and
    // hand them to a shell over there.
    if config
        .backend
        .as_deref()
        .is_some_and(crate::session::is_remote_backend)
    {
        return Err(format!(
            "Sandbox profile '{}' cannot be applied to a remote session: the policy backends \
             run on the machine friring runs on",
            profile.name
        ));
    }

    let selection = host.select(profile.backend);
    let backend = selection
        .backend()
        .map_err(|e| format!("{e}\n{}", selection.rejection_summary()))?;

    let agent_sandbox = def.and_then(|d| d.sandbox.as_ref());
    let mut policy = profile.resolve(backend, home).map_err(|e| e.to_string())?;
    crate::sandbox::apply_agent_requirements(&mut policy, agent_sandbox, home);

    let plan = host
        .inner_sandbox(backend, &policy, agent_sandbox)
        .ok_or_else(|| format!("Sandbox backend '{backend}' has no inner-sandbox verdict"))?;

    let session_key = session_key(config);
    let workspace = config
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let tmp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let database = crate::paths::database_file().map(|p| p.to_string_lossy().into_owned());
    // The credential family, not the registry name: a rebranded claude shares
    // claude's credential file, and denying it would log the agent out.
    let family = def.map(|d| d.hook_schema.as_deref().unwrap_or(&d.name));

    let mut launch = SandboxLaunch::new(&policy, home, &session_key).with_tmp_dir(&tmp_dir);
    if let Some(workspace) = workspace.as_deref() {
        launch = launch.with_workspace(workspace);
    }
    if let Some(database) = database.as_deref() {
        launch = launch.with_friring_db(database);
    }
    if let Some(family) = family {
        launch = launch.with_agent(family);
    }

    // The agent's bypass flags belong to the *agent*, so they go on the agent's
    // own argv — inside the wrapper, after everything the backend contributes.
    let mut argv = Vec::with_capacity(1 + args.len() + plan.extra_args.len());
    argv.push(command.to_string());
    argv.extend(args.iter().cloned());
    argv.extend(plan.extra_args.iter().cloned());

    let wrapped = host
        .wrap(backend, argv, &launch)
        .map_err(|e| e.to_string())?;
    let mut wrapped = wrapped.into_iter();
    let command = wrapped
        .next()
        .ok_or_else(|| format!("Sandbox backend '{backend}' produced an empty command line"))?;

    let mut env: HashMap<String, String> = policy
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.extend(plan.env.iter().map(|(k, v)| (k.clone(), v.clone())));

    Ok(SandboxedInvocation {
        command,
        args: wrapped.collect(),
        env,
        state: format!("{backend} · inner agent sandbox: {}", plan.state),
        label: plan.label,
    })
}

/// Names the generated profile file, so two sessions of one profile never race
/// on it. The friring session id when the caller pinned one (it always does on
/// a real spawn, because it is also `FRIRING_SESSION`), otherwise the agent's
/// own conversation id.
fn session_key(config: &SessionConfig) -> String {
    config
        .session_id
        .map(|id| id.to_string())
        .or_else(|| config.agent_session_id.clone())
        .unwrap_or_else(|| "session".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentSandboxDef, SandboxBackendKind, SandboxPath, SandboxProfile};

    fn agent_def() -> AgentDef {
        AgentDef {
            name: "claude".into(),
            command: "claude".into(),
            args: vec![],
            resume_args: vec![],
            fork_args: vec![],
            new_session_args: vec![],
            resume_latest: false,
            hook_schema: None,
            sandbox: Some(AgentSandboxDef {
                bypass: vec!["--dangerously-skip-permissions".into()],
                env: [("DISABLE_AUTOUPDATER".to_string(), "1".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            }),
        }
    }

    fn config_with(profile: Option<SandboxProfile>) -> SessionConfig {
        SessionConfig {
            agent: "claude".into(),
            sandbox: profile,
            ..Default::default()
        }
    }

    #[test]
    fn a_session_without_a_profile_is_left_alone() {
        let config = config_with(None);
        let decision = apply(Some(&agent_def()), &config, "claude", &["--foo".into()]).unwrap();
        assert_eq!(decision, SandboxDecision::Unsandboxed);
    }

    #[test]
    fn an_unavailable_backend_fails_the_launch_and_names_every_rung() {
        // `wsl-distro` is a place backend: unavailable in this build on every
        // host, so the assertion holds wherever the suite runs.
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::WslDistro;
        let config = config_with(Some(profile));
        let err = apply(Some(&agent_def()), &config, "claude", &[]).unwrap_err();
        assert!(err.contains("wsl-distro"), "{err}");
    }

    #[test]
    fn the_escape_hatch_turns_that_failure_into_a_reported_skip() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::WslDistro;
        profile.allow_unsandboxed_fallback = true;
        let config = config_with(Some(profile));
        let decision = apply(Some(&agent_def()), &config, "claude", &[]).unwrap();
        let SandboxDecision::Skipped { reason } = decision else {
            panic!("expected a skip, got {decision:?}");
        };
        assert!(reason.contains("wsl-distro"), "{reason}");
    }

    #[test]
    fn a_remote_session_is_refused_rather_than_wrapped_locally() {
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        let mut config = config_with(Some(profile));
        config.backend = Some("ssh:devbox".into());
        let err = apply(Some(&agent_def()), &config, "claude", &[]).unwrap_err();
        assert!(err.contains("remote session"), "{err}");
    }

    /// A host with bwrap and nothing else, so the whole composition runs on a
    /// machine that has no sandbox installed. Nothing here touches a real home:
    /// the profile's paths and the secrets deny list are both expanded against
    /// the fabricated `home` passed in.
    fn stub_host() -> SandboxHost {
        SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::linux_with_bwrap("0.11.0"),
        ))
    }

    #[test]
    fn the_wrapper_surrounds_the_agent_and_appends_its_bypass_flags() {
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        let mut config = config_with(Some(profile));
        config.cwd = Some("/fabricated/home/dev/app".into());
        let def = agent_def();

        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            Some(&def),
            &config,
            "claude",
            &["--resume".into(), "abc".into()],
        )
        .unwrap();

        assert_eq!(wrapped.command, crate::sandbox::bwrap::BWRAP);
        // The agent's own command line is last, after the backend's `--`, with
        // the bypass flags appended to *its* arguments rather than the
        // wrapper's.
        let tail: Vec<&str> = wrapped
            .args
            .iter()
            .skip_while(|a| *a != "--")
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            [
                "--",
                "claude",
                "--resume",
                "abc",
                "--dangerously-skip-permissions"
            ]
        );
        assert!(wrapped
            .state
            .starts_with("bwrap · inner agent sandbox: off"));
    }

    /// `docs/SANDBOX.md` §Launch integration: the session identity variables
    /// are set on the tmux window, *outside* a policy boundary, and inherited
    /// through it. Wrapping must therefore only ever **add** environment —
    /// dropping `FRIRING_SESSION` kills status reporting silently.
    #[test]
    fn wrapping_adds_environment_and_never_replaces_the_sessions_own() {
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        let mut config = config_with(Some(profile));
        config
            .env
            .insert("FRIRING_SESSION".into(), "session-uuid".into());
        config
            .env
            .insert("FRIRING_SESSION_ID".into(), "agent-uuid".into());
        let def = agent_def();

        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            Some(&def),
            &config,
            "claude",
            &[],
        )
        .unwrap();

        // The wrap contributes the agent's declared environment and nothing
        // that would shadow an identity variable.
        assert_eq!(
            wrapped.env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );
        assert!(!wrapped.env.contains_key("FRIRING_SESSION"));
        // Neither backend applies environment in argv (a policy is a rule on a
        // process, so the wrapped agent inherits the window) — so the identity
        // variables must not appear there either, or they would be duplicated
        // into a place the sandbox layer cannot keep in sync.
        assert!(!wrapped.args.iter().any(|a| a.contains("FRIRING_SESSION")));

        // What the caller does with the two halves.
        let mut env = config.env.clone();
        env.extend(wrapped.env);
        assert_eq!(
            env.get("FRIRING_SESSION").map(String::as_str),
            Some("session-uuid")
        );
        assert_eq!(
            env.get("FRIRING_SESSION_ID").map(String::as_str),
            Some("agent-uuid")
        );
        assert_eq!(
            env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );
    }

    /// An agent with no `[agents.<name>.sandbox]` block still launches — it
    /// just gets no help, which is the agent-neutrality rule.
    #[test]
    fn an_agent_that_declares_nothing_is_still_wrapped() {
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        let config = config_with(Some(profile));
        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config,
            "aider",
            &[],
        )
        .unwrap();
        assert_eq!(wrapped.command, crate::sandbox::bwrap::BWRAP);
        assert_eq!(wrapped.args.last().map(String::as_str), Some("aider"));
        assert!(wrapped.state.contains("declares no bypass flags"));
    }

    /// The one shape assertion that does not need a backend to be installed:
    /// the session key is what names the generated profile, and a spawn always
    /// pins the friring id before launch.
    #[test]
    fn the_session_key_prefers_the_friring_id() {
        let mut config = config_with(None);
        assert_eq!(session_key(&config), "session");
        config.agent_session_id = Some("agent-conversation".into());
        assert_eq!(session_key(&config), "agent-conversation");
        let id = crate::session::SessionId::default();
        config.session_id = Some(id);
        assert_eq!(session_key(&config), id.to_string());
    }
}
