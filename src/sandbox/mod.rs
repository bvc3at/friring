//! Sandboxed agents — the isolation boundary friring puts around an agent.
//!
//! `docs/SANDBOX.md` is the design contract; ADR-25 there is why this is a core
//! module rather than an extension, and ADR-26 is why every backend is one of
//! two shapes. The short version:
//!
//! - A **policy backend** ([`seatbelt`], [`bwrap`]) applies a kernel policy to
//!   a process tree by wrapping the agent's argv. tmux stays outside, so
//!   discovery, reattach and scrollback are untouched.
//! - A **place backend** ([`container`], and the VMs and distro clones still to
//!   come) is an environment reached through a transport, with tmux inside. It
//!   is created once per profile and shared by every session that picks it, so
//!   it implements [`SandboxBackend::ensure`] — and, because its egress relay
//!   runs *in* the place, [`SandboxBackend::wrap`] for the command that runs
//!   there.
//!
//! What lives where: [`crate::session::sandbox_profile`] holds the pure profile
//! and policy data (what the user edits, what storage persists); this module
//! holds everything with a side effect — probing a host, generating an SBPL
//! profile or a bubblewrap command line, and choosing a backend.
//!
//! Architecture: `sandbox` sits in the same tier as `agent`. It may reference
//! `session`, `paths` and `shell`, and never `ui`, `git` or `app`.
//!
//! ```no_run
//! use friring::sandbox::{create_session_scratch, egress, SandboxHost, SandboxLaunch};
//! use friring::session::{SandboxPath, SandboxProfile};
//!
//! let host = SandboxHost::local();
//! let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
//! // Run the ladder first: a policy names the backend that will actually run.
//! let backend = host.select(profile.backend).backend()?;
//! let policy = profile.resolve(backend, "/home/u")?;
//! // A filtered network mode is enforced by a proxy *outside* the boundary
//! // (ADR-27), so one is bound — on the transport this backend can reach —
//! // before the launch that opens a hole to it is composed.
//! let caps = host.backend(backend).expect("a built-in backend").capabilities();
//! let scratch = create_session_scratch("session-id")?;
//! let prepared = egress::prepare("session-id", &policy, caps.proxy_transport, &scratch)?;
//! let launch = SandboxLaunch::new(&policy, "/home/u", "session-id")
//!     .with_proxy(prepared.grant.endpoint);
//! let _argv = host.wrap(backend, vec!["claude".to_string()], &launch)?;
//! // The instance belongs to no session until something is running behind it:
//! // dropping the handle instead releases it and leaves the session's own
//! // boundary alone.
//! prepared.pending.commit();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod agent;
pub mod auth;
pub mod backend;
pub mod bwrap;
pub mod container;
pub mod dirs;
pub mod egress;
pub mod launcher;
pub mod probe;
pub mod projection;
pub mod seatbelt;
pub mod secrets;
pub mod select;

use std::sync::{Arc, OnceLock};

pub use agent::{apply_agent_requirements, compose_inner_sandbox, InnerSandboxPlan};
pub use auth::{
    Boundary, CredentialInput, CredentialPlan, CredentialStrategy, LoginState, StateDir,
};
pub use backend::{
    Argv, Availability, Caps, Egress, InnerSandboxVerdict, PlaceLaunch, PlaceRelay, ProxyEndpoint,
    ProxyTransport, SandboxBackend, SandboxError, SandboxLaunch, SandboxResult,
};
pub use bwrap::{BwrapBackend, BwrapDetails};
pub use container::{ContainerBackend, ContainerEngine, EnsuredPlace};
pub use dirs::{
    check_writable_roots, cleanup_place, cleanup_session, create_place_dirs,
    create_place_session_dir, create_session_scratch,
};
pub use egress::{proxy_required, PendingEgress, Prepared, ProxyGrant, SessionDenial};
pub use probe::{detect_platform, HostPlatform, LocalProbeHost, ProbeHost, RemoteProbeHost};
pub use projection::{plan as plan_projection, Finding, ProjectionInput, ProjectionPlan, Verdict};
pub use seatbelt::SeatbeltBackend;
pub use secrets::{secrets_for, SecretKind, SecretPath, SecretPlatform, SECRET_PATHS};
pub use select::{ladder, select_backend, RejectedRung, Selection};

use crate::session::{AgentSandboxDef, HostDef, SandboxBackendKind};

/// Re-exported where the backends use it: the pure record lives in
/// [`crate::session`], because `storage` and `agent` name it too and neither
/// may reference this module.
pub use crate::session::SandboxInstance;

/// Every backend friring can offer on one host, with their probes cached.
///
/// Built once per host and kept: a probe costs a process spawn (and a network
/// round trip for a remote host), and the picker asks about every rung each
/// time it paints.
pub struct SandboxHost {
    platform: OnceLock<HostPlatform>,
    probe_host: Arc<dyn ProbeHost>,
    seatbelt: SeatbeltBackend,
    bwrap: BwrapBackend,
    docker: ContainerBackend,
    podman: ContainerBackend,
}

impl SandboxHost {
    /// The backends available on `probe_host`.
    pub fn new(probe_host: Arc<dyn ProbeHost>) -> Self {
        Self {
            platform: OnceLock::new(),
            seatbelt: SeatbeltBackend::new(
                Arc::clone(&probe_host),
                seatbelt::default_profile_dir(),
            ),
            bwrap: BwrapBackend::new(Arc::clone(&probe_host)),
            docker: ContainerBackend::new(ContainerEngine::Docker, Arc::clone(&probe_host)),
            podman: ContainerBackend::new(ContainerEngine::Podman, Arc::clone(&probe_host)),
            probe_host,
        }
    }

    /// The machine friring itself runs on.
    pub fn local() -> Self {
        Self::new(Arc::new(LocalProbeHost))
    }

    /// The process-wide [`local`](Self::local) host.
    ///
    /// Probing costs a process spawn, and both callers ask repeatedly: the
    /// profile editor resolves `auto` on every keystroke that could change it,
    /// and every spawn resolves it again. One host means one probe per backend
    /// for the life of the process — which is also the caching
    /// [`SandboxBackend::probe`] promises, just hoisted to where the object
    /// itself would otherwise be rebuilt.
    pub fn local_shared() -> &'static Self {
        static LOCAL: OnceLock<SandboxHost> = OnceLock::new();
        LOCAL.get_or_init(Self::local)
    }

    /// A configured SSH host or WSL distro — a sandbox is probed where it will
    /// run, not where the TUI runs.
    pub fn remote(host: HostDef) -> Self {
        Self::new(Arc::new(RemoteProbeHost::new(host)))
    }

    /// What kind of machine this is. Detected once.
    pub fn platform(&self) -> HostPlatform {
        *self
            .platform
            .get_or_init(|| detect_platform(self.probe_host.as_ref()))
    }

    /// The backend object for `kind`, or `None` when this build does not have
    /// one — which is `apple-container` and `wsl-distro`, the two place backends
    /// still to come.
    pub fn backend(&self, kind: SandboxBackendKind) -> Option<&dyn SandboxBackend> {
        match kind {
            SandboxBackendKind::Seatbelt => Some(&self.seatbelt),
            SandboxBackendKind::Bwrap => Some(&self.bwrap),
            SandboxBackendKind::Docker => Some(&self.docker),
            SandboxBackendKind::Podman => Some(&self.podman),
            _ => None,
        }
    }

    /// The container backend behind `kind`, for the operations only a place has:
    /// ensuring an instance, reaching it through the transport, reclaiming it.
    /// `None` for anything that is not a container engine.
    pub fn container(&self, kind: SandboxBackendKind) -> Option<&ContainerBackend> {
        match kind {
            SandboxBackendKind::Docker => Some(&self.docker),
            SandboxBackendKind::Podman => Some(&self.podman),
            _ => None,
        }
    }

    /// Whether `kind` can be used here, and if not, why. Never a silent skip:
    /// a backend this build does not implement says so in the same shape as one
    /// the host is missing.
    pub fn probe(&self, kind: SandboxBackendKind) -> Availability {
        match self.backend(kind) {
            Some(backend) => backend.probe(),
            None if kind == SandboxBackendKind::Auto => {
                Availability::unavailable("'auto' is resolved by the ladder, never probed")
            }
            None => Availability::unavailable(format!(
                "the {kind} backend is not built yet; this friring has seatbelt, bwrap, docker \
                 and podman"
            )),
        }
    }

    /// Resolve `requested` against this host's ladder.
    pub fn select(&self, requested: SandboxBackendKind) -> Selection {
        select_backend(requested, self.platform(), |kind| self.probe(kind))
    }

    /// Wrap `argv` for `kind`. Fails with the backend's own reason when the
    /// backend is a place (whose half is [`SandboxBackend::ensure`]) or is not
    /// in this build.
    pub fn wrap(
        &self,
        kind: SandboxBackendKind,
        argv: Argv,
        launch: &SandboxLaunch<'_>,
    ) -> SandboxResult<Argv> {
        match self.backend(kind) {
            Some(backend) => backend.wrap(argv, launch),
            None => Err(SandboxError::NotInThisStage {
                backend: kind,
                detail: "no backend of that kind is built into this friring".to_string(),
            }),
        }
    }

    /// The composition an agent's own sandbox gets under `kind` — off, and why.
    /// `None` when the backend is not one this build knows.
    pub fn inner_sandbox(
        &self,
        kind: SandboxBackendKind,
        policy: &crate::session::SandboxPolicy,
        agent: Option<&AgentSandboxDef>,
    ) -> Option<InnerSandboxPlan> {
        let verdict = self.backend(kind)?.capabilities().inner_agent_sandbox;
        Some(compose_inner_sandbox(policy, verdict, agent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::probe::StubHost;
    use crate::session::{SandboxPath, SandboxProfile};

    #[test]
    fn a_linux_host_offers_bwrap_and_explains_the_rest() {
        let host = SandboxHost::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        assert_eq!(host.platform(), HostPlatform::Linux);
        assert!(host.probe(SandboxBackendKind::Bwrap).is_available());

        // seatbelt is the wrong OS; docker is the right OS with nothing
        // installed; apple-container is a backend this build does not have.
        assert!(host
            .probe(SandboxBackendKind::Seatbelt)
            .message()
            .contains("macOS-only"));
        assert!(host
            .probe(SandboxBackendKind::Docker)
            .message()
            .contains("docker is not installed"));
        assert!(host
            .probe(SandboxBackendKind::AppleContainer)
            .message()
            .contains("not built yet"));

        let selection = host.select(SandboxBackendKind::Auto);
        assert_eq!(selection.chosen, Some(SandboxBackendKind::Bwrap));
    }

    #[test]
    fn a_macos_host_resolves_to_seatbelt_and_wraps_with_it() {
        let host = SandboxHost::new(Arc::new(StubHost::macos(26, true)));
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        let chosen = host.select(profile.backend).backend().unwrap();
        assert_eq!(chosen, SandboxBackendKind::Seatbelt);

        let policy = profile.resolve(chosen, "/Users/u").unwrap();
        // The default profile is `allowlist`, which only means anything with
        // the proxy that enforces it.
        let launch = SandboxLaunch::new(&policy, "/Users/u", "host-test")
            .with_proxy(ProxyEndpoint::Loopback { port: 8123 });
        let argv = host
            .wrap(chosen, vec!["claude".to_string()], &launch)
            .unwrap();
        assert_eq!(
            argv.first().map(String::as_str),
            Some(seatbelt::SANDBOX_EXEC)
        );
        assert_eq!(argv.last().map(String::as_str), Some("claude"));
        let _ = std::fs::remove_file(host.seatbelt.profile_path(&launch).unwrap());
    }

    /// A place backend composes the command that runs *inside* the place, so
    /// handing it one policy resolved for another backend — or a launch with no
    /// place at all — is refused rather than wrapped.
    #[test]
    fn a_place_backend_will_not_wrap_another_backends_policy() {
        let host = SandboxHost::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let err = host
            .wrap(SandboxBackendKind::Podman, vec!["claude".into()], &launch)
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unsupported { .. }), "{err}");

        // And a backend this build has no object for still says so in the same
        // shape as one the host is missing.
        let err = host
            .wrap(
                SandboxBackendKind::AppleContainer,
                vec!["claude".into()],
                &launch,
            )
            .unwrap_err();
        assert!(matches!(err, SandboxError::NotInThisStage { .. }), "{err}");
    }

    /// One profile must mean one thing on both backends.
    ///
    /// The divergence this exists to catch: bwrap sorted every mount so the
    /// most specific one landed last and won, while seatbelt anchored only the
    /// read-only roots and let the *ancestor's* deny win. "repo read-only,
    /// repo/work read-write" therefore meant opposite things on macOS and
    /// Linux. The verdicts below are computed the way each kernel computes them
    /// — last matching SBPL rule, last matching bwrap mount — so the assertion
    /// is about behaviour rather than about text.
    mod conformance {
        use std::sync::Arc;

        use crate::sandbox::backend::{SandboxBackend, SandboxError, SandboxLaunch};
        use crate::sandbox::probe::StubHost;
        use crate::sandbox::{bwrap, dirs, seatbelt, BwrapBackend, SeatbeltBackend};
        use crate::session::{NetworkMode, SandboxBackendKind, SandboxPath, SandboxProfile};

        /// The path a `(subpath "…")` filter names, if the line has one.
        fn subpath_of(line: &str) -> Option<&str> {
            let rest = line.split_once("(subpath \"")?.1;
            rest.split_once('"').map(|(path, _)| path)
        }

        /// Whether the generated profile leaves `probe` writable: the last rule
        /// whose subpath covers it decides, which is how SBPL evaluates.
        fn seatbelt_grants_write(text: &str, probe: &str) -> bool {
            let mut granted = false;
            for line in text.lines() {
                if !subpath_of(line).is_some_and(|path| dirs::encloses(path, probe)) {
                    continue;
                }
                if line.starts_with("(allow file-read* file-write*") {
                    granted = true;
                } else if line.starts_with("(deny file-write*")
                    || line.starts_with("(deny file-read* file-write*")
                {
                    granted = false;
                }
            }
            granted
        }

        /// The same question of a bwrap command line: the last mount whose
        /// destination covers `probe` decides, which is how the mount namespace
        /// ends up.
        fn bwrap_grants_write(argv: &[String], probe: &str) -> bool {
            let mut granted = false;
            let mut i = 0;
            while i < argv.len() {
                let (dest, writable, width) = match argv[i].as_str() {
                    flag @ ("--bind" | "--bind-try" | "--ro-bind" | "--ro-bind-try")
                        if i + 2 < argv.len() =>
                    {
                        (&argv[i + 2], flag.starts_with("--bind"), 3)
                    }
                    "--tmpfs" | "--dev" | "--proc" if i + 1 < argv.len() => (&argv[i + 1], true, 2),
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                if dirs::encloses(dest, probe) {
                    granted = writable;
                }
                i += width;
            }
            granted
        }

        /// One profile's paths, and what each probe path underneath it must
        /// resolve to on *both* backends.
        type Case = (Vec<SandboxPath>, Vec<(&'static str, bool)>);

        #[test]
        fn both_backends_agree_on_overlapping_paths() {
            let cases: Vec<Case> = vec![
                (
                    vec![
                        SandboxPath::read_only("/repo"),
                        SandboxPath::workspace("/repo/work"),
                    ],
                    vec![("/repo/f", false), ("/repo/work/f", true)],
                ),
                (
                    vec![
                        SandboxPath::workspace("/repo"),
                        SandboxPath::read_only("/repo/vendor"),
                    ],
                    vec![("/repo/f", true), ("/repo/vendor/f", false)],
                ),
                (
                    vec![
                        SandboxPath::workspace("/repo"),
                        SandboxPath::read_only("/repo/vendor"),
                        SandboxPath::workspace("/repo/vendor/cache"),
                    ],
                    vec![
                        ("/repo/f", true),
                        ("/repo/vendor/f", false),
                        ("/repo/vendor/cache/f", true),
                    ],
                ),
                (
                    vec![
                        SandboxPath::workspace("/repo"),
                        SandboxPath::read_only("/srv/shared"),
                    ],
                    vec![
                        ("/repo/f", true),
                        ("/srv/shared/f", false),
                        ("/elsewhere/f", false),
                    ],
                ),
            ];

            for (paths, expected) in cases {
                let profile = SandboxProfile::new("dev", paths.clone());
                let mac = profile
                    .resolve(SandboxBackendKind::Seatbelt, "/home/u")
                    .unwrap();
                let linux = profile
                    .resolve(SandboxBackendKind::Bwrap, "/home/u")
                    .unwrap();
                let text = seatbelt::render_profile(&SandboxLaunch::new(&mac, "/home/u", "s1"));
                let argv = bwrap::build_argv(
                    "/usr/bin/bwrap",
                    &SandboxLaunch::new(&linux, "/home/u", "s1"),
                    None,
                    &|_| false,
                )
                .unwrap();

                for (probe, writable) in expected {
                    let listed: Vec<String> = paths
                        .iter()
                        .map(|p| format!("{} {}", p.path, p.mode))
                        .collect();
                    assert_eq!(
                        seatbelt_grants_write(&text, probe),
                        writable,
                        "seatbelt: {probe} under [{}]",
                        listed.join(", ")
                    );
                    assert_eq!(
                        bwrap_grants_write(&argv, probe),
                        writable,
                        "bwrap: {probe} under [{}]",
                        listed.join(", ")
                    );
                }
            }
        }

        /// A launch friring refuses is refused by both backends, with one
        /// sentence — the checks live on the launch, not in either policy
        /// generator, precisely so they cannot drift apart again.
        #[test]
        fn both_backends_refuse_the_same_launches() {
            let mac = SeatbeltBackend::new(
                Arc::new(StubHost::macos(26, true)),
                seatbelt::default_profile_dir(),
            );
            let linux = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));

            let mut reaching_the_database =
                SandboxProfile::new("dev", vec![SandboxPath::workspace("~")]);
            // Denies that only the egress proxy can enforce, with no proxy
            // started for the launch: refused on both backends, in one sentence.
            let mut unenforceable_denies =
                SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
            unenforceable_denies.network_mode = NetworkMode::Full;
            unenforceable_denies.network_deny = vec!["evil.example".into()];

            for profile in [&mut reaching_the_database, &mut unenforceable_denies] {
                let mac_policy = profile
                    .resolve(SandboxBackendKind::Seatbelt, "/home/u")
                    .unwrap();
                let linux_policy = profile
                    .resolve(SandboxBackendKind::Bwrap, "/home/u")
                    .unwrap();
                let db = "/home/u/.local/share/friring/friring.db";
                let mac_err = mac
                    .wrap(
                        vec!["claude".into()],
                        &SandboxLaunch::new(&mac_policy, "/home/u", "s1").with_friring_db(db),
                    )
                    .unwrap_err();
                let linux_err = linux
                    .wrap(
                        vec!["claude".into()],
                        &SandboxLaunch::new(&linux_policy, "/home/u", "s1").with_friring_db(db),
                    )
                    .unwrap_err();
                assert!(matches!(mac_err, SandboxError::Refused { .. }), "{mac_err}");
                assert_eq!(mac_err.to_string(), linux_err.to_string());
            }
        }
    }

    #[test]
    fn the_inner_sandbox_verdict_follows_the_chosen_backend() {
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        let agent = AgentSandboxDef {
            bypass: vec!["--no-sandbox".into()],
            ..Default::default()
        };

        let mac = SandboxHost::new(Arc::new(StubHost::macos(26, true)));
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let plan = mac
            .inner_sandbox(SandboxBackendKind::Seatbelt, &policy, Some(&agent))
            .unwrap();
        assert_eq!(plan.verdict, InnerSandboxVerdict::Denied);

        let linux = SandboxHost::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let plan = linux
            .inner_sandbox(SandboxBackendKind::Bwrap, &policy, Some(&agent))
            .unwrap();
        assert_eq!(plan.verdict, InnerSandboxVerdict::Redundant);
        // A backend this build has no object for has no verdict to give.
        assert!(linux
            .inner_sandbox(SandboxBackendKind::WslDistro, &policy, Some(&agent))
            .is_none());
    }
}
