//! The `docker` / `podman` place backend.
//!
//! A **place** is an environment that outlives an individual command (ADR-26):
//! it is created once per profile, shared by every session that picks that
//! profile, and reached through a transport with tmux running *inside* it. That
//! is the whole difference from a policy backend — nothing here wraps a host
//! process, and everything here has a lifecycle.
//!
//! ```text
//! friring
//!   └─ tmux (host, control mode)
//!        └─ docker exec -i <ctr> tmux …      ← the transport
//!             └─ tmux (in the place)
//!                  └─ friring-cli sandbox relay + the agent   ← [`SandboxBackend::wrap`]
//! ```
//!
//! So this backend implements **both** halves of [`SandboxBackend`]:
//! [`ensure`](SandboxBackend::ensure) makes the place, and
//! [`wrap`](SandboxBackend::wrap) composes the command that runs *in* it — the
//! egress relay beside the agent, because the relay has to live inside the
//! network namespace and the namespace is the container's. A launch composed
//! without a place ([`SandboxLaunch::with_place`]) is refused rather than
//! wrapped: for a policy backend an unwrapped argv is a bug, and for a place it
//! would be an agent running on the host under a profile that says otherwise.
//!
//! What is pure and what is not: [`plan`] turns a policy into mounts, labels,
//! environment and a command line; [`gc`] decides what to reclaim; [`image`]
//! decides what to run. Only this file runs a process, and everything it runs
//! goes through the injected [`ProbeHost`], so no test starts a container.
//!
//! One thing here is neither pure nor a process: a filtered profile's place has
//! to be *shown* to reach the egress proxy's socket before it is used as
//! though it were filtered, because these engines run their daemon on this
//! kernel on a Linux host and inside a Linux VM on a Mac or a Windows box, and
//! nothing about the engine's name says which. See
//! `ContainerBackend::check_proxy_reachable`.

pub mod engine;
pub mod gc;
pub mod image;
pub mod plan;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, OnceLock, PoisonError};
/// Only the dial probe waits on anything here, and it is unix-only.
#[cfg(unix)]
use std::time::Duration;

use std::sync::Arc;

use crate::sandbox::backend::{
    Argv, Availability, Caps, Egress, InnerSandboxVerdict, ProxyEndpoint, ProxyTransport,
    SandboxBackend, SandboxError, SandboxLaunch, SandboxResult,
};
use crate::sandbox::dirs;
use crate::sandbox::egress::{proxy_required, RELAY_PORT};
use crate::sandbox::launcher::relay_launcher_argv;
use crate::sandbox::place::{valid_container_ref, PlaceBackend};
use crate::sandbox::probe::{ProbeHost, ProbeOutput};
use crate::session::{
    NetworkMode, ReadScope, SandboxBackendKind, SandboxInstance, SandboxProfile, SandboxShape,
};

pub use engine::{ContainerEngine, EngineDetails};
pub use gc::{gc_plan, GcInput, GcPlan, InstanceRecord, LiveContainer};
pub use image::{ImageSource, DEFAULT_IMAGE};
pub use plan::{create_argv, plan_instance, InstancePlan, MountCheck, PlanInput, CONTAINER_HOME};

/// friring's own CLI, as a place's image must provide it.
///
/// The relay runs *inside* the boundary, so it has to be a binary the place can
/// execute — the host's copy is the wrong architecture as often as not (a Linux
/// place on a macOS host). It is resolved once, when the place is ensured, and
/// the absolute path that came back is what a launch runs; a place whose image
/// does not carry it refuses a filtered launch with the fix rather than starting
/// an agent that believes it is proxied and reaches nothing.
pub const RELAY_BINARY: &str = "friring-cli";

/// How many loopback ports a place reserves for relays, starting at
/// [`RELAY_PORT`].
///
/// A place is shared by every session of its profile, and they share its network
/// namespace — so unlike a bubblewrap sandbox, where every launch gets a private
/// `127.0.0.1` and one fixed port is enough, two sessions here would collide on
/// it. Each session takes the lowest free port in this span instead, and keeps
/// it across a relaunch. A profile with more concurrent sessions than this has
/// bigger problems than egress.
pub const RELAY_PORT_SPAN: u16 = 64;

/// What friring asks the place to dial its probe listener with.
///
/// tmux rather than a socket tool of friring's own, for two reasons that both
/// come down to what an image is allowed to be. It is already the first line of
/// the image contract (`packaging/sandbox/Containerfile`) — the transport runs
/// tmux *inside* the place — so a place that cannot run it has no session
/// anyway; and it is a unix-socket client whose whole job is "is a server
/// listening on this path", which is exactly the question. A one-shot connect
/// in friring's own CLI would read better and be unusable: `friring-cli` inside
/// a place is the **image's** copy, so a subcommand added today is absent from
/// every image built before it, and a probe that refused those would take a
/// working sandbox away for lack of a binary friring cannot ship into it.
///
/// [`DIAL_COMMAND`] is chosen to be one that never starts a server: this asks a
/// question of a socket, it does not put anything on one.
const DIALER: &str = "tmux";

/// The tmux command the probe runs, and tmux's own answer when the connect
/// failed.
///
/// `list-sessions` carries no `CMD_STARTSERVER`, so a socket it cannot reach
/// leaves nothing behind — it prints [`DIAL_REFUSED`] and exits. That sentence
/// is how the probe tells "the place tried and the kernel said no" from "the
/// place never got to try", which are two different refusals with two different
/// fixes.
#[cfg(unix)]
const DIAL_COMMAND: &str = "list-sessions";

/// tmux's wording for a `connect(2)` that did not reach a listener.
///
/// Unix-only with the rest of the dial probe: friring binds the listener the
/// place dials, and there is none to bind on a host without unix sockets — see
/// [`probe_socket_reach`].
#[cfg(unix)]
const DIAL_REFUSED: &str = "no server running on";

/// How often the probe's listener is polled while the place is being asked to
/// dial it.
#[cfg(unix)]
const DIAL_POLL: Duration = Duration::from_millis(5);

/// How long the probe keeps listening after the dial command has returned.
///
/// `listen(2)` queues a connection the moment the client's `connect(2)`
/// completes, so a client that came and went while the loop was asleep is still
/// there to accept — but only if the loop is still running. Paid solely on the
/// failing path: a probe that has already seen its connection stops at once.
#[cfg(unix)]
const DIAL_GRACE: Duration = Duration::from_millis(250);

/// The `state` a `sandbox_instances` row carries for a place this backend
/// handed back.
///
/// The only one it ever writes: [`ContainerBackend::ensure_place`] returns when
/// the place is up or not at all, so a row that says anything else was written
/// by something that is no longer true.
pub const INSTANCE_STATE_RUNNING: &str = "running";

/// A place that exists and is running, with everything a launch needs to reach
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsuredPlace {
    /// What the caller records in `sandbox_instances`.
    pub instance: SandboxInstance,
    /// The synthetic per-profile home **on the host**, which this place mounts
    /// at [`CONTAINER_HOME`]. What a launch projects configuration into and
    /// keeps the profile's login in — taken from the plan rather than re-derived,
    /// so it is by construction the directory that was mounted.
    pub home_dir: String,
    /// The absolute path of [`RELAY_BINARY`] inside the place, when the profile
    /// needs one.
    pub relay_program: Option<String>,
}

/// Docker or Podman.
pub struct ContainerBackend {
    engine: ContainerEngine,
    host: Arc<dyn ProbeHost>,
    details: OnceLock<EngineDetails>,
    /// Which loopback port each session's relay took, per place. See
    /// [`RELAY_PORT_SPAN`].
    ///
    /// In memory rather than in the database on purpose: a proxy lives in the
    /// friring process that started it (`docs/SANDBOX.md` §Launch integration),
    /// so the set of relays that exist is exactly the set this process
    /// composed.
    ports: Mutex<HashMap<String, BTreeMap<String, u16>>>,
    /// The places that have been *shown* to reach a unix socket friring bound
    /// outside them — see [`ContainerBackend::check_proxy_reachable`].
    ///
    /// Keyed on the container id, so a rebuilt place is asked again. Only the
    /// affirmative answer is kept: it is a property of a live container's kernel
    /// and mount, and cannot change while that container exists, whereas a
    /// failure may be a wedged daemon that the next launch should re-ask rather
    /// than inherit a refusal from.
    reached: Mutex<HashSet<String>>,
}

impl ContainerBackend {
    pub fn new(engine: ContainerEngine, host: Arc<dyn ProbeHost>) -> Self {
        Self {
            engine,
            host,
            details: OnceLock::new(),
            ports: Mutex::new(HashMap::new()),
            reached: Mutex::new(HashSet::new()),
        }
    }

    pub fn engine(&self) -> ContainerEngine {
        self.engine
    }

    /// The probe's full answer, cached with the availability.
    pub fn details(&self) -> &EngineDetails {
        self.details
            .get_or_init(|| engine::probe(self.engine, self.host.as_ref()))
    }

    /// The vetted absolute path of the engine CLI, or the probe's own reason.
    fn program(&self) -> SandboxResult<&str> {
        let details = self.details();
        details
            .program
            .as_deref()
            .ok_or_else(|| SandboxError::Unavailable {
                backend: self.kind(),
                reason: details.availability.message(),
            })
    }

    /// Make sure `profile`'s place exists and is running, and answer with
    /// everything a launch into it needs.
    ///
    /// Idempotent, and cheap when the place is already there: one `inspect`.
    /// Reuses a healthy container, starts a stopped one, and rebuilds one that
    /// will not start — leaving the old container behind for [`gc_plan`] rather
    /// than removing it here, because a container the caller has not yet
    /// recorded is a container nothing could find again if this call died
    /// halfway.
    ///
    /// # Errors
    ///
    /// The engine is unavailable, the profile cannot be resolved, a mount cannot
    /// be honoured (`plan::plan_instance`), the image is missing and cannot be
    /// built, the container will not start, or a filtered profile's place cannot
    /// be shown to reach the egress proxy's socket (`check_proxy_reachable`).
    pub fn ensure_place(&self, profile: &SandboxProfile) -> SandboxResult<EnsuredPlace> {
        let refuse = |detail: String| SandboxError::Refused {
            profile: profile.name.clone(),
            detail,
        };
        let (plan, source, policy) = self.plan_for(profile)?;
        self.ensure_image(&policy.profile, &source)?;

        let id = self.start_or_create(&plan, &refuse)?;
        let relay_program = if proxy_required(&policy) {
            // Order matters to the sentence a user reads: the image contract is
            // asked first, because "your image has no friring-cli" is a fix in
            // the image and "this place cannot dial a socket friring bound" is a
            // fix in the host or the profile, and quoting the second at someone
            // whose image is simply incomplete would send them the wrong way.
            let relay = self.resolve_relay(&id, &plan.image, &refuse)?;
            self.check_proxy_reachable(&id, &policy, &refuse)?;
            Some(relay)
        } else {
            None
        };

        Ok(EnsuredPlace {
            instance: SandboxInstance {
                profile: policy.profile.clone(),
                engine: self.kind(),
                external_id: id,
                state: INSTANCE_STATE_RUNNING.to_string(),
            },
            home_dir: plan.home_dir.clone(),
            relay_program,
        })
    }

    /// What `profile`'s place would be, without creating or starting one.
    ///
    /// The half of [`ensure_place`](Self::ensure_place) that only decides. It
    /// does mint the place's own directories, because a bind mount's source has
    /// to exist before it can be planned — the engine creates a missing one as
    /// root, and a directory the sandbox's user cannot write is a place whose
    /// agent dies on first launch — but those are friring's own, `0700`, and
    /// idempotent.
    ///
    /// # Errors
    ///
    /// The engine is unavailable, the profile will not resolve against it, or a
    /// mount cannot be honoured.
    fn plan_for(
        &self,
        profile: &SandboxProfile,
    ) -> SandboxResult<(InstancePlan, ImageSource, crate::session::SandboxPolicy)> {
        // Before anything is created on disk: an unavailable engine must fail
        // with the probe's own actionable sentence, not with whatever the first
        // command it could not run said.
        self.program()?;
        let refuse = |detail: String| SandboxError::Refused {
            profile: profile.name.clone(),
            detail,
        };
        let home = self.host.home().ok_or_else(|| {
            refuse(
                "friring could not resolve a home directory on this host, and a profile's paths \
                 are written relative to one"
                    .to_string(),
            )
        })?;
        let policy = profile
            .resolve(self.kind(), &home)
            .map_err(|detail| refuse(detail.to_string()))?;
        // Before anything is minted on disk, for the same reason the engine's
        // availability is: a profile that hands the sandbox the engine binary
        // has no place worth building.
        self.check_engine_containment(&policy, &refuse)?;

        let (place_dir, home_dir) = dirs::create_place_dirs(&policy.profile)?;
        let (place_dir, home_dir) = (
            representable(&place_dir, "the place directory", &refuse)?,
            representable(&home_dir, "the place's home directory", &refuse)?,
        );
        let source = image::resolve(&policy)?;
        let details = self.details();
        let exists = |path: &str| self.host.path_exists(path);
        let plan = plan_instance(PlanInput {
            policy: &policy,
            image: source.reference(),
            place_dir: &place_dir,
            home_dir: &home_dir,
            user: details.run_as_user(),
            userns_keep_id: details.userns_keep_id(self.engine),
            check: MountCheck {
                // No launch here, so no other machine's data directory to
                // protect: `protected_data_dirs` still covers this host's own,
                // which is the one a container on it could reach.
                friring_db: None,
                home: Some(&home),
                exists: &exists,
                resolve: &dirs::place_mount_source,
            },
        })?;
        Ok((plan, source, policy))
    }

    /// Refuse a profile that hands the sandbox the engine's own binary.
    ///
    /// The probe vetted the CLI against the *host* (see
    /// [`dirs::rewritable_root`]); this profile decides what the agent can
    /// write, and one granting the directory the engine lives in grants the
    /// program that asks for the isolation — an engine at a user-writable
    /// prefix (`/usr/local/bin`, `/opt/homebrew/bin`) plus a profile making that
    /// prefix read-write is the sandbox choosing what the *next* launch runs.
    /// The check and its sentence mirror
    /// [`BwrapBackend::wrap`](crate::sandbox::bwrap::BwrapBackend), which
    /// refuses the same shape for the same reason.
    ///
    /// The rule is [`dirs::program_in_writable_root`]; a program this friring
    /// cannot
    /// resolve — a remote host's, which is not on this filesystem — leaves the
    /// literal comparison standing rather than refusing a launch over a question
    /// friring could not ask.
    fn check_engine_containment(
        &self,
        policy: &crate::session::SandboxPolicy,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        let program = self.program()?;
        let resolved = dirs::canonical(program);
        let Some((root, found)) =
            dirs::program_in_writable_root(&policy.rw_paths, program, resolved.as_deref())
        else {
            return Ok(());
        };
        let named = if found == program {
            format!("'{program}'")
        } else {
            format!("'{program}', which resolves to '{found}'")
        };
        Err(refuse(format!(
            "the read-write path '{root}' contains {} itself ({named}), so the sandbox could \
             replace the program that applies its own boundary",
            self.engine
        )))
    }

    /// The spec digest `profile` resolves to **right now** — what decides
    /// whether a running place still describes it.
    ///
    /// Garbage collection's input, and `None` is deliberately not an answer of
    /// "no place should exist": a profile that cannot be planned (an engine
    /// that is down, a mount source that is temporarily missing) has *no
    /// opinion*, and reclaiming on the strength of a planning failure would take
    /// a running agent's container away over a missing external disk.
    pub fn current_spec(&self, profile: &SandboxProfile) -> Option<String> {
        self.plan_for(profile).ok().map(|(plan, _, _)| plan.spec)
    }

    /// Adopt the place this plan names, or build it. Answers with its id.
    fn start_or_create(
        &self,
        plan: &InstancePlan,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        let Some(existing) = self.inspect(&plan.name) else {
            return self.create(plan, refuse);
        };
        if !existing.owned {
            // Somebody else's container is sitting on the name friring wants.
            // Neither adopting nor removing it is friring's to do — the whole
            // point of the owner label is that this branch exists.
            return Err(refuse(format!(
                "a container named '{}' already exists and was not created by friring, so it is \
                 neither reused nor removed. Rename that container, or rename the profile",
                plan.name
            )));
        }
        // A container of ours whose *name* matches also matches its spec: the
        // digest is part of the name precisely so an edited profile asks for a
        // different container rather than silently reusing mounts it no longer
        // describes.
        if existing.running() {
            return Ok(existing.id);
        }
        if self
            .engine_run(&["start", &plan.name])
            .is_ok_and(|output| output.ok())
        {
            return Ok(existing.id);
        }
        // It will not start — a place whose kernel resources went with a host
        // reboot, or one wedged in `dead`. Remove it by name (the name is what
        // the replacement needs) and build again.
        let _ = self.engine_run(&["rm", "--force", &existing.id]);
        self.create(plan, refuse)
    }

    fn create(
        &self,
        plan: &InstancePlan,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        let program = self.program()?;
        // The mounts were decided when the plan was built, and that can be
        // minutes ago: an image pull or a build sits between the two, and every
        // source in the plan is a path some *other* place's agent may be writing
        // the whole time. So they are re-checked here, with nothing between the
        // check and the spawn but composing the argv — see
        // [`MountCheck::check`] for the window that remains.
        let home = self.host.home();
        let exists = |path: &str| self.host.path_exists(path);
        MountCheck {
            friring_db: None,
            home: home.as_deref(),
            exists: &exists,
            resolve: &dirs::place_mount_source,
        }
        .check(&plan.mounts, refuse)?;
        let argv = create_argv(program, plan);
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let output = self
            .host
            .run(program, &args)
            .map_err(|detail| refuse(format!("the {} could not be run: {detail}", self.engine)))?;
        if !output.ok() {
            return Err(refuse(format!(
                "{} could not create the sandbox container: {}",
                self.engine,
                first_line(&output.stderr, "the engine gave no reason")
            )));
        }
        // `run --detach` prints the new container's full id, last — an engine
        // that also wrote a pull line put it above. The id is checked against
        // the engines' own name grammar before it is recorded or used: it
        // becomes the argument right after `exec`'s flags on every later command
        // line, and a value that could pass for a flag must never get there.
        let id = output
            .stdout
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .filter(|id| valid_container_ref(id))
            .ok_or_else(|| {
                refuse(format!(
                    "{} did not answer with a container id friring can use, so there is nothing \
                     to record or to reach",
                    self.engine
                ))
            })?;
        Ok(id.to_string())
    }

    /// Make sure the image is there, building or pulling it when that is what
    /// its source allows.
    fn ensure_image(&self, profile: &str, source: &ImageSource) -> SandboxResult<()> {
        if self
            .engine_run(&["image", "inspect", source.reference()])
            .is_ok_and(|output| output.ok())
        {
            return Ok(());
        }
        match source {
            ImageSource::Default => Err(source.missing(profile)),
            ImageSource::Named(reference) => match self.engine_run(&["pull", reference]) {
                Ok(output) if output.ok() => Ok(()),
                _ => Err(source.missing(profile)),
            },
            ImageSource::Built { tag, containerfile } => {
                let context = image::build_context(containerfile);
                match self.engine_run(&["build", "-t", tag, "-f", containerfile, context]) {
                    Ok(output) if output.ok() => Ok(()),
                    _ => Err(source.missing(profile)),
                }
            }
        }
    }

    /// Where friring's own CLI lives inside the place.
    ///
    /// Resolved once per ensure and pinned, rather than named on the launch's
    /// command line: an agent with a writable home could put something earlier
    /// on `PATH`, and while the relay holds neither the token nor the policy —
    /// both stay outside the boundary — a launch that silently ran the agent's
    /// own binary in its place would be a boundary reporting something it did
    /// not do.
    fn resolve_relay(
        &self,
        container: &str,
        image: &str,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        let resolved = self
            .engine_run(&[
                "exec",
                container,
                "/bin/sh",
                "-c",
                &format!("command -v {RELAY_BINARY}"),
            ])
            .ok()
            .filter(ProbeOutput::ok)
            .map(|output| output.trimmed().to_string())
            .filter(|path| path.starts_with('/'));
        resolved.ok_or_else(|| {
            refuse(format!(
                "this profile filters egress, which a place reaches through '{RELAY_BINARY}' \
                 running inside it — and the image '{image}' does not carry one on PATH. Use the \
                 image built from friring's own Containerfile (packaging/sandbox/Containerfile), \
                 or add friring-cli to yours"
            ))
        })
    }

    /// Prove that this place can reach a unix socket friring binds outside it,
    /// and refuse the profile when it cannot.
    ///
    /// Every filtered mode is enforced by friring's proxy *outside* the
    /// boundary, reached over a bind-mounted unix socket and a relay inside the
    /// namespace (ADR-27). That works where the place shares friring's kernel,
    /// because an `AF_UNIX` listener lives in the kernel that called `bind(2)`
    /// — and **docker and podman are not always on this kernel**. Docker
    /// Desktop, `podman machine` and colima all run the daemon in a Linux VM, so
    /// a mount carries the socket *file* across and `connect(2)` on it inside
    /// finds no listener in the guest's own table. The place would then be
    /// started on `--network none` with a relay dialling nothing: no egress at
    /// all, under a profile whose UI says the allowlist is being applied.
    ///
    /// So it is **asked rather than assumed**. friring binds a listener under
    /// the place's own directory — which the plan mounts at the same absolute
    /// path — and has the place dial it ([`DIALER`]). The observation is
    /// friring's own accept on the host side, not a guest tool's exit status:
    /// what is being measured is whether the connection crossed at all.
    /// Assuming instead would mean keeping a list of which engines are
    /// VM-backed on which hosts, which is a list that goes stale.
    ///
    /// Both failing answers refuse, and neither is silent. The place is left
    /// running: it is the profile's, shared by its sessions, and a profile whose
    /// network the user sets to `full` uses the very same container.
    ///
    /// A place's own agent can write that directory, so it can unlink or replace
    /// the probe socket. Neither buys anything: the worst it can do is make the
    /// probe fail (a refusal), or make it pass on a host where the socket does
    /// not really carry — which starts a place with *less* network than the
    /// profile claims, never more.
    ///
    /// # Errors
    ///
    /// The place dialled and the connect was refused, friring could not get it
    /// to dial at all, or the listener could not be bound.
    fn check_proxy_reachable(
        &self,
        container: &str,
        policy: &crate::session::SandboxPolicy,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        if self
            .reached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(container)
        {
            return Ok(());
        }
        let place_dir = dirs::place_dir(&policy.profile).ok_or_else(|| {
            refuse(
                "friring could not resolve its data directory, so it has nowhere to bind the \
                 socket this place would have to dial"
                    .to_string(),
            )
        })?;
        let place_dir = representable(&place_dir, "the place directory", refuse)?;
        self.probe_reach(container, &place_dir, policy, refuse)
    }

    /// Bind the listener, have the place dial it, and turn what happened into
    /// the launch's answer — recording a place that crossed, so its next session
    /// is not measured again.
    #[cfg(unix)]
    fn probe_reach(
        &self,
        container: &str,
        place_dir: &str,
        policy: &crate::session::SandboxPolicy,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        let dial = |socket: &str| self.dial_from_place(container, socket);
        match probe_socket_reach(place_dir, &dial) {
            Ok((Reach::Crossed, _)) => {
                self.reached
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(container.to_string());
                Ok(())
            }
            Ok((Reach::Refused, socket)) => {
                Err(refuse(socket_does_not_carry(policy, self.engine, &socket)))
            }
            Ok((Reach::Unprovable, socket)) => Err(refuse(reach_unprovable(
                policy,
                &format!(
                    "friring bound a listener at '{socket}' and asked the place to connect to it \
                     with '{DIALER}', which answered with nothing friring can read"
                ),
            ))),
            Err(detail) => Err(refuse(reach_unprovable(policy, &detail))),
        }
    }

    /// A host with no unix sockets has nothing for a place to dial, and nothing
    /// for the proxy to listen on either (ADR-27) — so the question is answered
    /// without asking it, and the filtered profile is refused rather than
    /// started with a relay dialling nothing.
    #[cfg(not(unix))]
    fn probe_reach(
        &self,
        _container: &str,
        _place_dir: &str,
        policy: &crate::session::SandboxPolicy,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        Err(refuse(reach_unprovable(
            policy,
            "this host has no unix sockets, and the egress proxy a filtered mode needs is reached \
             over one",
        )))
    }

    /// Ask the place to connect to `socket`, which is at the same absolute path
    /// inside it as on the host.
    ///
    /// The exit status is deliberately ignored — a dialer that reached friring's
    /// listener and then failed to speak its protocol to it has still answered
    /// the only question being asked. Its *output* is read for one thing:
    /// [`DIAL_REFUSED`], which separates a connect the kernel turned down from a
    /// dialer that never ran.
    ///
    /// Unix-only with the listener it dials, and with the socket path that would
    /// be named to it.
    #[cfg(unix)]
    fn dial_from_place(&self, container: &str, socket: &str) -> Result<ProbeOutput, String> {
        self.engine_run(&["exec", container, DIALER, "-S", socket, DIAL_COMMAND])
    }

    /// What the engine says about one container, or `None` when it has none.
    fn inspect(&self, name_or_id: &str) -> Option<Inspected> {
        let output = self
            .engine_run(&[
                "inspect",
                "--type",
                "container",
                "--format",
                INSPECT_FORMAT,
                name_or_id,
            ])
            .ok()
            .filter(ProbeOutput::ok)?;
        Inspected::parse(output.trimmed())
    }

    /// Every container friring created under this engine, as garbage collection
    /// sees them.
    ///
    /// Filtered on friring's own label at the engine, and each result is
    /// re-checked for it — [`gc_plan`] never names a container whose
    /// [`LiveContainer::owned`] is false, so a mistyped filter cannot widen what
    /// is reclaimed.
    pub fn live_places(&self) -> SandboxResult<Vec<LiveContainer>> {
        let listed = self
            .engine_run(&[
                "ps",
                "--all",
                "--no-trunc",
                "--quiet",
                "--filter",
                &format!("label={}=1", plan::LABEL_OWNER),
            ])
            .map_err(|detail| SandboxError::Unavailable {
                backend: self.kind(),
                reason: detail,
            })?;
        if !listed.ok() {
            return Err(SandboxError::Unavailable {
                backend: self.kind(),
                reason: first_line(&listed.stderr, "the engine would not list its containers"),
            });
        }
        Ok(listed
            .stdout
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .filter_map(|id| self.inspect(id))
            .map(Inspected::into_live)
            .collect())
    }

    /// Carry out a [`GcPlan`]'s removals, answering with the ones that failed.
    ///
    /// Best effort and non-fatal: a container that will not go is a container
    /// the next pass tries again. Each id is checked against friring's own label
    /// one final time, because this is the only function here that destroys
    /// something.
    pub fn reap(&self, plan: &GcPlan) -> Vec<String> {
        let mut failures = Vec::new();
        for id in &plan.remove {
            match self.inspect(id) {
                // Already gone between planning and reaping — the outcome this
                // pass wanted, so not a failure.
                None => continue,
                Some(container) if !container.owned => {
                    failures.push(format!(
                        "{id}: not a container friring created, so it was left alone"
                    ));
                    continue;
                }
                Some(_) => {}
            }
            match self.engine_run(&["rm", "--force", id]) {
                Ok(output) if output.ok() => {}
                Ok(output) => failures.push(format!(
                    "{id}: {}",
                    first_line(&output.stderr, "the engine gave no reason")
                )),
                Err(detail) => failures.push(format!("{id}: {detail}")),
            }
        }
        failures
    }

    /// The absolute engine binary the probe resolved and vetted.
    ///
    /// What the sandbox transport is built from: it addresses a place as
    /// `<engine> exec -i <container> tmux …`, and the engine has to be the path
    /// this probe pinned rather than a name re-resolved through whatever `PATH`
    /// friring inherited — the engine's socket is commonly root-equivalent, so
    /// choosing what runs it is choosing what the boundary is.
    ///
    /// # Errors
    ///
    /// The engine is unavailable; the message is the probe's own.
    pub fn engine_program(&self) -> SandboxResult<&str> {
        self.program()
    }

    /// The loopback port this session's relay listens on inside `container`.
    ///
    /// Stable for a session across relaunches, and never handed to two sessions
    /// at once — see [`RELAY_PORT_SPAN`] for why a place cannot use one fixed
    /// port the way a bubblewrap sandbox can.
    ///
    /// That is **addressing, not separation**: the loopback belongs to the place
    /// and every session in it can dial any of these ports (`docs/SANDBOX.md`
    /// §The two sandbox shapes).
    ///
    /// # Errors
    ///
    /// The place already has [`RELAY_PORT_SPAN`] sessions with a relay.
    pub fn relay_port(&self, container: &str, session_key: &str) -> SandboxResult<u16> {
        let mut ports = self.ports.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = ports.entry(container.to_string()).or_default();
        let port = assign_port(taken, session_key, RELAY_PORT, RELAY_PORT_SPAN).ok_or(
            SandboxError::Unsupported {
                backend: self.kind(),
                detail: format!(
                    "this sandbox already has {RELAY_PORT_SPAN} sessions with an egress relay, \
                     and they share the place's loopback"
                ),
            },
        )?;
        taken.insert(session_key.to_string(), port);
        Ok(port)
    }

    /// Forget a session's relay port, so the place can hand it to another.
    pub fn release_relay_port(&self, container: &str, session_key: &str) {
        let mut ports = self.ports.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(taken) = ports.get_mut(container) {
            taken.remove(session_key);
            if taken.is_empty() {
                ports.remove(container);
            }
        }
    }

    /// Forget every port a session held, whichever place it held them in.
    ///
    /// What teardown can actually call: a session ending holds its own key and
    /// not a container id — the place it ran in outlives it, and an edited
    /// profile may already have superseded the one it was launched into. A
    /// place that never got the release back would run out of its
    /// [`RELAY_PORT_SPAN`] after enough sessions had come and gone.
    pub fn release_relay_ports(&self, session_key: &str) {
        let mut ports = self.ports.lock().unwrap_or_else(PoisonError::into_inner);
        ports.retain(|_, taken| {
            taken.remove(session_key);
            !taken.is_empty()
        });
    }

    /// Run the engine with these arguments, resolving the program once.
    fn engine_run(&self, args: &[&str]) -> Result<ProbeOutput, String> {
        let program = self.program().map_err(|error| error.to_string())?;
        self.host.run(program, args)
    }
}

/// The lifecycle seam every caller of a place shares (`crate::sandbox::place`).
///
/// Delegation rather than indirection: the inherent methods above are the
/// implementations and stay this backend's documented API, and this block is
/// what lets the launch path, teardown, the reclaiming pass and `friring-cli`
/// name one interface instead of one accessor per tool.
impl PlaceBackend for ContainerBackend {
    fn ensure_place(&self, profile: &SandboxProfile) -> SandboxResult<EnsuredPlace> {
        ContainerBackend::ensure_place(self, profile)
    }

    fn ensure_agent_program(
        &self,
        policy: &crate::session::SandboxPolicy,
        place: &EnsuredPlace,
        program: &str,
    ) -> SandboxResult<()> {
        ContainerBackend::ensure_agent_program(self, policy, place, program)
    }

    fn current_spec(&self, profile: &SandboxProfile) -> Option<String> {
        ContainerBackend::current_spec(self, profile)
    }

    fn engine_program(&self) -> SandboxResult<&str> {
        ContainerBackend::engine_program(self)
    }

    fn live_places(&self) -> SandboxResult<Vec<LiveContainer>> {
        ContainerBackend::live_places(self)
    }

    fn reap(&self, plan: &GcPlan) -> Vec<String> {
        ContainerBackend::reap(self, plan)
    }

    fn relay_port(&self, container: &str, session_key: &str) -> SandboxResult<u16> {
        ContainerBackend::relay_port(self, container, session_key)
    }

    fn release_relay_ports(&self, session_key: &str) {
        ContainerBackend::release_relay_ports(self, session_key);
    }
}

impl SandboxBackend for ContainerBackend {
    fn kind(&self) -> SandboxBackendKind {
        self.engine.kind()
    }

    fn probe(&self) -> Availability {
        self.details().availability.clone()
    }

    fn capabilities(&self) -> Caps {
        Caps {
            shape: SandboxShape::Place,
            // The one shape that can: a cgroup is the engine's to set, and both
            // engines take `--memory` and `--cpus`.
            limits: true,
            // Every mode, because these engines can express every mode — but a
            // *filtered* one also needs the proxy's socket to carry a listener
            // into the place, which is a property of the host this engine's
            // daemon happens to run on rather than of the engine. Nothing here
            // can consult that: capabilities are static and are read while the
            // UI paints, and the answer needs a running place. It is measured
            // once per place instead, in
            // [`ContainerBackend::check_proxy_reachable`], which refuses the
            // profile with the fix rather than letting this list promise
            // something the boundary would not deliver.
            network_modes: NetworkMode::ALL,
            // A place has no host filesystem to read. `host-minus-secrets` is a
            // statement about the machine friring runs on, and none of it is in
            // here unless the profile lists it.
            read_scopes: &[ReadScope::Workspace],
            persistent: true,
            // The host credential store is not reachable from inside, by
            // construction — which is the whole reason ADR-28 offers a token or
            // a per-profile login instead of copying one in.
            host_credentials: false,
            inner_agent_sandbox: InnerSandboxVerdict::Redundant,
            // `--network none` leaves the place its own loopback and no route to
            // the host's, so a bind-mounted socket is the only transport that
            // *could* cross. Whether it does on this host is the question
            // `check_proxy_reachable` settles.
            proxy_transport: ProxyTransport::UnixSocket,
        }
    }

    /// Compose the command that runs **inside** the place: the egress relay
    /// beside the agent.
    ///
    /// The place backend's half of the argv seam. Nothing here names the engine
    /// — reaching the place is the transport's job — so what comes out is what
    /// tmux *in* the container is asked to run.
    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv> {
        if launch.policy.backend != self.kind() {
            return Err(SandboxError::Unsupported {
                backend: self.kind(),
                detail: format!(
                    "policy was resolved for '{}'; resolve it for {} first",
                    launch.policy.backend, self.engine
                ),
            });
        }
        launch.validate()?;
        // A launch with no place is a launch that never called `ensure`, and
        // returning the argv unchanged would run the agent on the *host* under a
        // profile that says it is in a container. Refusing routes it through the
        // profile's own `allow_unsandboxed_fallback` switch instead, like every
        // other boundary friring will not grant.
        let place = launch.place.ok_or_else(|| SandboxError::Refused {
            profile: launch.policy.profile.clone(),
            detail: format!(
                "this profile runs in a {} place, whose command is composed for the inside of \
                 the container; this launch was built without one",
                self.engine
            ),
        })?;

        let socket = match launch.egress() {
            Egress::Open | Egress::Closed => return Ok(argv),
            Egress::Proxied(ProxyEndpoint::UnixSocket { inside_path, .. }) => inside_path.clone(),
            Egress::Proxied(ProxyEndpoint::Loopback { .. }) => {
                return Err(SandboxError::Unsupported {
                    backend: self.kind(),
                    detail: "a container on '--network none' has no route to host loopback; the \
                             egress proxy must expose a unix socket for this backend"
                        .to_string(),
                })
            }
        };
        // Proxied, so the relay is the only way out of the place — a launch
        // composed without one would start an agent whose proxy environment
        // names a port nothing listens on, under a profile claiming a filtered
        // network. Refusing routes it through `allow_unsandboxed_fallback`.
        let relay = place.relay.ok_or_else(|| SandboxError::Refused {
            profile: launch.policy.profile.clone(),
            detail: format!(
                "this profile's network mode is enforced by the egress proxy, which a place \
                 reaches through '{RELAY_BINARY}' running inside it; this launch was built \
                 without one"
            ),
        })?;
        let listen = format!("127.0.0.1:{}", relay.port);
        let mut out = relay_launcher_argv(relay.program, &listen, &socket);
        out.extend(argv);
        Ok(out)
    }

    fn ensure(&self, profile: &SandboxProfile) -> SandboxResult<SandboxInstance> {
        Ok(self.ensure_place(profile)?.instance)
    }
}

/// The `inspect` template: id, status, and friring's two labels.
///
/// One line, `|`-separated, because both engines render the same container JSON
/// and neither guarantees the same *list* template — `ps --format` differs
/// between them, `inspect --format` does not.
///
/// `pub(crate)` so a test elsewhere can script an engine's answer for a named
/// container rather than for whatever the first `inspect` happens to be.
pub(crate) const INSPECT_FORMAT: &str = "{{.Id}}|{{.State.Status}}|{{index .Config.Labels \
                              \"dev.friring.sandbox\"}}|{{index .Config.Labels \
                              \"dev.friring.sandbox.profile\"}}|{{index .Config.Labels \
                              \"dev.friring.sandbox.spec\"}}";

/// One container as `inspect` described it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Inspected {
    id: String,
    status: String,
    owned: bool,
    profile: Option<String>,
    spec: Option<String>,
}

impl Inspected {
    /// Parse one `INSPECT_FORMAT` line. `None` when it is not one — an engine
    /// that answered something else is not a container friring may act on.
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.trim().split('|');
        let id = fields.next()?.trim();
        let status = fields.next()?.trim();
        let owner = fields.next().map(str::trim).unwrap_or_default();
        let profile = label(fields.next());
        let spec = label(fields.next());
        if id.is_empty() {
            return None;
        }
        Some(Self {
            id: id.to_string(),
            status: status.to_string(),
            owned: owner == "1",
            profile,
            spec,
        })
    }

    fn running(&self) -> bool {
        self.status == "running"
    }

    fn into_live(self) -> LiveContainer {
        LiveContainer {
            id: self.id,
            profile: self.profile,
            spec: self.spec,
            owned: self.owned,
        }
    }
}

/// A label as a Go template renders a missing one.
fn label(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty() && *value != "<no value>")
        .map(str::to_string)
}

/// The lowest free port in `base..base + span`, or the one this session already
/// holds.
///
/// Stable across a relaunch, because the session keeps its entry: the launch
/// that replaces it composes the same address into the same environment, and a
/// port that moved would leave the previous launch's environment naming a
/// listener that is no longer there.
fn assign_port(
    taken: &BTreeMap<String, u16>,
    session_key: &str,
    base: u16,
    span: u16,
) -> Option<u16> {
    if let Some(port) = taken.get(session_key) {
        return Some(*port);
    }
    (base..base.saturating_add(span)).find(|port| !taken.values().any(|held| held == port))
}

/// What one reachability probe settled.
///
/// Unix-only, with the probe that decides it: a host without unix sockets never
/// asks the question — see `ContainerBackend::probe_reach`.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// The place dialled friring's listener: a unix socket carries a *listener*
    /// across this boundary, which is what a filtered mode is built on.
    Crossed,
    /// The place tried and the connect was turned down. The socket file crosses;
    /// what is bound behind it does not.
    Refused,
    /// friring could not get the place to try, so it knows nothing either way —
    /// which is not the same as knowing it works, and is refused for that
    /// reason.
    Unprovable,
}

/// Decide a probe from what friring saw and what the dialer said.
///
/// Separated from the IO so the whole decision is assertable: `observed` is
/// friring's own accept on the host side, which is the measurement; the dialer's
/// output only ever downgrades "friring saw nothing" into the more precise
/// [`Reach::Refused`].
#[cfg(unix)]
fn dial_verdict(observed: bool, dialled: &Result<ProbeOutput, String>) -> Reach {
    if observed {
        return Reach::Crossed;
    }
    let said = match dialled {
        Ok(output) => format!("{}{}", output.stdout, output.stderr),
        Err(detail) => detail.clone(),
    };
    if said.contains(DIAL_REFUSED) {
        Reach::Refused
    } else {
        Reach::Unprovable
    }
}

/// Bind a listener under `place_dir`, have `dial` reach for it from inside the
/// place, and answer with what happened and the path that was dialled.
///
/// The listener is friring's, on the host, in a directory the place mounts at
/// the same absolute path — so the only thing standing between the two is the
/// boundary itself. Bound `0600` inside a `0700` directory, removed before this
/// returns, and named with a nonce so two friring processes probing the same
/// place at once do not collide on it (and so nothing planted at a predictable
/// name is ever adopted: `bind(2)` fails on an existing path rather than
/// replacing it, which refuses the launch instead of dialling a socket somebody
/// else is holding).
///
/// The accept runs on a thread of its own because the dial blocks: a client's
/// `connect(2)` completes as soon as the kernel queues it, but a dialer that
/// then waits for a reply waits until something accepts.
///
/// # Errors
///
/// The path cannot be named exactly, or the listener cannot be bound.
#[cfg(unix)]
fn probe_socket_reach(
    place_dir: &str,
    dial: &dyn Fn(&str) -> Result<ProbeOutput, String>,
) -> Result<(Reach, String), String> {
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = format!(
        "{}-{}-{:?}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
    );
    let path = std::path::Path::new(place_dir).join(format!("re{}.sock", dirs::digest(&nonce)));
    let socket = path
        .to_str()
        .ok_or_else(|| {
            format!(
                "the probe socket path ('{}') is not valid UTF-8, so friring cannot name it to \
                 the place",
                path.display()
            )
        })?
        .to_string();

    let listener = UnixListener::bind(&path).map_err(|e| {
        format!(
            "friring could not bind the listener it would have the place dial, at '{socket}': {e}"
        )
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("friring could not poll its listener at '{socket}': {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    let observed = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let accepting = std::thread::spawn({
        let (observed, stop) = (Arc::clone(&observed), Arc::clone(&stop));
        move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    // Accepted and dropped at once. The measurement *is* the
                    // arrival: nothing is read from the connection, so nothing
                    // the place sends can influence the answer, and closing it
                    // is what lets the dialer stop waiting and exit.
                    Ok(_) => {
                        observed.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(DIAL_POLL);
                    }
                    Err(_) => break,
                }
            }
        }
    });

    let dialled = dial(&socket);
    if !observed.load(Ordering::Relaxed) {
        std::thread::sleep(DIAL_GRACE);
    }
    stop.store(true, Ordering::Relaxed);
    let _ = accepting.join();
    let _ = std::fs::remove_file(&path);
    Ok((
        dial_verdict(observed.load(Ordering::Relaxed), &dialled),
        socket,
    ))
}

/// The refusal for a place that dialled and was turned down.
///
/// The container engines' [`crate::sandbox::apple::plan::egress_refusal`],
/// which says the same thing about a VM whose kernel is not the host's — the
/// difference is only that this one had to be measured, because docker and
/// podman are on this kernel on a Linux host and in a Linux VM on a Mac or a
/// Windows box, and the engine's own name says nothing about which.
///
/// Only a host that can bind a listener can be told its socket did not carry, so
/// this goes with the probe.
#[cfg(unix)]
fn socket_does_not_carry(
    policy: &crate::session::SandboxPolicy,
    engine: ContainerEngine,
    socket: &str,
) -> String {
    format!(
        "network mode '{}' is enforced by friring's egress proxy outside the boundary, which a \
         place reaches over a bind-mounted unix socket — and this place cannot dial one. friring \
         bound a listener at '{socket}', in a directory the place mounts at that same path, and a \
         connect from inside it was refused: an AF_UNIX listener lives in the kernel that bound \
         it, so where {engine} runs its daemon in a Linux VM (Docker Desktop, podman machine, \
         colima) the mount carries the socket file across and nothing is listening on the far \
         side. Starting the place anyway would give it '--network none' and a relay dialling \
         nothing — no egress at all, under a profile that says otherwise. Run this profile on a \
         {engine} whose daemon is on this machine's kernel, or set its network to 'full' with no \
         denies here, or run it on seatbelt (which shares the host's network stack) or on bwrap",
        policy.network
    )
}

/// The refusal for a place friring could not get an answer out of.
///
/// Separate from [`socket_does_not_carry`] because the fix is: the boundary may
/// well carry a socket, and what failed is the asking. Refusing anyway is the
/// same fail-closed rule the rest of this feature applies — "not shown to work"
/// is not "works". `detail` is a whole clause, because what could not be asked
/// differs (a listener that would not bind names a path this one never had).
fn reach_unprovable(policy: &crate::session::SandboxPolicy, detail: &str) -> String {
    format!(
        "network mode '{}' is enforced by friring's egress proxy outside the boundary, which a \
         place reaches over a bind-mounted unix socket — and friring could not prove this place \
         can dial one: {detail}. A place that cannot be shown to reach the proxy is not started \
         believing it is filtered, so clear what that reason names — '{DIALER}' has to run inside \
         the place, which it already has to for the transport — or set this profile's network to \
         'full' with no denies here, or run it on seatbelt or on bwrap",
        policy.network
    )
}

/// A path the sandbox layer can name exactly, or a refusal.
///
/// Everything here becomes a mount source, and a lossy conversion would name a
/// different directory — the same rule the launch path applies to every
/// security-relevant path.
fn representable(
    path: &std::path::Path,
    what: &str,
    refuse: &dyn Fn(String) -> SandboxError,
) -> SandboxResult<String> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        refuse(format!(
            "{what} ('{}') is not valid UTF-8, and a mount built from an approximation of it \
             would name a different directory",
            path.display()
        ))
    })
}

/// The first non-empty line of an engine's stderr, which is where its own
/// actionable message is.
fn first_line(stderr: &str, fallback: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::backend::{PlaceLaunch, PlaceRelay};
    use crate::sandbox::probe::StubHost;
    use crate::session::{SandboxPath, SandboxPolicy};

    const PROGRAM: &str = "/usr/bin/podman";

    fn profile() -> SandboxProfile {
        SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")])
    }

    fn resolved(mutate: impl FnOnce(&mut SandboxProfile)) -> SandboxPolicy {
        let mut profile = profile();
        mutate(&mut profile);
        profile
            .resolve(SandboxBackendKind::Podman, "/home/u")
            .unwrap()
    }

    /// A Linux host with a working rootless podman and nothing else.
    fn host() -> StubHost {
        use crate::sandbox::probe::ProbeOutput;
        StubHost::new()
            .with_home("/home/u")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("podman")
            .with_command("id -u", ProbeOutput::success("1000\n"))
            .with_command("id -g", ProbeOutput::success("1000\n"))
            .with_command(
                &format!(
                    "{PROGRAM} info --format {}",
                    "{{.Version.Version}}|{{.Host.Security.Rootless}}"
                ),
                ProbeOutput::success("5.2.2|true\n"),
            )
    }

    fn backend(host: StubHost) -> ContainerBackend {
        ContainerBackend::new(ContainerEngine::Podman, Arc::new(host))
    }

    fn socket_endpoint() -> ProxyEndpoint {
        ProxyEndpoint::UnixSocket {
            host_path: "/data/pl/dev/abc/proxy.sock".to_string(),
            inside_path: "/data/pl/dev/abc/proxy.sock".to_string(),
        }
    }

    #[test]
    fn a_place_is_probed_where_it_would_run() {
        let backend = backend(host());
        assert!(backend.probe().is_available());
        assert_eq!(backend.kind(), SandboxBackendKind::Podman);
        let caps = backend.capabilities();
        assert_eq!(caps.shape, SandboxShape::Place);
        // The two capabilities only a place has, and the one it does not.
        assert!(caps.limits);
        assert!(caps.persistent);
        assert!(!caps.host_credentials);
        assert_eq!(caps.proxy_transport, ProxyTransport::UnixSocket);
    }

    /// The failure this refusal prevents: a place backend whose `wrap` returned
    /// the argv unchanged would launch the agent on the *host*, under a profile
    /// that says it is in a container.
    #[test]
    fn a_launch_composed_without_a_place_is_refused_rather_than_run() {
        let backend = backend(host());
        let policy = resolved(|p| p.network_mode = NetworkMode::None);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let err = backend.wrap(vec!["claude".into()], &launch).unwrap_err();
        assert!(matches!(err, SandboxError::Refused { .. }), "{err}");
        assert!(err.to_string().contains("without one"), "{err}");
    }

    #[test]
    fn the_in_place_command_starts_the_relay_before_the_agent() {
        let backend = backend(host());
        let policy = resolved(|_| {});
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_proxy(socket_endpoint())
            .with_place(PlaceLaunch {
                relay: Some(PlaceRelay {
                    program: "/usr/local/bin/friring-cli",
                    port: 8119,
                }),
            });
        let argv = backend
            .wrap(vec!["claude".into(), "--resume".into()], &launch)
            .unwrap();
        // The launcher's three positionals, then the agent's own argv.
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[4], "/usr/local/bin/friring-cli");
        assert_eq!(argv[5], "127.0.0.1:8119");
        assert_eq!(argv[6], "/data/pl/dev/abc/proxy.sock");
        assert_eq!(&argv[7..], ["claude", "--resume"]);
        // Nothing here names the engine: reaching the place is the transport's
        // job, and this command runs inside it.
        assert!(!argv.iter().any(|token| token.contains("podman")));
    }

    #[test]
    fn a_place_refuses_a_loopback_proxy_and_needs_no_relay_without_one() {
        let backend = backend(host());
        let policy = resolved(|_| {});
        let place = PlaceLaunch {
            relay: Some(PlaceRelay {
                program: "/usr/local/bin/friring-cli",
                port: 8118,
            }),
        };
        let loopback = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_proxy(ProxyEndpoint::Loopback { port: 8123 })
            .with_place(place);
        let err = backend.wrap(vec!["claude".into()], &loopback).unwrap_err();
        assert!(
            err.to_string().contains("no route to host loopback"),
            "{err}"
        );

        // A profile with no filtered egress needs no relay, and the agent's argv
        // crosses unchanged.
        let closed = resolved(|p| p.network_mode = NetworkMode::None);
        let launch = SandboxLaunch::new(&closed, "/home/u", "s1").with_place(place);
        assert_eq!(
            backend.wrap(vec!["claude".into()], &launch).unwrap(),
            ["claude"]
        );
    }

    /// Sessions of one profile share a place, and therefore its loopback: the
    /// fixed port a bubblewrap sandbox can use would collide here.
    #[test]
    fn every_session_in_a_place_gets_its_own_relay_port() {
        let backend = backend(host());
        let first = backend.relay_port("ctr", "s1").unwrap();
        let second = backend.relay_port("ctr", "s2").unwrap();
        assert_eq!(first, RELAY_PORT);
        assert_ne!(first, second);
        // Stable across a relaunch: the environment composed for that session
        // names this address.
        assert_eq!(backend.relay_port("ctr", "s1").unwrap(), first);
        // A different place has its own loopback, so it starts again at the base.
        assert_eq!(backend.relay_port("other", "s9").unwrap(), RELAY_PORT);
        // And a session that ends gives its port back.
        backend.release_relay_port("ctr", "s1");
        assert_eq!(backend.relay_port("ctr", "s3").unwrap(), first);
    }

    #[test]
    fn ports_are_assigned_from_the_lowest_free_one_and_run_out_loudly() {
        let mut taken = BTreeMap::new();
        assert_eq!(assign_port(&taken, "s1", 8118, 2), Some(8118));
        taken.insert("s1".to_string(), 8118);
        assert_eq!(assign_port(&taken, "s1", 8118, 2), Some(8118));
        assert_eq!(assign_port(&taken, "s2", 8118, 2), Some(8119));
        taken.insert("s2".to_string(), 8119);
        assert_eq!(assign_port(&taken, "s3", 8118, 2), None);
    }

    #[test]
    fn an_inspect_line_yields_the_labels_that_decide_ownership() {
        let parsed = Inspected::parse("abc123|running|1|dev|aaaa").unwrap();
        assert!(parsed.owned);
        assert!(parsed.running());
        assert_eq!(parsed.profile.as_deref(), Some("dev"));
        assert_eq!(parsed.spec.as_deref(), Some("aaaa"));

        // Somebody else's container: every label is missing, and a Go template
        // renders that as `<no value>`.
        let foreign = Inspected::parse("def456|running|<no value>|<no value>|<no value>").unwrap();
        assert!(!foreign.owned);
        assert!(foreign.profile.is_none());
        assert!(Inspected::parse("").is_none());
    }

    /// The transport addresses a place by the engine path this probe pinned —
    /// never a bare name, which an inherited `PATH` would get to choose.
    #[test]
    fn the_engine_the_transport_runs_is_the_one_the_probe_vetted() {
        let backend = backend(host());
        assert_eq!(backend.engine_program().unwrap(), PROGRAM);
        assert!(PROGRAM.starts_with('/'));
    }

    /// Everything an engine tells friring about a place ends up as the argument
    /// after `exec`'s flags, so nothing that could pass for a flag is accepted.
    #[test]
    fn only_a_reference_an_engine_could_have_minted_is_accepted() {
        assert!(valid_container_ref("friring-sbx-dev-0123456789ab"));
        assert!(valid_container_ref(&"a".repeat(64)));
        for bad in [
            "",
            "-i",
            "--rm",
            "/etc/passwd",
            "ctr with space",
            "ctr;rm -rf",
            "ctr\n--privileged",
        ] {
            assert!(!valid_container_ref(bad), "{bad:?} must be refused");
        }
        assert!(!valid_container_ref(&"a".repeat(129)));
    }

    /// The one test that talks to a real engine — and it creates nothing.
    ///
    /// It runs the two commands the lifecycle is built on (`info`, and the
    /// label-filtered `ps`) against whatever is installed, which is what catches
    /// an argv or a template this crate got wrong about the engines themselves;
    /// a unit test over a stub can only prove friring is consistent with its own
    /// idea of them. Nothing is pulled, built, created or removed: `ps` with
    /// friring's owner label answers with friring's own places, and on a machine
    /// that has never run one that is the empty list.
    ///
    /// Skipped with a reason where no engine is installed or none is answering,
    /// so it is a capability check rather than a test that fails on a laptop.
    #[test]
    fn a_real_engine_answers_the_two_queries_the_lifecycle_needs() {
        let host: Arc<dyn ProbeHost> = Arc::new(crate::sandbox::probe::LocalProbeHost);
        let available: Vec<ContainerBackend> = [ContainerEngine::Docker, ContainerEngine::Podman]
            .into_iter()
            .map(|engine| ContainerBackend::new(engine, Arc::clone(&host)))
            .filter(|backend| backend.probe().is_available())
            .collect();
        if available.is_empty() {
            eprintln!(
                "skipped: no container engine is installed and answering on this host, so there \
                 is nothing to ask"
            );
            return;
        }
        for backend in available {
            assert!(backend.engine_program().unwrap().starts_with('/'));
            let places = backend.live_places().unwrap_or_else(|error| {
                panic!("{} could not list places: {error}", backend.engine())
            });
            // Whatever came back is friring's own, and each is addressable.
            for place in places {
                assert!(place.owned, "the label filter returned {place:?}");
                assert!(valid_container_ref(&place.id));
            }
        }
    }

    /// The only function here that destroys anything, over the four shapes a
    /// planned id can turn out to have by the time it is reached.
    ///
    /// The load-bearing one is the second: the label is re-checked immediately
    /// before removal, so a container that lost friring's label (or an id that
    /// now names somebody else's container) is left alone and said out loud
    /// rather than removed.
    #[test]
    fn reaping_removes_only_what_is_still_friring_s_and_reports_the_rest() {
        use crate::sandbox::probe::ProbeOutput;

        let inspect =
            |id: &str| format!("{PROGRAM} inspect --type container --format {INSPECT_FORMAT} {id}");
        let host = host()
            // Ours, and it goes.
            .with_command(
                &inspect("mine"),
                ProbeOutput::success("mine|running|1|dev|aaaa\n"),
            )
            .with_command(
                &format!("{PROGRAM} rm --force mine"),
                ProbeOutput::success("mine\n"),
            )
            // Not ours any more: no owner label, so no `rm` is scripted — one
            // reaching the engine would fail this test with "No such file".
            .with_command(
                &inspect("theirs"),
                ProbeOutput::success("theirs|running|<no value>|<no value>|<no value>\n"),
            )
            // Gone between planning and reaping, which is the outcome the pass
            // wanted.
            .with_command(
                &inspect("vanished"),
                ProbeOutput::failure(1, "no such container\n"),
            )
            // Ours, and the engine will not let go of it.
            .with_command(
                &inspect("stuck"),
                ProbeOutput::success("stuck|running|1|dev|aaaa\n"),
            )
            .with_command(
                &format!("{PROGRAM} rm --force stuck"),
                ProbeOutput::failure(1, "container is in use\nand a second line nobody needs\n"),
            );

        let plan = GcPlan {
            remove: ["mine", "theirs", "vanished", "stuck"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..GcPlan::default()
        };
        let failures = backend(host).reap(&plan);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(
            failures[0].starts_with("theirs: ") && failures[0].contains("left alone"),
            "{failures:?}"
        );
        assert_eq!(failures[1], "stuck: container is in use");
    }

    /// bwrap refuses a launch whose writable roots enclose the program applying
    /// its boundary; a place is no different, and the engine CLI is that
    /// program — it is what asks the daemon for the isolation, and an engine at
    /// a user-writable prefix (`/usr/local/bin`, `/opt/homebrew/bin`) plus a
    /// profile granting that prefix is the sandbox choosing what the host runs
    /// next.
    #[test]
    fn a_profile_that_hands_over_the_engine_binary_is_refused_before_anything_is_built() {
        let backend = backend(host());
        let mut granted = profile();
        granted.name = "handover".to_string();
        granted.paths = vec![SandboxPath::workspace("/usr/bin")];
        let err = backend.plan_for(&granted).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
        assert!(text.contains(PROGRAM), "{text}");
        assert!(
            text.contains("replace the program that applies its own boundary"),
            "{text}"
        );

        // Read-only is a different grant: nothing there can replace anything,
        // so this one gets as far as the ordinary mount checks.
        let mut readable = profile();
        readable.name = "handover".to_string();
        readable.paths = vec![SandboxPath::read_only("/usr/bin")];
        let text = backend.plan_for(&readable).unwrap_err().to_string();
        assert!(!text.contains("replace the program"), "{text}");
        dirs::cleanup_place("handover");
    }

    /// A plan's mounts are decided long before the container is made — an image
    /// pull or build sits in between — and every source in it is a path some
    /// other place's agent may be writing the whole time. So they are checked
    /// again with nothing between the check and the spawn.
    ///
    /// The engine here is scripted to create the container happily: the only
    /// thing that refuses the second call is the re-check.
    #[cfg(unix)]
    #[test]
    fn a_source_that_became_a_symlink_after_planning_is_refused_at_create() {
        let base = dirs::test_temp_base("create-recheck");
        let repo = base.join("repo");
        let hooks = repo.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        // A place tree of this test's own: the directories are keyed by profile
        // name, and a sibling test cleaning up "dev" would take them away.
        let (place, home) = dirs::create_place_dirs("recheck").unwrap();
        let place = place.display().to_string();
        let home_dir = home.display().to_string();
        let policy = resolved(|profile| {
            profile.name = "recheck".to_string();
            profile.paths = vec![SandboxPath::workspace(repo.display().to_string())];
        });

        // Planned when everything was still what it claimed to be.
        let identity = |path: &str| Ok(path.to_string());
        let plan = plan_instance(PlanInput {
            policy: &policy,
            image: "friring/sandbox:1",
            place_dir: &place,
            home_dir: &home_dir,
            user: None,
            userns_keep_id: false,
            check: MountCheck {
                friring_db: None,
                home: None,
                exists: &|_| true,
                resolve: &identity,
            },
        })
        .unwrap();

        let mut scripted = host().with_command_prefix(
            &format!("{PROGRAM} run "),
            crate::sandbox::probe::ProbeOutput::success("abc123\n"),
        );
        for path in [&repo.display().to_string(), &hooks.display().to_string()] {
            scripted = scripted.with_path(path);
        }
        let backend = backend(scripted.with_path(&place).with_path(&home_dir));
        let refuse = |detail: String| SandboxError::Refused {
            profile: "recheck".to_string(),
            detail,
        };
        assert_eq!(backend.create(&plan, &refuse).unwrap(), "abc123");

        // And now a source changes meaning underneath the plan.
        std::fs::remove_dir(&hooks).unwrap();
        std::os::unix::fs::symlink(dirs::tmux_socket_root(), &hooks).unwrap();
        let err = backend.create(&plan, &refuse).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(err.contains(&hooks.display().to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&base);
        dirs::cleanup_place("recheck");
    }

    /// The bypass a check that only read the name on `PATH` would miss: an
    /// engine reached through a symlink is replaceable wherever it *lands*.
    #[cfg(unix)]
    #[test]
    fn an_engine_reached_through_a_symlink_is_judged_by_where_it_lands() {
        let base = dirs::test_temp_base("engine-symlink");
        let real = base.join("opt");
        let bin = base.join("usr/bin");
        for dir in [&real, &bin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let binary = real.join("podman");
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        let link = bin.join("podman");
        std::os::unix::fs::symlink(&binary, &link).unwrap();

        let granted = vec![real.display().to_string()];
        let program = link.display().to_string();
        // The name on `PATH` is nowhere near the granted root…
        assert!(dirs::program_in_writable_root(&granted, &program, None).is_none());
        // …and the binary it actually runs is inside it.
        let resolved = dirs::canonical(&program);
        let (root, found) = dirs::program_in_writable_root(&granted, &program, resolved.as_deref())
            .expect("refused");
        assert_eq!(root, granted[0]);
        assert_eq!(found, binary.display().to_string());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A data directory short enough that a socket path under it fits in
    /// `sun_path`, which the macOS unit-test temp directory does not.
    ///
    /// Only the tests that bind one need it, and those are the unix ones.
    #[cfg(unix)]
    fn short_data_dir(name: &str) -> crate::paths::TestPathGuard {
        crate::paths::TestPathGuard::new(
            std::path::Path::new("/tmp").join(format!("frc{}{name}", std::process::id())),
        )
    }

    /// Whether a place can reach a socket friring bound outside it is
    /// **measured**, and friring's own accept is the measurement.
    ///
    /// The three answers are three different launches: one that may be composed
    /// as filtered, one that may not because the boundary demonstrably does not
    /// carry a listener (a VM-backed engine — Docker Desktop, podman machine,
    /// colima — where the mount carries the socket file and nothing else), and
    /// one friring could not settle, which is refused for being unsettled
    /// rather than assumed to work.
    #[cfg(unix)]
    #[test]
    fn only_a_place_that_dials_friring_s_listener_may_be_composed_as_filtered() {
        use crate::sandbox::probe::ProbeOutput;
        let dir = std::env::temp_dir().join(format!("frr{}-reach", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let place = dir.display().to_string();

        // Something inside connected. Nothing is read from the connection, so
        // the dialer's own exit status and output cannot influence this.
        let (reach, socket) = probe_socket_reach(&place, &|path| {
            std::os::unix::net::UnixStream::connect(path).map_err(|e| e.to_string())?;
            Ok(ProbeOutput::failure(1, "protocol version mismatch\n"))
        })
        .unwrap();
        assert_eq!(reach, Reach::Crossed);
        // And the probe leaves nothing behind in a directory the place mounts.
        assert!(!std::path::Path::new(&socket).exists(), "{socket}");

        // The connect was turned down: the file crossed, the listener did not.
        let (reach, _) = probe_socket_reach(&place, &|path| {
            Ok(ProbeOutput::failure(1, format!("{DIAL_REFUSED} {path}\n")))
        })
        .unwrap();
        assert_eq!(reach, Reach::Refused);

        // Nobody dialled and nobody said why — which is not "it works".
        for dial in [
            &|_: &str| Ok(ProbeOutput::success("")) as Result<ProbeOutput, String>,
            &|_: &str| Err("exec: executable file not found in $PATH".to_string()),
        ] as [&dyn Fn(&str) -> Result<ProbeOutput, String>; 2]
        {
            let (reach, _) = probe_socket_reach(&place, dial).unwrap();
            assert_eq!(reach, Reach::Unprovable);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole point, at the level a user meets it: a filtered profile whose
    /// place cannot dial the proxy's socket is refused, with what is unreachable
    /// named — rather than started on `--network none` with a relay forwarding
    /// to nothing while the UI reports the allowlist applied.
    #[cfg(unix)]
    #[test]
    fn a_filtered_profile_whose_place_cannot_reach_the_proxy_is_refused() {
        use crate::sandbox::probe::ProbeOutput;
        let _paths = short_data_dir("reach");
        // A real directory of this test's own, resolved: a mount source whose
        // meaning a symlink could change is refused long before the probe, and
        // on macOS `/home` is one.
        let repo = dirs::test_temp_base("reach").join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let repo = repo.display().to_string();
        let filtered = SandboxProfile::new("reach", vec![SandboxPath::workspace(repo.clone())]);
        assert!(
            proxy_required(
                &filtered
                    .resolve(SandboxBackendKind::Podman, "/home/u")
                    .unwrap()
            ),
            "the default network mode is the filtered one this test is about"
        );
        let (place, home) = dirs::create_place_dirs("reach").unwrap();

        let scripted = host()
            .with_path(&repo)
            .with_path(&place.display().to_string())
            .with_path(&home.display().to_string())
            .with_command_prefix(
                &format!("{PROGRAM} image inspect"),
                ProbeOutput::success("[{}]\n"),
            )
            .with_command_prefix(&format!("{PROGRAM} run"), ProbeOutput::success("abc123\n"))
            .with_command(
                &format!("{PROGRAM} exec abc123 /bin/sh -c command -v {RELAY_BINARY}"),
                ProbeOutput::success("/usr/local/bin/friring-cli\n"),
            )
            // The dial: this place is a VM's, so the connect is refused.
            .with_command_prefix(
                &format!("{PROGRAM} exec abc123 {DIALER}"),
                ProbeOutput::failure(1, format!("{DIAL_REFUSED} /sock\n")),
            );
        let backend = backend(scripted);
        let err = backend.ensure_place(&filtered).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
        // Names the mode, what could not be dialled, and the way out.
        assert!(text.contains("cannot dial one"), "{text}");
        assert!(text.contains("/sandbox/pl/reach/"), "{text}");
        assert!(
            text.contains("AF_UNIX listener lives in the kernel"),
            "{text}"
        );
        assert!(text.contains("seatbelt"), "{text}");

        // A profile whose mode needs no proxy uses the very same place and is
        // never asked the question — the place is not torn down over it.
        let mut open = filtered.clone();
        open.network_mode = NetworkMode::None;
        backend.ensure_place(&open).expect("no proxy, no probe");
        dirs::cleanup_place("reach");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Once per place, not once per session: the answer is a property of a live
    /// container's kernel and mounts, and re-dialling on every launch would put
    /// an `exec` on the path of every relaunch of every session in it.
    #[cfg(unix)]
    #[test]
    fn a_place_already_shown_to_reach_the_proxy_is_not_dialled_again() {
        let _paths = short_data_dir("reach-cache");
        let backend = backend(host());
        let policy = resolved(|_| {});
        let refuse = |detail: String| SandboxError::Refused {
            profile: policy.profile.clone(),
            detail,
        };
        // Nothing is scripted to answer an `exec`, so a second dial could only
        // fail: reaching `Ok` proves none was made.
        backend
            .reached
            .lock()
            .unwrap()
            .insert("already-proven".to_string());
        backend
            .check_proxy_reachable("already-proven", &policy, &refuse)
            .expect("a place proven once is not asked again");
        assert!(backend
            .check_proxy_reachable("never-asked", &policy, &refuse)
            .is_err());
    }

    /// Nothing may be ensured on a host without the engine, and the reason is
    /// the probe's own sentence rather than a later, stranger failure.
    #[test]
    fn an_unavailable_engine_refuses_before_it_touches_anything() {
        let bare = StubHost::new()
            .with_command(
                "uname -s",
                crate::sandbox::probe::ProbeOutput::success("Linux\n"),
            )
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n");
        let backend = backend(bare);
        let err = backend.ensure(&profile()).unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable { .. }), "{err}");
        assert!(err.to_string().contains("not installed"), "{err}");
    }
}
