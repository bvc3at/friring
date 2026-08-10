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

/// Re-exported because `session_ops` may not reference [`crate::sandbox`] at
/// all (`tests/architecture_rules.rs`), and the headless launch paths have to
/// name the handle they hold across a kill and a spawn.
pub use crate::sandbox::PendingEgress;

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
    /// The same composition minus the profile name: the payload of the
    /// [`Applied`](crate::session::SandboxState::Applied) half of
    /// [`SessionInfo::sandbox_state`](crate::session::SessionInfo::sandbox_state)
    /// — the info panel already labels the row with the profile.
    pub state: String,
    /// The **place** this invocation runs in, for a place backend (ADR-26).
    /// `None` for a policy backend, whose boundary is the wrapped process and
    /// whose tmux window is on the host.
    ///
    /// The caller spawns through this instead of the session's own backend: it
    /// is the transport, and its [`name`](crate::agent::SessionBackend::name)
    /// is the `sandbox:<profile>` that lands in `backend_type` and drives
    /// restore.
    pub place: Option<crate::agent::transport::Place>,
    /// The place row to record in `sandbox_instances`, so garbage collection
    /// can find a container this launch created. `None` for a policy backend,
    /// which creates nothing that outlives the process.
    pub instance: Option<crate::sandbox::SandboxInstance>,
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
/// A filtered profile binds an egress proxy here, because argv has to name the
/// port or the socket. That instance is **provisional**: the session keeps
/// whatever it is already using until the launch this composed for is running.
/// Every caller must therefore take [`pending_egress`] before it can fail and
/// [`commit`](PendingEgress::commit) it once the pane exists — the boundary is
/// released, and the session's own left alone, on every path that does not.
///
/// # Errors
///
/// The profile named a backend that is unavailable here, resolved to a policy
/// this build cannot express, could not write its generated profile, could not
/// be given the egress proxy its network mode is enforced by, or asks for a
/// boundary friring will not grant — a read-write path reaching the database or
/// the host's tmux socket, a path that is not valid UTF-8 — and the profile
/// does not permit running unsandboxed. Failing the spawn is deliberate:
/// quietly launching an agent outside the boundary the user asked for is a
/// security regression, not a degraded mode.
pub fn apply(
    def: Option<&AgentDef>,
    config: &SessionConfig,
    command: &str,
    args: &[String],
) -> Result<SandboxDecision, String> {
    let Some(profile) = config.sandbox.as_ref() else {
        // A session whose profile was cleared keeps nothing running — but not
        // before this launch happens, because until then the agent still
        // running is the one that boundary belongs to. Free for the
        // overwhelmingly common case: no supervisor is started to say so.
        PendingEgress::clearing(&session_key(config)).park();
        return Ok(SandboxDecision::Unsandboxed);
    };
    let fallback = profile.allow_unsandboxed_fallback;
    let home = match crate::paths::home_dir() {
        Some(home) => match representable("the home directory", &home) {
            Ok(home) => home,
            Err(reason) if fallback => return Ok(SandboxDecision::Skipped { reason }),
            Err(reason) => return Err(reason),
        },
        None if fallback => {
            return Ok(SandboxDecision::Skipped {
                reason: NO_HOME.to_string(),
            })
        }
        None => return Err(NO_HOME.to_string()),
    };
    match with_host(|host| build(host, &home, def, config, command, args)) {
        Ok(invocation) => Ok(SandboxDecision::Wrapped(Box::new(invocation))),
        Err(reason) if fallback => Ok(SandboxDecision::Skipped { reason }),
        Err(reason) => Err(reason),
    }
}

/// Take the egress instance the composition for this session prepared, so the
/// launch that is about to happen owns it.
///
/// Always answers, so a launch path treats every session the same way: a
/// session with no profile — or one whose network mode the kernel enforces on
/// its own — gets a handle with nothing to commit. Call it immediately after
/// [`apply`], before anything that can fail, and
/// [`commit`](PendingEgress::commit) it once the agent's pane exists.
pub fn pending_egress(config: &SessionConfig) -> PendingEgress {
    crate::sandbox::egress::claim(&session_key(config))
}

/// Run `wrap` against the host friring itself runs on — or, in a test, the one
/// it installed.
fn with_host<R>(wrap: impl FnOnce(&SandboxHost) -> R) -> R {
    #[cfg(test)]
    if let Some(host) = TEST_HOST.with(|installed| installed.borrow().clone()) {
        return wrap(&host);
    }
    wrap(SandboxHost::local_shared())
}

#[cfg(test)]
thread_local! {
    /// See [`TestSandboxHost`].
    static TEST_HOST: std::cell::RefCell<Option<std::sync::Arc<SandboxHost>>> =
        const { std::cell::RefCell::new(None) };
}

/// Compose against a fabricated host for as long as this guard lives — the
/// sandbox twin of [`crate::paths::TestPathGuard`], and thread-local for the
/// same reason.
///
/// [`apply`] resolves the backend from the machine friring runs on, so without
/// this every test of a *launch* would assert something different depending on
/// whether the host it ran on happened to have seatbelt or bubblewrap
/// installed. Nothing here executes a sandbox: the wrapped argv is composed and
/// handed to a stub.
#[cfg(test)]
pub(crate) struct TestSandboxHost;

#[cfg(test)]
impl TestSandboxHost {
    pub(crate) fn new(host: SandboxHost) -> Self {
        let host = std::sync::Arc::new(host);
        TEST_HOST.with(|installed| *installed.borrow_mut() = Some(host));
        Self
    }

    /// A host offering seatbelt, whatever this machine is.
    ///
    /// The one shape that composes end to end inside a test process: the
    /// transport is host loopback, so no relay — and therefore no
    /// `friring-cli` beside the test binary — is involved. Named rather than
    /// built by the caller because `session_ops` may not reference
    /// [`crate::sandbox`] at all, and its launch paths need this too.
    pub(crate) fn seatbelt() -> Self {
        Self::new(SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::macos(26, true),
        )))
    }
}

#[cfg(test)]
impl Drop for TestSandboxHost {
    fn drop(&mut self) {
        TEST_HOST.with(|installed| *installed.borrow_mut() = None);
    }
}

/// Every sandbox path is expanded against a home, and a profile stores `~`
/// un-expanded precisely so it can be expanded against a *different* one.
const NO_HOME: &str = "Cannot resolve a home directory to expand sandbox paths against";

/// A security-relevant path as the sandbox layer needs it: exactly, or not at
/// all.
///
/// Every rule a backend writes is a string. A lossy conversion turns a
/// non-UTF-8 byte into `U+FFFD`, and the rule then names a path that does not
/// exist while the real one stays visible — an ADR-29 deny that denies nothing,
/// a secrets deny that hides nothing. The launch is refused instead, in front of
/// the user, wherever a path of this kind cannot be spelled.
fn representable(what: &str, path: &std::path::Path) -> Result<String, String> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        format!(
            "Cannot apply a sandbox profile: {what} ('{}') is not valid UTF-8, and a sandbox rule \
             built from an approximation of it would name a different file",
            path.display()
        )
    })
}

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

    let selection = host.select(profile.backend);
    let backend = selection
        .backend()
        .map_err(|e| format!("{e}\n{}", selection.rejection_summary()))?;
    let shape = backend.shape();

    // Both policy backends generate their artefacts (a `.sb` profile file, an
    // argv referring to local paths) on the machine friring runs on, so wrapping
    // a remote invocation would build them here and hand them to a shell over
    // there. A **place** is exempt, and not by omission: a place *is* the
    // elsewhere — friring reaches it through its own transport rather than
    // through the session's, so the session's own backend says nothing about
    // where the boundary is applied.
    if shape != Some(crate::session::SandboxShape::Place)
        && config
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

    let agent_sandbox = def.and_then(|d| d.sandbox.as_ref());
    let mut policy = profile.resolve(backend, home).map_err(|e| e.to_string())?;
    crate::sandbox::apply_agent_requirements(&mut policy, agent_sandbox, home);

    let plan = host
        .inner_sandbox(backend, &policy, agent_sandbox)
        .ok_or_else(|| format!("Sandbox backend '{backend}' has no inner-sandbox verdict"))?;
    let transport = host
        .backend(backend)
        .ok_or_else(|| format!("Sandbox backend '{backend}' is not built into this friring"))?
        .capabilities()
        .proxy_transport;

    let session_key = session_key(config);
    let workspace = config
        .cwd
        .as_ref()
        .map(|p| representable("the session's working directory", p))
        .transpose()?;

    // A place exists before the launch does: the container has to be running
    // for the transport to reach its tmux, for the relay port to be one no
    // sibling session holds, and for the relay binary inside it to be resolved.
    // Ensuring is idempotent, so a relaunch adopts the same place.
    let ensured = match shape {
        Some(crate::session::SandboxShape::Place) => Some(
            host.container(backend)
                .ok_or_else(|| {
                    format!("Sandbox backend '{backend}' is a place this friring cannot create")
                })?
                .ensure_place(profile)
                .map_err(|e| e.to_string())?,
        ),
        _ => None,
    };

    // Never the host temp root: `/tmp` holds friring's own tmux socket, and a
    // read-write grant over it is a complete escape (ADR-29's sibling problem —
    // see `crate::sandbox::dirs`). friring mints a private per-session directory
    // instead, adopting one left behind by a crashed run. A place's lives under
    // that place's own tree, because the whole tree is what is mounted in.
    let tmp_dir = match &ensured {
        Some(_) => crate::sandbox::create_place_session_dir(&profile.name, &session_key),
        None => crate::sandbox::create_session_scratch(&session_key),
    }
    .map_err(|e| e.to_string())
    .and_then(|dir| representable("the sandbox scratch directory", &dir))?;

    // The one channel out of a policy boundary (ADR-29): the agent's hooks
    // append a state word to a file here and the status poll takes it, because
    // the database `friring-cli session signal` writes is what a sandbox may
    // never reach. A place needs no such directory — its hooks are not
    // projected in at all yet (see `place_note`), and when they are they will
    // reach friring the way an SSH host's do, over the tmux window it already
    // owns.
    let signals = match &ensured {
        None => Some(
            crate::paths::create_session_signal_dir(&session_key)
                .map_err(|e| format!("Cannot apply a sandbox profile: {e}"))?,
        ),
        Some(_) => None,
    };
    let signal_dir = signals
        .as_ref()
        .map(|channel| representable("the sandbox signal directory", &channel.dir))
        .transpose()?;
    if let Some(channel) = &signals {
        let file = representable("the sandbox signal file", &channel.file)?;
        // Inserted on the *policy*, whose environment is the launch's last word,
        // so an agent that declares `FRIRING_SIGNAL_FILE` in the registry cannot
        // point the channel somewhere friring does not read.
        policy.insert_env(crate::paths::SIGNAL_FILE_ENV, file);
    }

    let database = crate::paths::database_file()
        .map(|p| representable("friring's database", &p))
        .transpose()?;
    // The credential family, not the registry name: a rebranded claude shares
    // claude's credential file, and denying it would log the agent out.
    let family = def.map(|d| d.hook_schema.as_deref().unwrap_or(&d.name));

    // The egress proxy, before anything is launched: a mode the kernel cannot
    // express on its own (an allowlist, or denies under `full`) is enforced
    // there and nowhere else, so the boundary is not composable until it is
    // listening. A failure here refuses the launch — `apply` turns that into
    // the profile's own `allow_unsandboxed_fallback` decision — rather than
    // starting an agent that believes it is filtered and is not.
    //
    // `pending` is what keeps the instance from being the session's before the
    // launch is: holding it here means every `?` below releases it, and the
    // session's current boundary — a healthy agent's way out — is untouched
    // until the launch path commits.
    let (proxy, relay_port, pending) = if crate::sandbox::egress::proxy_required(&policy) {
        // The key is the boundary's identity: the token, the socket and the
        // first-use answers are all per session, and two launches sharing one
        // key share all three — starting the second would replace the first's
        // instance mid-run, and one session's "allow this domain?" would widen
        // the other's. A launch with no id of its own has no boundary to be
        // told apart by, so it is refused rather than filed under the fallback.
        if session_key == UNIDENTIFIED_SESSION {
            return Err(
                "This launch has no session id, so its egress boundary could not be told \
                 apart from another's"
                    .to_string(),
            );
        }
        // A namespaced policy sandbox gets a private loopback, so its relay can
        // use one fixed port. A place is shared by its profile's sessions and so
        // is its loopback, so each takes a port of its own and keeps it across
        // relaunches — and the address the proxy environment names has to be
        // that one, or every session after the first would fail closed.
        let relay = match (&ensured, host.container(backend)) {
            (Some(place), Some(container)) => container
                .relay_port(&place.instance.external_id, &session_key)
                .map(Some)
                .map_err(|e| e.to_string())?,
            _ => None,
        };
        let prepared = crate::sandbox::egress::prepare_at(
            &session_key,
            &policy,
            transport,
            std::path::Path::new(&tmp_dir),
            relay.map_or_else(crate::sandbox::egress::relay_addr, |port| {
                (std::net::Ipv4Addr::LOCALHOST, port).into()
            }),
        )
        .map_err(|e| e.to_string())?;
        for (key, value) in prepared.grant.env {
            policy.insert_env(key, value);
        }
        (Some(prepared.grant.endpoint), relay, prepared.pending)
    } else {
        // A profile edited from `allowlist` to `full` or `none` must not leave
        // the previous launch's listener behind — once this launch is the one
        // running, and not before.
        (None, None, PendingEgress::clearing(&session_key))
    };

    let mut launch = SandboxLaunch::new(&policy, home, &session_key).with_tmp_dir(&tmp_dir);
    if let Some(endpoint) = proxy {
        launch = launch.with_proxy(endpoint);
    }
    if let Some(dir) = signal_dir.as_deref() {
        launch = launch.with_signal_dir(dir);
    }
    if let Some(place) = &ensured {
        // A relay exactly when the launch is proxied. `ensure_place` refuses a
        // filtered profile whose image carries no `friring-cli`, so the two are
        // already consistent — refusing here rather than composing an empty
        // program keeps that an invariant of this function too.
        let relay = match relay_port {
            Some(port) => Some(crate::sandbox::PlaceRelay {
                program: place.relay_program.as_deref().ok_or_else(|| {
                    format!(
                        "Sandbox profile '{}' filters egress, and the place friring ensured for \
                         it reported no relay binary inside it",
                        profile.name
                    )
                })?,
                port,
            }),
            None => None,
        };
        launch = launch.with_place(crate::sandbox::PlaceLaunch { relay });
    }
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
    // A place has none of the host's filesystem it did not ask for, so an arg
    // naming a friring-managed config file (claude's `--settings <config
    // dir>/hooks/claude.json`) points at nothing in there — and an agent handed
    // a settings path that does not exist dies on startup. Until config
    // projection lands the flag is dropped, which is the same treatment a host
    // with no POSIX place for the file already gets.
    let dropped_config = if ensured.is_some() {
        let (kept, dropped) = crate::agent::config_args::without_config_paths(argv);
        argv = kept;
        dropped
    } else {
        Vec::new()
    };

    // A wrap that fails takes `pending` down with the stack: nothing is going to
    // use the instance prepared for this launch, and the session's own — which
    // this composition has not touched — keeps serving whatever is running.
    let wrapped = host
        .wrap(backend, argv, &launch)
        .map_err(|e| e.to_string())?;
    let mut wrapped = wrapped.into_iter();
    let command = wrapped
        .next()
        .ok_or_else(|| format!("Sandbox backend '{backend}' produced an empty command line"))?;

    // The agent's declared environment first, the *policy's* last: an agent
    // that declares `HTTP_PROXY` in the registry must not shadow the boundary's
    // own, which would route it around the allowlist. `apply_agent_requirements`
    // has already folded the agent's declarations into the policy, so nothing
    // an agent asked for is lost by the precedence — only overruled where the
    // boundary has an answer of its own.
    let mut env: HashMap<String, String> = plan
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.extend(policy.env.iter().map(|(k, v)| (k.clone(), v.clone())));

    // The place's address for the transport, built from the engine path the
    // probe vetted rather than a bare name an inherited `PATH` could re-resolve.
    let place = match &ensured {
        Some(ensured) => Some(
            crate::agent::transport::Place::new(
                host.container(backend)
                    .ok_or_else(|| format!("Sandbox backend '{backend}' is not a place"))?
                    .engine_program()
                    .map_err(|e| e.to_string())?,
                &ensured.instance.external_id,
                &profile.name,
            )
            .map_err(|e| format!("Cannot reach the sandbox place: {e:#}"))?,
        ),
        None => None,
    };

    // Composed, so the instance survives this stack — but as the *launch's*,
    // not the session's. Whoever spawns the pane claims it with
    // `pending_egress` and commits it once there is something behind it.
    pending.park();

    let note = place_note(ensured.is_some(), &dropped_config);
    Ok(SandboxedInvocation {
        command,
        args: wrapped.collect(),
        env,
        state: format!("{backend} · inner agent sandbox: {}{note}", plan.state),
        label: format!("{}{note}", plan.label),
        place,
        instance: ensured.map(|ensured| ensured.instance),
    })
}

/// What a place-backed launch has to say for itself beyond the boundary it
/// applied, or `""` for a policy launch.
///
/// Config projection is not built yet, so a place starts from a synthetic
/// per-profile home with none of the host's agent configuration in it (ADR-28
/// forbids binding the real one): the agent has to sign in inside the pane, and
/// where friring's own hook config was among the arguments it was dropped, so
/// the session reports no status. Both are recoverable and neither is
/// self-explanatory, so the composition says so wherever it is shown rather than
/// leaving the user with a session that silently never leaves `idle`.
fn place_note(place: bool, dropped_config: &[String]) -> String {
    if !place {
        return String::new();
    }
    let mut note = " · no host config projected — sign in inside the pane".to_string();
    if !dropped_config.is_empty() {
        note.push_str("; friring's hooks were dropped, so it reports no status");
    }
    note
}

/// Drop the per-session state a sandboxed launch minted: its egress proxy, the
/// scratch directory the agent wrote, and the policy file generated for it.
///
/// Call this when a session ends or is deleted. Skipping the directories costs
/// disk rather than correctness — the next launch of the same session adopts
/// what is there, which is what makes a crashed run recoverable — but an
/// agent's writable scratch should not outlive the agent, and its way out
/// certainly should not: the proxy is stopped **first**, so the socket is
/// unlinked by the process that bound it rather than pulled out from under a
/// live listener. Harmless for a session that never had a profile.
pub fn cleanup(config: &SessionConfig) {
    cleanup_key(&session_key(config));
}

/// The same cleanup for a teardown path that holds a persisted session row
/// rather than the [`SessionConfig`] the launch was composed from.
///
/// A real spawn always pins `SessionConfig::session_id` (it is also
/// `FRIRING_SESSION`), so the friring session id *is* the launch key the
/// wrapped invocation was keyed on — and a session that never had one minted
/// nothing to drop. Exists so `session_ops`, which may not reference
/// [`crate::sandbox`], still reaches the key derivation that lives here.
pub fn cleanup_by_session_id(session_id: crate::session::SessionId) {
    cleanup_key(&session_id.to_string());
}

/// Everything one launch key owns, in the order that keeps a live listener from
/// being pulled out from under itself.
fn cleanup_key(key: &str) {
    crate::sandbox::egress::stop(key);
    crate::sandbox::cleanup_session(key);
    // A place-backed session's writable directory is inside the place's tree
    // rather than the policy scratch root, because the whole tree is what is
    // mounted in — so both are dropped, and each is a no-op for the shape that
    // did not use it.
    crate::sandbox::dirs::cleanup_place_session(key);
    crate::paths::remove_session_signal_dir(key);
    // A place outlives its sessions, so nothing here removes one — but the
    // loopback port this session held inside it is the place's to hand out
    // again, and a place with a bounded span of them would otherwise run out
    // after enough sessions had come and gone.
    with_host(|host| {
        for kind in [
            crate::session::SandboxBackendKind::Docker,
            crate::session::SandboxBackendKind::Podman,
        ] {
            if let Some(container) = host.container(kind) {
                container.release_relay_ports(key);
            }
        }
    });
}

/// Ensure `profile`'s place exists and answer with the transport that reaches
/// it, plus the instance to record.
///
/// The restore path's half of the launch composition: a session persisted with
/// `backend_type = sandbox:<profile>` has to be reached before it can be
/// adopted, and the place it names may be stopped (a host reboot) or gone (an
/// engine that was pruned). Ensuring is idempotent, so a place that is already
/// running costs one `inspect`.
///
/// Blocking, and deliberately so: this runs a container engine, which on a cold
/// place is seconds. Callers keep it off the render path.
///
/// # Errors
///
/// The engine is unavailable, the profile cannot be resolved against it, or the
/// place will not start — each with the backend's own actionable sentence.
pub fn open_place(
    profile: &crate::session::SandboxProfile,
) -> Result<
    (
        std::sync::Arc<dyn crate::agent::SessionBackend>,
        crate::sandbox::SandboxInstance,
    ),
    String,
> {
    with_host(|host| {
        let backend = host
            .select(profile.backend)
            .backend()
            .map_err(|e| e.to_string())?;
        let container = host.container(backend).ok_or_else(|| {
            format!(
                "Sandbox profile '{}' resolves to '{backend}', which is not a place this friring \
                 can open",
                profile.name
            )
        })?;
        let ensured = container.ensure_place(profile).map_err(|e| e.to_string())?;
        let place = crate::agent::transport::Place::new(
            container.engine_program().map_err(|e| e.to_string())?,
            &ensured.instance.external_id,
            &profile.name,
        )
        .map_err(|e| format!("Cannot reach the sandbox place: {e:#}"))?;
        Ok((
            crate::agent::backend::place_backend(&place),
            ensured.instance,
        ))
    })
}

/// The place a profile's sessions are running in **right now**, or `None`.
///
/// Deliberately does not create one: the callers are teardown paths, and
/// starting a container in order to kill a pane inside it — or in order to
/// discover there is none — is the opposite of what they are for. It asks each
/// engine friring can drive for the containers *it* created, and picks the one
/// carrying this profile's label, so a profile whose `auto` backend has changed
/// since the launch is still found.
///
/// Exists here because `session_ops` may not reference [`crate::sandbox`] at
/// all, and the transport address is assembled from two things only this layer
/// holds: the engine path the probe vetted, and the engine's own container id.
pub fn running_place(profile: &str) -> Option<crate::agent::transport::Place> {
    with_host(|host| {
        for kind in [
            crate::session::SandboxBackendKind::Docker,
            crate::session::SandboxBackendKind::Podman,
        ] {
            let Some(container) = host.container(kind) else {
                continue;
            };
            let Ok(engine) = container.engine_program() else {
                continue;
            };
            let Ok(places) = container.live_places() else {
                continue;
            };
            let found = places
                .into_iter()
                .find(|place| place.owned && place.profile.as_deref() == Some(profile))
                .and_then(|place| {
                    crate::agent::transport::Place::new(engine, &place.id, profile).ok()
                });
            if found.is_some() {
                return found;
            }
        }
        None
    })
}

/// The key a launch that pinned neither id falls back to.
///
/// A constant, so two such launches collide — which for the scratch directory
/// is a shared writable directory and no worse, and for the egress boundary
/// would be a shared credential and a shared socket. `build` refuses a proxied
/// launch under this key rather than filing two boundaries under one name.
const UNIDENTIFIED_SESSION: &str = "session";

/// Names the generated profile file and the scratch directory, so two sessions
/// of one profile never race on either. The friring session id when the caller
/// pinned one (it always does on a real spawn, because it is also
/// `FRIRING_SESSION`), otherwise the agent's own conversation id, and
/// [`UNIDENTIFIED_SESSION`] when there is neither.
fn session_key(config: &SessionConfig) -> String {
    config
        .session_id
        .map(|id| id.to_string())
        .or_else(|| config.agent_session_id.clone())
        .unwrap_or_else(|| UNIDENTIFIED_SESSION.to_string())
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
        let profile = closed_profile();
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

    /// Pin friring's data directory somewhere short, private and fabricated,
    /// for the tests that compose a real launch.
    ///
    /// A bubblewrap launch binds `<data>/sandbox/tmp/<key>/proxy.sock`, and a
    /// unix socket path has to fit in `sun_path` (103 bytes) — which the
    /// default unit-test base, several directories under the platform temp
    /// directory, does not leave room for on macOS. Held for the test's
    /// lifetime: the override is thread-local and resets on drop.
    fn fabricated_data_dir(name: &str) -> crate::paths::TestPathGuard {
        let base = std::env::temp_dir().join(format!("frs{}-{name}", std::process::id()));
        crate::paths::TestPathGuard::new(base)
    }

    /// A profile with no egress at all.
    ///
    /// The tests about argv and environment *shape* use this so they compose
    /// one thing: the default `allowlist` starts a proxy and — under bwrap —
    /// a relay, which the egress tests below exercise deliberately.
    fn closed_profile() -> SandboxProfile {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = crate::session::NetworkMode::None;
        profile
    }

    /// Where the stub host's bubblewrap lives. A launch runs the path the probe
    /// resolved, never the bare [`crate::sandbox::bwrap::BWRAP`] name.
    const STUB_BWRAP: &str = "/usr/bin/bwrap";

    #[test]
    fn the_wrapper_surrounds_the_agent_and_appends_its_bypass_flags() {
        let profile = closed_profile();
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

        assert_eq!(wrapped.command, STUB_BWRAP);
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
        let profile = closed_profile();
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
        let profile = closed_profile();
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
        assert_eq!(wrapped.command, STUB_BWRAP);
        assert_eq!(wrapped.args.last().map(String::as_str), Some("aider"));
        assert!(wrapped.state.contains("declares no bypass flags"));
    }

    /// A lossy conversion would turn a non-UTF-8 byte into `U+FFFD`, and every
    /// rule built from the result would name a path that does not exist — an
    /// ADR-29 deny that denies nothing while the real database stays visible.
    /// Refusing is the only honest answer.
    #[cfg(unix)]
    #[test]
    fn a_path_that_cannot_be_spelled_exactly_refuses_the_launch() {
        use std::os::unix::ffi::OsStringExt as _;

        let profile = closed_profile();
        let mut config = config_with(Some(profile));
        config.cwd = Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(
            vec![b'/', b'w', 0xff, b'k'],
        )));
        let err = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert!(err.contains("working directory"), "{err}");
    }

    /// The scratch directory is friring's own, per session, and adopted rather
    /// than re-created when a crashed run left one behind. It is emphatically
    /// not the host temp root, which holds friring's tmux socket.
    #[test]
    fn the_launch_mints_its_own_scratch_directory() {
        let profile = closed_profile();
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some("scratch-mint-test".into());
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

        let scratch = crate::sandbox::dirs::session_scratch_dir("scratch-mint-test").unwrap();
        assert!(
            scratch.is_dir(),
            "the scratch directory must exist by launch"
        );
        let scratch = scratch.display().to_string();
        assert!(
            wrapped.args.contains(&scratch),
            "the sandbox was not given its scratch directory: {:?}",
            wrapped.args
        );
        let host_temp = std::env::temp_dir().display().to_string();
        assert!(
            !wrapped.args.contains(&host_temp),
            "the host temp root must never be granted: {:?}",
            wrapped.args
        );

        crate::agent::sandboxing::cleanup(&config);
        assert!(!std::path::Path::new(&scratch).exists());
    }

    /// A macOS host, where the egress transport is host loopback and no relay
    /// is involved — the shape that composes end to end in a test process.
    fn mac_host() -> SandboxHost {
        SandboxHost::new(std::sync::Arc::new(crate::sandbox::probe::StubHost::macos(
            26, true,
        )))
    }

    /// The seatbelt port the kernel policy opens — which is the one the
    /// composed launch would dial.
    fn proxy_port(wrapped: &SandboxedInvocation) -> u16 {
        let param = wrapped
            .args
            .iter()
            .find(|a| a.starts_with("PROXY="))
            .unwrap_or_else(|| panic!("no proxy parameter in {:?}", wrapped.args));
        param
            .trim_start_matches("PROXY=localhost:")
            .parse()
            .unwrap_or_else(|e| panic!("{param}: {e}"))
    }

    /// Wait for every egress command queued so far to have been handled: the
    /// supervisor answers one at a time, so a reply to a later one is proof the
    /// earlier ones are done.
    fn settle(session_key: &str) -> Option<Vec<String>> {
        crate::sandbox::egress::running_allow_rules(session_key)
    }

    /// A boundary needs a name of its own, and a launch that has none is
    /// refused rather than filed under the shared fallback.
    ///
    /// Two such launches would share one key, and everything the key owns is
    /// per boundary: starting the second replaces the first's instance while
    /// its agent is still running, they hold one token and one socket, and one
    /// session's first-use answer widens the other's allowlist. The scratch
    /// directory tolerates the collision; the boundary must not.
    #[test]
    fn a_launch_with_no_session_id_gets_no_boundary_to_share() {
        let _guard = fabricated_data_dir("unidentified");
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let config = config_with(Some(profile));
        assert_eq!(session_key(&config), UNIDENTIFIED_SESSION);

        let refusal = build(
            &mac_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
        )
        .expect_err("an unidentifiable boundary must not be composed");
        assert!(refusal.contains("no session id"), "{refusal}");
        assert_eq!(
            settle(UNIDENTIFIED_SESSION),
            None,
            "a refused launch left an instance behind"
        );

        // The same profile with an id composes: it is the anonymity that is
        // refused, not the profile.
        let mut identified = config.clone();
        identified.agent_session_id = Some("egress-identified".into());
        build(
            &mac_host(),
            "/fabricated/home",
            None,
            &identified,
            "claude",
            &[],
        )
        .expect("an identified launch composes");
        cleanup(&identified);
    }

    /// Composing is not launching. The instance is bound — argv has to name its
    /// port — but it belongs to nobody until a pane exists, so the launch path
    /// claims it and commits it, and a composition thrown away in between
    /// leaves nothing running.
    #[test]
    fn a_composition_leaves_its_boundary_for_the_launch_to_claim() {
        let _guard = fabricated_data_dir("claim");
        let key = "egress-claim";
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some(key.into());

        let wrapped = build(
            &mac_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
        )
        .unwrap();
        let port = proxy_port(&wrapped);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        assert_eq!(
            settle(key),
            None,
            "composing must not hand the session a boundary it is not running behind"
        );

        let pending = pending_egress(&config);
        assert!(pending.is_pending(), "the launch found nothing to claim");
        assert!(
            !pending_egress(&config).is_pending(),
            "a second claim must not take the same instance twice"
        );
        pending.commit();
        assert_eq!(
            settle(key),
            Some(vec!["api.anthropic.com".to_string()]),
            "the launch's boundary is the session's once it has a pane"
        );

        cleanup(&config);
    }

    /// The other half: a launch that never happens releases what it composed,
    /// rather than leaving a listener and a token with no session behind them.
    #[test]
    fn a_launch_that_never_happens_releases_the_boundary_it_composed() {
        let _guard = fabricated_data_dir("released");
        let key = "egress-released";
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some(key.into());

        let wrapped = build(
            &mac_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
        )
        .unwrap();
        let port = proxy_port(&wrapped);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());

        drop(pending_egress(&config));
        settle(key);
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
            "the proxy for a launch that never happened is still listening"
        );
        assert_eq!(settle(key), None);
    }

    /// Editing a profile down to a mode the kernel enforces on its own is a
    /// composition that needs no proxy — and it must not take the running
    /// agent's away either, for exactly the same reason: the launch that
    /// replaces that agent may never happen.
    #[test]
    fn a_profile_that_no_longer_needs_a_proxy_keeps_the_running_one_until_the_relaunch() {
        let _guard = fabricated_data_dir("cleared");
        let key = "egress-cleared";
        let compose = |profile: SandboxProfile| {
            let mut config = config_with(Some(profile));
            config.agent_session_id = Some(key.into());
            let wrapped = build(
                &mac_host(),
                "/fabricated/home",
                None,
                &config,
                "claude",
                &[],
            )
            .expect("the boundary composes");
            (config, wrapped)
        };
        let filtered = || {
            let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
            profile.network_allow = vec!["api.anthropic.com".into()];
            profile
        };
        let unfiltered = || {
            let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
            profile.network_mode = crate::session::NetworkMode::None;
            profile
        };

        // The instance the running agent was launched with.
        let (config, wrapped) = compose(filtered());
        let port = proxy_port(&wrapped);
        pending_egress(&config).commit();
        assert_eq!(settle(key), Some(vec!["api.anthropic.com".to_string()]));

        // The profile is edited, and a relaunch composed against it — twice,
        // once thrown away and once launched.
        let (config, _) = compose(unfiltered());
        assert_eq!(
            settle(key),
            Some(vec!["api.anthropic.com".to_string()]),
            "composing retired a boundary the running agent is still reaching"
        );
        drop(pending_egress(&config));
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "a relaunch that never happened cost the running agent its egress"
        );

        let (config, _) = compose(unfiltered());
        pending_egress(&config).commit();
        assert_eq!(settle(key), None, "the relaunch needs no proxy");
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
            "the boundary the session no longer has is still listening"
        );
    }

    /// The whole of P2 in one launch: a filtered profile starts a proxy before
    /// the agent exists, the kernel policy opens exactly that port, and the
    /// agent is handed every spelling of the proxy environment.
    ///
    /// `ALL_PROXY` is asserted to be **`socks5h`**, which is the one detail
    /// that fails silently: with plain `socks5` the client resolves the
    /// hostname itself and hands the proxy an address, so every domain rule
    /// stops matching and the allowlist enforces nothing at all.
    #[test]
    fn a_filtered_profile_is_launched_with_a_proxy_and_the_environment_to_use_it() {
        let _guard = fabricated_data_dir("egress");
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some("egress-launch".into());
        let def = agent_def();

        let wrapped = build(
            &mac_host(),
            "/fabricated/home",
            Some(&def),
            &config,
            "claude",
            &[],
        )
        .unwrap();

        // The one hole the profile leaves open names the port the proxy is
        // already listening on: nothing can race a listener that is not bound.
        let port = proxy_port(&wrapped);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());

        let expected = format!("127.0.0.1:{port}");
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            let value = wrapped
                .env
                .get(name)
                .unwrap_or_else(|| panic!("{name} is unset: {:?}", wrapped.env));
            assert!(value.starts_with("http://"), "{name} = {value}");
            assert!(value.ends_with(&expected), "{name} = {value}");
        }
        for name in ["ALL_PROXY", "all_proxy"] {
            let value = wrapped.env.get(name).expect("ALL_PROXY is set");
            assert!(
                value.starts_with("socks5h://"),
                "{name} must keep resolution on the proxy's side: {value}"
            );
        }
        for name in ["NO_PROXY", "no_proxy"] {
            assert_eq!(
                wrapped.env.get(name).map(String::as_str),
                Some("localhost,127.0.0.1,::1"),
                "the agent's own local traffic must not be tunnelled"
            );
        }
        // The agent's declared environment still arrives; the boundary's
        // variables are simply the last word.
        assert_eq!(
            wrapped.env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );

        // Teardown takes the proxy with the scratch directory, so no listener
        // outlives the session that was given it. The stop is queued rather
        // than waited on — teardown must not block a UI thread — so the
        // assertion is that it happens, not that it has already happened.
        cleanup(&config);
        let closed = (0..200).any(|_| {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        });
        assert!(closed, "the proxy outlived its session");
    }

    /// Fail closed: a proxy that cannot start refuses the launch, so an
    /// `allowlist` sandbox is never started believing it is filtered. The
    /// profile's own escape hatch decides what happens next — and a fallback
    /// keeps the desired profile, which is what the next relaunch rebuilds
    /// from.
    #[test]
    fn a_proxy_that_cannot_start_refuses_the_launch() {
        // A data directory deep enough that the socket path cannot fit in
        // `sun_path`, which is a failure with no host and no network in it.
        let base =
            std::env::temp_dir().join(format!("frs{}-{}", std::process::id(), "d".repeat(120)));
        let _guard = crate::paths::TestPathGuard::new(&base);
        let mut config = config_with(Some(SandboxProfile::new(
            "dev",
            vec![SandboxPath::workspace("~/dev/app")],
        )));
        config.agent_session_id = Some("egress-refused".into());

        let err = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("egress proxy"), "{err}");
        assert!(err.contains("at most"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A place-backed session's writable directory lives inside the place's
    /// tree, not in the policy scratch root — so teardown has to reach it there
    /// or the socket, and everything the agent wrote beside it, outlives the
    /// session forever.
    #[test]
    fn deleting_a_place_backed_session_drops_its_directory_inside_the_place() {
        let _guard = fabricated_data_dir("place-teardown");
        let key = "place-teardown-session";
        let minted = crate::sandbox::create_place_session_dir("dev", key).unwrap();
        std::fs::write(minted.join("agent-scratch"), "x").unwrap();
        assert!(minted.is_dir());

        cleanup_key(key);
        assert!(
            !minted.exists(),
            "{} outlived the session",
            minted.display()
        );
        // The place itself is the profile's and outlives every session in it.
        assert!(crate::sandbox::dirs::place_dir("dev").unwrap().is_dir());
    }

    // ---- Place backends ---------------------------------------------------

    /// The container id the stub engine hands back for a freshly created place.
    const STUB_CONTAINER: &str = "1f2e3d4c5b6a798807162534435261708192a3b4c5d6e7f8091a2b3c4d5e6f70";

    /// A host with a working rootless podman and nothing else, scripted far
    /// enough to create a place and resolve the relay inside it.
    ///
    /// Nothing here starts, pulls or builds anything: every engine command goes
    /// through the injected probe host, and the paths are all fabricated or
    /// friring's own under the test's data directory.
    fn place_host() -> SandboxHost {
        use crate::sandbox::probe::ProbeOutput;
        const PODMAN: &str = "/usr/bin/podman";
        let place_dir = crate::sandbox::dirs::place_dir("dev").expect("a data directory");
        let home_dir = crate::sandbox::dirs::place_home_dir("dev").expect("a data directory");
        let stub = crate::sandbox::probe::StubHost::new()
            .with_home("/fabricated/home")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("podman")
            .with_command("id -u", ProbeOutput::success("1000\n"))
            .with_command("id -g", ProbeOutput::success("1000\n"))
            .with_command(
                &format!(
                    "{PODMAN} info --format {}",
                    "{{.Version.Version}}|{{.Host.Security.Rootless}}"
                ),
                ProbeOutput::success("5.2.2|true\n"),
            )
            .with_path("/fabricated/home/dev/app")
            .with_path(&place_dir.display().to_string())
            .with_path(&home_dir.display().to_string())
            .with_command_prefix(
                &format!("{PODMAN} image inspect"),
                ProbeOutput::success("[{}]\n"),
            )
            .with_command_prefix(
                &format!("{PODMAN} run"),
                ProbeOutput::success(format!("{STUB_CONTAINER}\n")),
            )
            .with_command_prefix(
                &format!("{PODMAN} exec"),
                ProbeOutput::success("/usr/local/bin/friring-cli\n"),
            );
        SandboxHost::new(std::sync::Arc::new(stub))
    }

    /// A data directory short enough for a **place's** unix socket path.
    ///
    /// A place nests its per-session directory one level deeper than a policy
    /// sandbox's (`sandbox/pl/<profile>/<digest>/proxy.sock`), and macOS's
    /// per-user temp root is ~49 bytes before anything is appended — together
    /// they overrun `sun_path`'s 103. The real data directory
    /// (`~/.local/share/friring`) is nowhere near it; this is the test
    /// environment's problem, and the launch says so with the fix when it is
    /// anyone's.
    #[cfg(unix)]
    fn short_data_dir(name: &str) -> crate::paths::TestPathGuard {
        crate::paths::TestPathGuard::new(
            std::path::Path::new("/tmp").join(format!("fr{}{name}", std::process::id())),
        )
    }

    fn place_profile(mutate: impl FnOnce(&mut SandboxProfile)) -> SandboxProfile {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::Podman;
        mutate(&mut profile);
        profile
    }

    /// The whole of a place launch in one composition: the place is ensured,
    /// the command composed for the *inside* of it, and the transport that
    /// reaches it handed back with the row to record.
    #[test]
    fn a_place_profile_composes_a_transport_and_an_in_place_command() {
        let _guard = fabricated_data_dir("place-compose");
        let profile = place_profile(|p| p.network_mode = crate::session::NetworkMode::None);
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some("place-compose".into());
        config.cwd = Some("/fabricated/home/dev/app".into());

        let wrapped = build(
            &place_host(),
            "/fabricated/home",
            Some(&agent_def()),
            &config,
            "claude",
            &["--resume".into()],
        )
        .expect("a place composes");

        // Nothing the in-place command says names the engine: reaching the
        // place is the transport's business, and this runs inside it.
        assert_eq!(wrapped.command, "claude");
        assert_eq!(wrapped.args.first().map(String::as_str), Some("--resume"));
        assert!(!wrapped.args.iter().any(|a| a.contains("podman")));

        // The transport, built from the vetted engine path and the id the
        // engine minted, named the way `backend_type` will record it.
        let place = wrapped.place.expect("a place-backed launch has a place");
        assert_eq!(place.engine(), "/usr/bin/podman");
        assert_eq!(place.container(), STUB_CONTAINER);
        assert_eq!(place.backend_name(), "sandbox:dev");

        // …and the row garbage collection finds the container by.
        let instance = wrapped
            .instance
            .expect("a place launch records an instance");
        assert_eq!(instance.profile, "dev");
        assert_eq!(instance.engine, SandboxBackendKind::Podman);
        assert_eq!(instance.external_id, STUB_CONTAINER);
    }

    /// A policy launch composes no place, and that is what stops the two halves
    /// of ADR-26 from being confused for one another.
    #[test]
    fn a_policy_profile_composes_no_place() {
        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config_with(Some(closed_profile())),
            "claude",
            &[],
        )
        .unwrap();
        assert!(wrapped.place.is_none());
        assert!(wrapped.instance.is_none());
    }

    /// A place *is* the elsewhere, so the refusal that keeps a policy backend
    /// off a remote session must not apply to it — otherwise a sandbox profile
    /// could never be used from a session friring reaches over a transport.
    #[test]
    fn a_place_is_not_refused_for_being_a_remote_session() {
        let _guard = fabricated_data_dir("place-remote");
        let mut config = config_with(Some(place_profile(|p| {
            p.network_mode = crate::session::NetworkMode::None;
        })));
        config.agent_session_id = Some("place-remote".into());
        config.backend = Some("ssh:devbox".into());
        build(
            &place_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
        )
        .expect("a place composes for a session friring reached elsewhere");

        // The policy half still refuses, which is the rule this exempts a place
        // from rather than deletes.
        let mut policy = config_with(Some(closed_profile()));
        policy.backend = Some("ssh:devbox".into());
        let err = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &policy,
            "claude",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("remote session"), "{err}");
    }

    /// Config projection is a later slice, so a place has none of the host's
    /// agent configuration in it. An argument naming a friring-managed file
    /// would point at nothing inside and kill the pane on startup, so it is
    /// dropped — and the composition says so, because a session that silently
    /// stops reporting status looks like a broken session.
    #[test]
    fn a_place_drops_host_config_arguments_and_says_it_did() {
        let _guard = fabricated_data_dir("place-config");
        let config_arg = crate::paths::config_file()
            .and_then(|p| p.parent().map(|d| d.join("hooks").join("claude.json")))
            .expect("a config directory")
            .display()
            .to_string();
        let mut config = config_with(Some(place_profile(|p| {
            p.network_mode = crate::session::NetworkMode::None;
        })));
        config.agent_session_id = Some("place-config".into());

        let wrapped = build(
            &place_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &["--settings".into(), config_arg.clone(), "--verbose".into()],
        )
        .unwrap();

        assert!(
            !wrapped
                .args
                .iter()
                .any(|a| *a == config_arg || a == "--settings"),
            "the flag and its path must go together: {:?}",
            wrapped.args
        );
        assert!(wrapped.args.contains(&"--verbose".to_string()));
        assert!(
            wrapped.state.contains("sign in inside the pane"),
            "{}",
            wrapped.state
        );
        assert!(
            wrapped.state.contains("reports no status"),
            "{}",
            wrapped.state
        );

        // A policy sandbox keeps them: the host's filesystem is what it is
        // subject to a policy, so the file is right where the argument says.
        let policy = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config_with(Some(closed_profile())),
            "claude",
            &["--settings".into(), config_arg.clone()],
        )
        .unwrap();
        assert!(policy.args.contains(&config_arg));
        assert!(!policy.state.contains("sign in inside the pane"));
    }

    /// Sessions of one profile share a place and therefore its loopback, so the
    /// address each one's proxy environment names has to be that session's own
    /// relay port — the second session would otherwise be handed the first's
    /// and fail closed while the profile still claimed a filtered network.
    #[test]
    fn every_session_in_a_place_gets_its_own_proxy_address() {
        let _guard = short_data_dir("pe");
        let host = place_host();
        let compose = |key: &str| {
            let mut config = config_with(Some(place_profile(|p| {
                p.network_allow = vec!["api.anthropic.com".into()];
            })));
            config.agent_session_id = Some(key.to_string());
            let wrapped = build(&host, "/fabricated/home", None, &config, "claude", &[])
                .expect("a filtered place composes");
            pending_egress(&config).commit();
            (config, wrapped)
        };

        let (first_config, first) = compose("place-egress-a");
        let (second_config, second) = compose("place-egress-b");
        let address = |wrapped: &SandboxedInvocation| {
            wrapped
                .env
                .get("HTTP_PROXY")
                .expect("a filtered launch is given the proxy environment")
                .rsplit_once(':')
                .map(|(_, port)| port.to_string())
                .expect("a proxy URL names a port")
        };
        assert_ne!(
            address(&first),
            address(&second),
            "two sessions in one place were handed one loopback port"
        );
        // …and each keeps its own across a relaunch, because the environment of
        // the launch that is still running names it.
        let (_, again) = compose("place-egress-a");
        assert_eq!(address(&first), address(&again));

        // The relay runs beside the agent, inside the place.
        assert_eq!(first.command, "/bin/sh");
        assert!(first.args.iter().any(|a| a == "/usr/local/bin/friring-cli"));

        cleanup(&first_config);
        cleanup(&second_config);
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

    /// Session teardown holds a stored row, not the config the launch was
    /// composed from, so the id-keyed cleanup has to reach exactly what a
    /// pinned launch minted. If the two key derivations ever drift, a deleted
    /// session leaves its agent's writable scratch on disk forever.
    #[test]
    fn a_delete_that_knows_only_the_session_id_still_reaches_the_scratch() {
        let id = crate::session::SessionId::default();
        let mut config = config_with(None);
        config.session_id = Some(id);

        let minted = crate::sandbox::create_session_scratch(&session_key(&config)).unwrap();
        std::fs::write(minted.join("agent-scratch"), "x").unwrap();

        cleanup_by_session_id(id);
        assert!(
            !minted.exists(),
            "{} outlived the session",
            minted.display()
        );
    }
}
