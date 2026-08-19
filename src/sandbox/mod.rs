//! Sandboxed agents — the isolation boundary friring puts around an agent.
//!
//! `docs/SANDBOX.md` is the design contract; ADR-25 there is why this is a core
//! module rather than an extension, and ADR-26 is why every backend is one of
//! two shapes. The short version:
//!
//! - A **policy backend** ([`seatbelt`], [`bwrap`]) applies a kernel policy to
//!   a process tree by wrapping the agent's argv. tmux stays outside, so
//!   discovery, reattach and scrollback are untouched.
//! - A **place backend** ([`container`], [`apple`], [`wsl`]) is an environment
//!   reached through a transport, with tmux inside. It is created once per
//!   profile and shared by every session that picks it, so it implements
//!   [`SandboxBackend::ensure`] — and, because its egress relay runs *in* the
//!   place, [`SandboxBackend::wrap`] for the command that runs there.
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
pub mod apple;
pub mod auth;
pub mod backend;
pub mod bwrap;
pub mod container;
pub mod dirs;
pub mod egress;
pub mod launcher;
pub mod place;
pub mod probe;
pub mod projection;
pub mod seatbelt;
pub mod secrets;
pub mod select;
pub mod wsl;

use std::sync::{Arc, OnceLock};

pub use agent::{apply_agent_requirements, compose_inner_sandbox, InnerSandboxPlan};
pub use apple::{AppleContainerBackend, AppleDetails};
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
    check_declared_paths, check_engine_socket_paths, check_writable_roots, cleanup_place,
    cleanup_session, create_place_dirs, create_place_session_dir, create_session_scratch,
};
pub use egress::{proxy_required, PendingEgress, Prepared, ProxyGrant, SessionDenial};
pub use place::{live_places_here, PlaceBackend, PLACE_KINDS};
pub use probe::{detect_platform, HostPlatform, LocalProbeHost, ProbeHost, RemoteProbeHost};
pub use projection::{plan as plan_projection, Finding, ProjectionInput, ProjectionPlan, Verdict};
pub use seatbelt::SeatbeltBackend;
pub use secrets::{secrets_for, SecretKind, SecretPath, SecretPlatform, SECRET_PATHS};
pub use select::{ladder, select_backend, RejectedRung, Selection};
pub use wsl::{EnsuredDistro, WslDetails, WslDistroBackend};

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
    apple: AppleContainerBackend,
    wsl: WslDistroBackend,
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
            apple: AppleContainerBackend::new(Arc::clone(&probe_host)),
            wsl: WslDistroBackend::new(Arc::clone(&probe_host)),
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

    /// The backend object for `kind`, or `None` for
    /// [`Auto`](SandboxBackendKind::Auto), which the ladder resolves rather than
    /// probes — and for a kind a later friring adds before it has an
    /// implementation.
    pub fn backend(&self, kind: SandboxBackendKind) -> Option<&dyn SandboxBackend> {
        match kind {
            SandboxBackendKind::Seatbelt => Some(&self.seatbelt),
            SandboxBackendKind::Bwrap => Some(&self.bwrap),
            SandboxBackendKind::Docker => Some(&self.docker),
            SandboxBackendKind::Podman => Some(&self.podman),
            SandboxBackendKind::AppleContainer => Some(&self.apple),
            SandboxBackendKind::WslDistro => Some(&self.wsl),
            _ => None,
        }
    }

    /// The backend behind `kind` as a **place**, for the operations only a place
    /// has: ensuring an instance, reaching it through the transport, reclaiming
    /// it.
    ///
    /// The one accessor every caller of a place uses, so a backend that skipped
    /// a step would have to skip it in the implementation rather than by not
    /// being wired to a caller. `None` for a policy backend, for
    /// [`Auto`](SandboxBackendKind::Auto), and for
    /// [`wsl-distro`](crate::sandbox::wsl), whose place is a registered distro
    /// reached by the `wsl:` transport rather than a container — see
    /// [`wsl_distro`](Self::wsl_distro).
    pub fn place(&self, kind: SandboxBackendKind) -> Option<&dyn PlaceBackend> {
        match kind {
            SandboxBackendKind::Docker => Some(&self.docker),
            SandboxBackendKind::Podman => Some(&self.podman),
            SandboxBackendKind::AppleContainer => Some(&self.apple),
            _ => None,
        }
    }

    /// The WSL distro backend, for the place operations the trait has no room
    /// for: registering a distro, listing the ones friring owns, destroying one.
    ///
    /// The one accessor that hands back a concrete backend, because this is the
    /// one whose operations [`PlaceBackend`] has no shape for — a distro is
    /// registered by `wsl.exe` and reached by the `wsl:` transport friring
    /// already has, so it shares no command line with either container backend.
    /// Everything the engines and Apple's tool do goes through
    /// [`place`](Self::place). `None` for anything that is not this backend.
    pub fn wsl_distro(&self, kind: SandboxBackendKind) -> Option<&WslDistroBackend> {
        match kind {
            SandboxBackendKind::WslDistro => Some(&self.wsl),
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
                "the {kind} backend is not built into this friring"
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

        // seatbelt and apple-container are the wrong OS; docker is the right OS
        // with nothing installed; wsl-distro is the wrong OS in its own words,
        // because a distro is registered from the Windows side.
        assert!(host
            .probe(SandboxBackendKind::Seatbelt)
            .message()
            .contains("macOS-only"));
        assert!(host
            .probe(SandboxBackendKind::AppleContainer)
            .message()
            .contains("macOS-only"));
        assert!(host
            .probe(SandboxBackendKind::Docker)
            .message()
            .contains("docker is not installed"));
        assert!(host
            .probe(SandboxBackendKind::WslDistro)
            .message()
            .contains("needs Windows"));

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

        // The same refusal from the other place backend, whose policy was
        // resolved for neither.
        let err = host
            .wrap(
                SandboxBackendKind::AppleContainer,
                vec!["claude".into()],
                &launch,
            )
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unsupported { .. }), "{err}");

        // And the third, whose command runs inside a distro rather than in a
        // container, refuses the same way.
        let err = host
            .wrap(
                SandboxBackendKind::WslDistro,
                vec!["claude".into()],
                &launch,
            )
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unsupported { .. }), "{err}");

        // A kind this build has no object for at all still says so in the same
        // shape as one the host is missing.
        assert!(matches!(
            host.wrap(SandboxBackendKind::Auto, vec!["claude".into()], &launch),
            Err(SandboxError::NotInThisStage { .. })
        ));
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

    /// The same promise for the **place** backends, which is a list of escapes
    /// rather than a list of paths.
    ///
    /// Every one of these was found by a review of the container backend and
    /// closed there; the risk a second and a third backend introduce is not a
    /// new bug but an old one re-opened by an implementation that renders its
    /// own command line and forgot a step. So each is asserted through the seam
    /// the launch path actually drives — [`PlaceBackend::ensure_place`] for the
    /// three engines, and `ensure_distro` then `wrap` for a
    /// [`wsl-distro`](crate::sandbox::wsl) place, which has no `PlaceBackend` to
    /// go through (see [`crate::sandbox::place`]) — and a fifth backend fails
    /// this test until it does the same.
    ///
    /// **`wsl-distro` is in the table before it is in the launch path.** No
    /// session launches into a distro yet, so every refusal here is latent —
    /// which is exactly why it is pinned now: the moment that path is wired is
    /// the moment nobody is looking at these.
    ///
    /// Nothing here starts, pulls or builds anything, and nothing registers,
    /// imports or unregisters a distro: every refusal happens while the plan is
    /// built or the argv is composed, before a single engine or `wsl.exe`
    /// command that changes anything is run, and the stub host would fail an
    /// unscripted one anyway. That is also what makes these assertions bite
    /// rather than pass vacuously — a backend that skipped one of the checks
    /// would run on to the command the stub has no answer for, and fail on the
    /// *sentence* rather than on the refusal.
    mod place_conformance {
        use std::sync::Arc;

        use crate::sandbox::backend::{PlaceLaunch, SandboxError, SandboxLaunch, SandboxResult};
        use crate::sandbox::container::plan::{create_argv, plan_instance, MountCheck, PlanInput};
        use crate::sandbox::probe::{ProbeOutput, StubHost};
        use crate::sandbox::{
            apple, dirs, wsl, AppleContainerBackend, ContainerBackend, ContainerEngine,
            PlaceBackend, SandboxBackend, WslDistroBackend,
        };
        use crate::session::{
            NetworkMode, SandboxBackendKind, SandboxPath, SandboxPolicy, SandboxProfile,
        };

        /// Where the `container` CLI sits on every stub below, so a test about a
        /// read-write root containing it can name one path for all of them.
        const APPLE_PROGRAM: &str = "/usr/bin/container";

        /// A backend under test, on a stub host where it probes available.
        struct Subject {
            kind: SandboxBackendKind,
            backend: Box<dyn PlaceBackend>,
        }

        /// Everything one backend does with a profile before an agent could run
        /// in its place, collapsed to its verdict.
        type Ensure = Box<dyn Fn(&SandboxProfile) -> SandboxResult<()>>;

        /// One backend's whole "make this profile's place and compose a launch
        /// into it" path, as a single fallible call.
        ///
        /// A closure rather than a `&dyn PlaceBackend`, because the shapes
        /// differ where it does not matter to a boundary: three engines answer
        /// [`PlaceBackend::ensure_place`], and a WSL place is ensured as a
        /// distro and then wrapped. What the caller needs from all four is the
        /// same — the refusal, or the fact that there was not one.
        struct PlacePath {
            kind: SandboxBackendKind,
            ensure: Ensure,
        }

        /// A profile with one workspace path, in the mode every backend here can
        /// honour: `allowlist` is the default and Apple's tool cannot enforce
        /// it, and a refusal about egress would mask the one each case is about.
        fn profile(name: &str, paths: Vec<SandboxPath>) -> SandboxProfile {
            let mut profile = SandboxProfile::new(name, paths);
            profile.network_mode = NetworkMode::Full;
            profile
        }

        /// Every place backend, each on a stub host that probes available, calls
        /// `home` home, and can see `paths` plus this profile's own place tree.
        ///
        /// One home for all of them so a single profile means the same thing on
        /// each: the backends read it from the host they were probed on, and a
        /// path that resolved differently per subject would make the assertions
        /// about different mounts.
        fn subjects(profile: &str, home: &str, paths: &[String]) -> Vec<Subject> {
            let (place_dir, home_dir) = dirs::create_place_dirs(profile).unwrap();
            let seen = |host: StubHost| {
                let mut host = host
                    .with_home(home)
                    .with_path(&place_dir.display().to_string())
                    .with_path(&home_dir.display().to_string());
                for path in paths {
                    host = host.with_path(path);
                }
                host
            };
            let engine_host = |engine: ContainerEngine| {
                seen(
                    StubHost::new()
                        .with_command("uname -s", ProbeOutput::success("Linux\n"))
                        .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
                        .with_binary(engine.program())
                        .with_command("id -u", ProbeOutput::success("1000\n"))
                        .with_command("id -g", ProbeOutput::success("1000\n"))
                        // By prefix, so the test does not restate each engine's own
                        // `info` template — which is not what it is about.
                        .with_command_prefix(
                            &format!("/usr/bin/{engine} info"),
                            ProbeOutput::success("27.1.1|true\n"),
                        ),
                )
            };
            let apple_host = seen(
                StubHost::macos(26, true)
                    .with_binary("container")
                    .with_command(
                        &format!("{APPLE_PROGRAM} --version"),
                        ProbeOutput::success("container CLI version 0.5.0\n"),
                    )
                    .with_command(
                        &format!("{APPLE_PROGRAM} system status"),
                        ProbeOutput::success("apiserver is running\n"),
                    )
                    .with_command(
                        &format!("{APPLE_PROGRAM} run --help"),
                        ProbeOutput::success(
                            "-d, --detach --name <name> -l, --label <label> --mount <mount> \
                             --network <network> -m, --memory <memory> -c, --cpus <cpus>",
                        ),
                    )
                    .with_command(
                        &format!("{APPLE_PROGRAM} exec --help"),
                        ProbeOutput::success("-i, --interactive\n"),
                    ),
            );
            vec![
                Subject {
                    kind: SandboxBackendKind::Docker,
                    backend: Box::new(ContainerBackend::new(
                        ContainerEngine::Docker,
                        Arc::new(engine_host(ContainerEngine::Docker)),
                    )),
                },
                Subject {
                    kind: SandboxBackendKind::Podman,
                    backend: Box::new(ContainerBackend::new(
                        ContainerEngine::Podman,
                        Arc::new(engine_host(ContainerEngine::Podman)),
                    )),
                },
                Subject {
                    kind: SandboxBackendKind::AppleContainer,
                    backend: Box::new(AppleContainerBackend::new(Arc::new(apple_host))),
                },
            ]
        }

        /// A fabricated home under the test temporary directory, resolved — the
        /// platform temp root is itself a symlink on macOS, and a source that is
        /// not its own canonical path is refused by a rule these cases are not
        /// about.
        fn fake_home(name: &str) -> String {
            let home = dirs::test_temp_base(name).join("home");
            std::fs::create_dir_all(&home).unwrap();
            home.display().to_string()
        }

        /// Where `wsl.exe` sits on the stub Windows host, spelled with forward
        /// slashes (Windows takes either) so the stub does not read it as a bare
        /// program name.
        const WSL_PROGRAM: &str = "C:/Windows/System32/wsl.exe";

        /// The bubblewrap inside a stub distro, at the same system prefix the
        /// stubs put every engine CLI at — so one read-write root (`/usr/bin`)
        /// names the program that applies the boundary on all four backends.
        const DISTRO_BWRAP: &str = "/usr/bin/bwrap";

        /// A Windows host carrying friring's distro for `profile`: registered,
        /// hardened, answering, and with bubblewrap at `bwrap` inside it.
        ///
        /// Scripted as a distro that already **exists**, so the whole path this
        /// exercises is `adopt` — which runs nothing that changes anything.
        /// Registering one is not scripted at all, so a refusal that stopped
        /// being one could not quietly import a distro here; it would fail on
        /// the unscripted `--export`.
        fn wsl_host(profile: &str, home: &str, bwrap: &str) -> StubHost {
            let distro = wsl::distro_name(profile);
            let inside = |argv: &str| format!("{WSL_PROGRAM} -d {distro} --exec {argv}");
            StubHost::new()
                .with_home("C:/Users/me")
                // No `uname` on a Windows host; `cmd.exe` is what settles it.
                .with_binary("cmd.exe")
                .with_binary_at(wsl::WSL, WSL_PROGRAM)
                .with_command(
                    &format!("{WSL_PROGRAM} --version"),
                    ProbeOutput::success("WSL version: 2.3.26.0\nKernel version: 5.15.167.4\n"),
                )
                .with_command(
                    &format!("{WSL_PROGRAM} --list --verbose"),
                    ProbeOutput::success(format!(
                        "  NAME             STATE           VERSION\n* Ubuntu-24.04     \
                         Running         2\n  {distro}    Stopped         2\n"
                    )),
                )
                .with_command(&inside("true"), ProbeOutput::success(""))
                .with_command(
                    &inside(&format!("cat {}", wsl::MARKER_FILE)),
                    ProbeOutput::success(format!("{profile}\n")),
                )
                .with_command(
                    &inside(&format!("cat {}", wsl::WSL_CONF)),
                    ProbeOutput::success(wsl::plan::WSL_CONF_CONTENTS),
                )
                .with_command(
                    &inside("sh -c printf %s \"$HOME\""),
                    ProbeOutput::success(format!("{home}\n")),
                )
                .with_command(
                    &inside("sh -c command -v bwrap"),
                    ProbeOutput::success(format!("{bwrap}\n")),
                )
        }

        /// A WSL place as the launch path drives it: ensure the distro, then
        /// compose the command that runs inside it.
        ///
        /// Both halves, because a WSL place's refusals are split across them —
        /// the profile's own paths are judged where the distro is ensured, and
        /// the launch's minted directories only exist by the time the argv is
        /// composed.
        fn wsl_path(profile: &str, home: &str) -> PlacePath {
            let backend = WslDistroBackend::new(Arc::new(wsl_host(profile, home, DISTRO_BWRAP)));
            PlacePath {
                kind: SandboxBackendKind::WslDistro,
                ensure: Box::new(move |asked| {
                    let ensured = backend.ensure_distro(asked)?;
                    let policy = asked
                        .resolve(SandboxBackendKind::WslDistro, &ensured.home)
                        .expect("a profile that ensured a distro resolves against its home");
                    backend
                        .wrap(
                            vec!["claude".to_string()],
                            &SandboxLaunch::new(&policy, &ensured.home, "s1")
                                .with_place(PlaceLaunch { relay: None }),
                        )
                        .map(|_| ())
                }),
            }
        }

        /// Every place friring can build, as the seam the launch path drives
        /// each one through.
        fn place_paths(profile: &str, home: &str, paths: &[String]) -> Vec<PlacePath> {
            let mut out: Vec<PlacePath> = subjects(profile, home, paths)
                .into_iter()
                .map(|subject| PlacePath {
                    kind: subject.kind,
                    ensure: Box::new(move |asked| subject.backend.ensure_place(asked).map(|_| ())),
                })
                .collect();
            out.push(wsl_path(profile, home));
            out
        }

        /// Every place backend refuses the same boundary, in its own words.
        ///
        /// The cases are the escapes five adversarial reviews closed on the
        /// first place backend: friring's own data directory **and the database
        /// file inside it** (ADR-29), the tmux socket directory that is a pane in
        /// every session, the container engine's control socket (the whole
        /// host), and a read-write root containing the program that applies the
        /// boundary.
        ///
        /// The database file is its own case because every ADR-29 gate used to
        /// test ancestry alone, and `encloses("<data>/friring.db", "<data>")` is
        /// false: a profile naming the file passed the editor, passed
        /// `sandbox import`, passed the mount check, and got
        /// `--mount type=bind,src=<db>` — an automations row from inside is a
        /// shell command the *host* runs. A policy backend survives that on a
        /// mask it applies whatever the profile says; a place has no mask, so
        /// the refusal below is the whole of the rule.
        #[test]
        fn every_place_backend_refuses_the_same_boundaries() {
            // A data directory of this test's own, outside the tmux socket root.
            // A unit-test build hangs friring's data directory off the platform
            // temp root, which on Linux *is* that socket root — so the tmux case
            // below would name an ancestor of the data directory and be refused
            // by ADR-29 instead, testing the fixture rather than the rule.
            let _paths = crate::paths::TestPathGuard::new(dirs::test_temp_base("conformance-data"));
            // Minted first: `data_dir` is only on disk once something has asked
            // for a place tree, and the case below resolves it.
            dirs::create_place_dirs("conform").unwrap();
            let data = std::fs::canonicalize(dirs::data_dir().unwrap())
                .unwrap()
                .display()
                .to_string();
            // Never created, only named: the point is that a path a profile can
            // write down is refused, and the database of a friring that has
            // never run is a path like any other.
            let database = format!("{data}/friring.db");
            let wal = format!("{database}-wal");
            let home = fake_home("place-conformance");
            let docker_dir = format!("{home}/.docker");
            std::fs::create_dir_all(&docker_dir).unwrap();

            // Resolved, because the tmux socket root is `/tmp` and that is a
            // symlink on macOS: the rule covers both spellings, and naming the
            // unresolved one here would be refused by the symlink rule — which
            // the case below is about — before this one was reached.
            let tmux_root = dirs::tmux_socket_root().display().to_string();
            let tmux_root = dirs::canonical(&tmux_root).unwrap_or(tmux_root);

            // Another profile's place tree, which holds the login its
            // `volume-login` made (ADR-28). Refused at the place's own seam and
            // not only when the profile is stored: a place is ensured from a
            // profile that is already in the database, and the editor is not
            // the only thing that can put one there.
            let other_place = format!("{data}/sandbox/pl/other/home");

            // (the path a profile names, the phrase its refusal must carry)
            let cases: [(&str, &str); 7] = [
                (data.as_str(), "ADR-29"),
                (&database, "ADR-29"),
                // The sidecar is the same escape by a slower route: a `-wal`
                // written from inside is replayed by the host on next open.
                (&wal, "ADR-29"),
                (&other_place, "friring's own"),
                (&tmux_root, "tmux socket directory"),
                (&docker_dir, "control socket"),
                // Both spellings of the Apple CLI's directory are the same one
                // here, and it is where the stubs put every engine binary — and
                // every distro's bubblewrap — too.
                ("/usr/bin", "itself"),
            ];

            for (path, phrase) in cases {
                let asked = profile("conform", vec![SandboxPath::workspace(path)]);
                for place in place_paths("conform", &home, &[path.to_string()]) {
                    let err = (place.ensure)(&asked)
                        .expect_err(&format!("{} must refuse '{path}'", place.kind));
                    let text = err.to_string();
                    assert!(
                        matches!(
                            err,
                            SandboxError::Refused { .. } | SandboxError::Tampered { .. }
                        ),
                        "{}: {text}",
                        place.kind
                    );
                    assert!(
                        text.contains(phrase),
                        "{} refused '{path}' for the wrong reason: {text}",
                        place.kind
                    );
                }
            }
        }

        /// A WSL place refuses the program that applies its boundary at **both**
        /// of its seams, not only where the profile is read.
        ///
        /// `ensure_distro` sees the profile's own read-write paths; a launch
        /// adds writable directories the profile never named — its workspace,
        /// its signal directory, its scratch — and only `wrap` sees those. `wrap`
        /// composes the bubblewrap argv itself rather than going through
        /// [`crate::sandbox::bwrap::BwrapBackend::wrap`], so the check there is
        /// not inherited from anywhere; it has to be its own.
        #[test]
        fn a_wsl_launch_refuses_a_minted_path_that_contains_bubblewrap() {
            let home = fake_home("place-conformance-wsl");
            // Installed from source, which is where a minimal distro's
            // bubblewrap comes from — and the prefix a launch could hand over
            // without the profile ever naming it.
            let bwrap = "/usr/local/bin/bwrap";
            let backend = WslDistroBackend::new(Arc::new(wsl_host("conform-wsl", &home, bwrap)));

            let asked = profile("conform-wsl", vec![SandboxPath::workspace("/srv/work")]);
            let ensured = backend
                .ensure_distro(&asked)
                .expect("the profile names nothing the distro refuses");
            let policy = asked
                .resolve(SandboxBackendKind::WslDistro, &ensured.home)
                .unwrap();

            let err = backend
                .wrap(
                    vec!["claude".to_string()],
                    &SandboxLaunch::new(&policy, &ensured.home, "s1")
                        .with_place(PlaceLaunch { relay: None })
                        .with_workspace("/usr/local"),
                )
                .expect_err("a writable path containing bubblewrap must be refused");
            let text = err.to_string();
            assert!(text.contains("itself"), "{text}");
            assert!(text.contains(bwrap), "{text}");

            // And the other seam on its own: a profile naming the prefix is
            // refused before a distro is handed to anything, which is what the
            // headless `sandbox` commands and the manager view drive.
            let named = profile("conform-wsl", vec![SandboxPath::workspace("/usr/local")]);
            let err = WslDistroBackend::new(Arc::new(wsl_host("conform-wsl", &home, bwrap)))
                .ensure_distro(&named)
                .expect_err("a read-write path containing bubblewrap must be refused");
            assert!(err.to_string().contains("itself"), "{err}");
        }

        /// A mount source reached through a symlink is refused rather than
        /// followed **or rewritten**, on every backend.
        ///
        /// Its own case because it is the one refusal that must not route
        /// through `allow_unsandboxed_fallback`: the commonest way to land here
        /// is an agent inside a place planting a link where the next plan mounts
        /// from, and "the boundary's state is wrong, so run outside it" is a way
        /// out of the sandbox.
        ///
        /// Unix-only for the fixture rather than for the rule: planting the link
        /// this is about takes `std::os::unix::fs::symlink`, and a Windows one
        /// takes a privilege the test process does not have.
        #[cfg(unix)]
        #[test]
        fn every_place_backend_refuses_a_symlinked_mount_source() {
            let base = dirs::test_temp_base("place-conformance-link");
            let home = fake_home("place-conformance-link-home");
            let real = base.join("real");
            let link = base.join("link");
            std::fs::create_dir_all(&real).unwrap();
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let link = link.display().to_string();

            let asked = profile("conform-link", vec![SandboxPath::workspace(&link)]);
            for subject in subjects("conform-link", &home, std::slice::from_ref(&link)) {
                let err = subject
                    .backend
                    .ensure_place(&asked)
                    .expect_err(&format!("{} must refuse a symlinked source", subject.kind));
                assert!(
                    err.is_tampering(),
                    "{}: a symlinked source is interference, not a profile to edit: {err}",
                    subject.kind
                );
                assert!(
                    err.to_string().contains("symlink"),
                    "{}: {err}",
                    subject.kind
                );
            }
        }

        /// Both renderers mount every path at exactly its host path.
        ///
        /// The plan is shared, so this is about the two command lines built from
        /// it: a git linked worktree references its main repository by absolute
        /// path and back, and agents key state by absolute project path, so a
        /// renderer that relocated one source would break resume and `git` in
        /// ways no test of the plan alone would catch. The one deliberate
        /// exception is the synthetic home, which is mounted at a fixed path
        /// inside (ADR-28).
        #[test]
        fn both_renderers_mount_every_path_at_its_own_path() {
            let policy: SandboxPolicy = profile(
                "conform-paths",
                vec![
                    SandboxPath::workspace("/srv/work"),
                    SandboxPath::read_only("/srv/shared"),
                ],
            )
            .resolve(SandboxBackendKind::Docker, "/home/u")
            .unwrap();
            let plan = plan_instance(PlanInput {
                policy: &policy,
                image: "friring/sandbox:1",
                place_dir: "/srv/place",
                home_dir: "/srv/place/home",
                user: None,
                userns_keep_id: false,
                check: MountCheck {
                    friring_db: None,
                    home: Some("/home/u"),
                    exists: &|_| true,
                    resolve: &|path| Ok(path.to_string()),
                },
            })
            .unwrap();

            for (rendered, source_key, target_key) in [
                (create_argv("/usr/bin/docker", &plan), "src=", "dst="),
                (
                    apple::plan::create_argv(APPLE_PROGRAM, &plan, apple::plan::NETWORK),
                    "source=",
                    "target=",
                ),
            ] {
                let mounts: Vec<&String> = rendered
                    .iter()
                    .filter(|token| token.starts_with("type=bind,"))
                    .collect();
                assert!(!mounts.is_empty(), "{rendered:?}");
                for spec in mounts {
                    let field = |key: &str| {
                        spec.split(',')
                            .find_map(|part| part.strip_prefix(key))
                            .map(str::to_string)
                            .unwrap_or_else(|| panic!("{spec} has no {key}"))
                    };
                    let (source, target) = (field(source_key), field(target_key));
                    // The one deliberate exception: the synthetic home lands at
                    // a fixed path inside, because an image built for another
                    // user may not be able to create the host's home path.
                    if source == plan.home_dir {
                        continue;
                    }
                    assert_eq!(source, target, "{spec} relocates a path");
                }
            }
            // And the plan itself carries both of the profile's paths, so the
            // assertion above is about mounts that exist.
            let sources: Vec<&str> = plan.mounts.iter().map(|m| m.source.as_str()).collect();
            assert!(sources.contains(&"/srv/work"), "{sources:?}");
            assert!(sources.contains(&"/srv/shared"), "{sources:?}");
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
        // A place has a verdict of its own — and a WSL place's is bubblewrap's,
        // because bubblewrap is what runs inside it.
        assert_eq!(
            linux
                .inner_sandbox(SandboxBackendKind::WslDistro, &policy, Some(&agent))
                .unwrap()
                .verdict,
            InnerSandboxVerdict::Redundant
        );
        // A kind this build has no object for has no verdict to give.
        assert!(linux
            .inner_sandbox(SandboxBackendKind::Auto, &policy, Some(&agent))
            .is_none());
    }
}
