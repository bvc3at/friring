//! The `apple-container` place backend — Apple's `container` CLI.
//!
//! A **place** is an environment that outlives an individual command (ADR-26):
//! created once per profile, shared by every session that picks it, and reached
//! through a transport with tmux running *inside* it. This one is the strongest
//! boundary macOS offers, because Containerization.framework gives each
//! container a lightweight virtual machine of its own — a kernel that is not the
//! host's, OCI images, and virtiofs for the directories a profile grants.
//!
//! ```text
//! friring
//!   └─ tmux (host, control mode)
//!        └─ container exec -i <ctr> tmux …    ← the transport
//!             └─ tmux (in the place, in its VM)
//!                  └─ the agent
//! ```
//!
//! Everything about *what* a place is made of is shared with the container
//! engines — [`crate::sandbox::container::plan`] decides the mounts, the labels,
//! the synthetic home and the spec digest, and
//! [`crate::sandbox::container::gc`] decides what to reclaim. Only the command
//! lines differ ([`plan`]), because sharing the rules is what keeps a second
//! place backend from being a second set of escapes.
//!
//! Three things are this backend's own, and each is a refusal rather than an
//! approximation:
//!
//! - **No filtered egress.** friring's proxy sits outside the boundary and a
//!   place reaches it over a bind-mounted unix socket (ADR-27) — which does not
//!   cross a VM boundary, because an `AF_UNIX` listener lives in the kernel that
//!   bound it. So `allowlist`, and `full` carrying denies, are refused here
//!   rather than started believing they are filtered; so is `none`, whose
//!   promise friring cannot keep with this tool. See [`plan::egress_refusal`].
//! - **macOS 26 and Apple Silicon.** The VM is arm64 (an amd64 *image* runs
//!   under Rosetta inside it), and below macOS 26 the tool cannot create a
//!   network, so every place would share the one every other container on the
//!   Mac is on. Both are probe failures with the reason, never a silent skip.
//! - **No `--cap-drop`, `--security-opt` or `--user`.** Those harden a process
//!   sharing the host's kernel; here the guest kernel is its own, and virtiofs
//!   performs host-side access as the user running the VM, so a bind mount stays
//!   writable without friring naming a uid.
//!
//! Every command this file runs goes through the injected [`ProbeHost`], so no
//! test starts a virtual machine, pulls an image or touches the real tool.

pub mod cli;
pub mod plan;

use std::sync::{Arc, OnceLock};

use crate::sandbox::backend::{
    Argv, Availability, Caps, Egress, InnerSandboxVerdict, ProxyTransport, SandboxBackend,
    SandboxError, SandboxLaunch, SandboxResult,
};
use crate::sandbox::container::gc::{GcPlan, LiveContainer};
use crate::sandbox::container::image::{self, ImageSource, DEFAULT_IMAGE};
use crate::sandbox::container::plan::{
    plan_instance, InstancePlan, MountCheck, PlanInput, LABEL_OWNER, LABEL_PROFILE, LABEL_SPEC,
};
use crate::sandbox::container::{EnsuredPlace, INSTANCE_STATE_RUNNING};
use crate::sandbox::dirs;
use crate::sandbox::launcher::SHELL;
use crate::sandbox::place::{valid_container_ref, PlaceBackend};
use crate::sandbox::probe::{ProbeHost, ProbeOutput};
use crate::session::{
    NetworkMode, ReadScope, SandboxBackendKind, SandboxInstance, SandboxPolicy, SandboxProfile,
    SandboxShape,
};

pub use cli::AppleDetails;

/// How to build the default image with this tool, quoted verbatim in the
/// refusal that needs it.
///
/// friring publishes no image (see [`crate::sandbox::container::image`]), and
/// this tool builds with its own BuildKit instance, which has to be running —
/// so the builder is part of the command rather than a second thing to discover.
pub const DEFAULT_IMAGE_BUILD: &str = "build it once: container builder start && container build \
                                       --tag friring/sandbox:1 --file \
                                       packaging/sandbox/Containerfile packaging/sandbox";

/// Asks a place where a program is, with the name as a positional parameter.
///
/// `command -v` is POSIX and answers with an absolute path for anything on
/// `PATH`. The name arrives as `"$1"` rather than spliced into the script
/// because it comes from the user's `agents.toml`: a shell command built by
/// concatenation is one an unusual agent name gets to rewrite. Mirrors the
/// lookup [`crate::sandbox::container::image`] does through an engine.
const LOOKUP: &str = "command -v \"$1\"\n";

/// `$0` for that shell, so a `ps` inside the place says what the process is.
const LOOKUP_NAME: &str = "friring-agent-lookup";

/// Apple's `container` CLI as a place backend.
pub struct AppleContainerBackend {
    host: Arc<dyn ProbeHost>,
    details: OnceLock<AppleDetails>,
}

impl AppleContainerBackend {
    pub fn new(host: Arc<dyn ProbeHost>) -> Self {
        Self {
            host,
            details: OnceLock::new(),
        }
    }

    /// The probe's full answer, cached with the availability.
    pub fn details(&self) -> &AppleDetails {
        self.details.get_or_init(|| cli::probe(self.host.as_ref()))
    }

    /// The vetted absolute path of the CLI, or the probe's own reason.
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

    /// The absolute CLI the sandbox transport addresses a place with.
    ///
    /// `<tool> exec -i <container> tmux …`, and the tool has to be the path this
    /// probe pinned rather than a name re-resolved through whatever `PATH`
    /// friring inherited — choosing what starts the boundary is choosing what
    /// the boundary is.
    ///
    /// # Errors
    ///
    /// The backend is unavailable; the message is the probe's own.
    pub fn engine_program(&self) -> SandboxResult<&str> {
        self.program()
    }

    /// Make sure `profile`'s place exists and is running, and answer with
    /// everything a launch into it needs.
    ///
    /// Idempotent, and cheap when the place is already there. Reuses a healthy
    /// container, starts a stopped one, and rebuilds one that will not start —
    /// leaving the old one behind for
    /// [`gc_plan`](crate::sandbox::container::gc::gc_plan) rather than removing
    /// it here, because a container the caller has not yet recorded is one
    /// nothing could find again if this call died halfway.
    ///
    /// # Errors
    ///
    /// The tool is unavailable, the profile cannot be resolved, its network mode
    /// cannot be honoured here ([`plan::egress_refusal`]), a mount cannot be
    /// honoured, friring's own network cannot be created, the image is missing
    /// and cannot be built, or the container will not start.
    pub fn ensure_place(&self, profile: &SandboxProfile) -> SandboxResult<EnsuredPlace> {
        let refuse = |detail: String| SandboxError::Refused {
            profile: profile.name.clone(),
            detail,
        };
        let (plan, source, policy) = self.plan_for(profile)?;
        let network = self.ensure_network(&refuse)?;
        self.ensure_image(&policy.profile, &source)?;
        let id = self.start_or_create(&plan, network, &refuse)?;

        Ok(EnsuredPlace {
            instance: SandboxInstance {
                profile: policy.profile.clone(),
                engine: self.kind(),
                external_id: id,
                state: INSTANCE_STATE_RUNNING.to_string(),
            },
            home_dir: plan.home_dir.clone(),
            // Never a relay: one exists to reach the egress proxy over a
            // bind-mounted socket, and that socket does not cross this boundary
            // (see the module docs). Every mode that would need one is refused
            // in `plan_for`, before anything is created.
            relay_program: None,
        })
    }

    /// What `profile`'s place would be, without creating or starting one.
    ///
    /// The half of [`ensure_place`](Self::ensure_place) that only decides. It
    /// does mint the place's own directories, because a bind mount's source has
    /// to exist before it can be planned, but those are friring's own, `0700`,
    /// and idempotent.
    ///
    /// # Errors
    ///
    /// As [`ensure_place`](Self::ensure_place), short of the tool commands.
    fn plan_for(
        &self,
        profile: &SandboxProfile,
    ) -> SandboxResult<(InstancePlan, ImageSource, SandboxPolicy)> {
        // Before anything is created on disk: an unavailable tool must fail with
        // the probe's own actionable sentence, not with whatever the first
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
        // The three refusals that are this backend's own, all before anything is
        // minted: a boundary friring cannot apply is not one to build a place
        // for.
        if let Some(detail) = plan::egress_refusal(&policy) {
            return Err(refuse(detail));
        }
        self.check_limits(&policy, &refuse)?;
        self.check_program_containment(&policy, &refuse)?;

        let (place_dir, home_dir) = dirs::create_place_dirs(&policy.profile)?;
        let (place_dir, home_dir) = (
            representable(&place_dir, "the place directory", &refuse)?,
            representable(&home_dir, "the place's home directory", &refuse)?,
        );
        let source = image::resolve(&policy)?;
        let exists = |path: &str| self.host.path_exists(path);
        let plan = plan_instance(PlanInput {
            policy: &policy,
            image: source.reference(),
            place_dir: &place_dir,
            home_dir: &home_dir,
            // Neither reaches this CLI: the guest kernel is not the host's and
            // virtiofs does host-side access as the user running the VM, so a
            // bind mount is writable without friring naming a uid (see [`plan`]).
            user: None,
            userns_keep_id: false,
            check: MountCheck {
                // No launch here, so no other machine's data directory to
                // protect: `protected_data_dirs` still covers this host's own,
                // which is the one a place on it could reach.
                friring_db: None,
                home: Some(&home),
                exists: &exists,
                resolve: &dirs::place_mount_source,
            },
        })?;
        Ok((plan, source, policy))
    }

    /// The spec digest `profile` resolves to **right now** — what decides
    /// whether a running place still describes it.
    ///
    /// Garbage collection's input, and `None` is deliberately not an answer of
    /// "no place should exist": a profile that cannot be planned has *no
    /// opinion*, and reclaiming on the strength of a planning failure would take
    /// a running agent's place away over a missing external disk.
    pub fn current_spec(&self, profile: &SandboxProfile) -> Option<String> {
        self.plan_for(profile).ok().map(|(plan, _, _)| plan.spec)
    }

    /// Refuse a limit this build of the tool cannot be told about.
    ///
    /// `docs/SANDBOX.md` §Failure modes: an unavailable capability is reported
    /// as unavailable rather than accepted and ignored. A profile that says
    /// "4 GB" and gets whatever the tool defaults to is a boundary nobody can
    /// reason about.
    fn check_limits(
        &self,
        policy: &SandboxPolicy,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        let details = self.details();
        for (value, flag, what) in [
            (policy.memory_mb.is_some(), "memory", "a memory limit"),
            (policy.cpus.is_some(), "cpus", "a CPU limit"),
        ] {
            if value && !details.documents(flag) {
                return Err(refuse(format!(
                    "this profile sets {what}, and this build of Apple's container CLI does not \
                     document '--{flag}' on 'container run' — friring will not start a place \
                     whose limit it could not pass"
                )));
            }
        }
        Ok(())
    }

    /// Refuse a profile that hands the sandbox the tool's own binary.
    ///
    /// The probe vetted the CLI against the *host* (see
    /// [`dirs::rewritable_root`]); this profile decides what the agent can
    /// write, and one granting the directory the tool lives in grants the
    /// program that asks for the isolation. The rule and the sentence mirror
    /// [`BwrapBackend::wrap`](crate::sandbox::bwrap::BwrapBackend) and the
    /// container engines, which refuse the same shape for the same reason.
    fn check_program_containment(
        &self,
        policy: &SandboxPolicy,
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
            "the read-write path '{root}' contains Apple's container CLI itself ({named}), so the \
             sandbox could replace the program that applies its own boundary"
        )))
    }

    /// Make sure friring's own network exists, and answer with its name.
    ///
    /// Every place is attached to it rather than to the network every other
    /// container on this Mac shares (see [`plan::NETWORK`]). Checked on every
    /// ensure rather than cached, so a network the user deleted is created again
    /// instead of failing the next `run` with the tool's own message.
    ///
    /// # Errors
    ///
    /// The network is absent and cannot be created.
    fn ensure_network(
        &self,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<&'static str> {
        if self.network_exists() {
            return Ok(plan::NETWORK);
        }
        let created = self
            .tool_run(&["network", "create", plan::NETWORK])
            .map(|output| output.ok())
            .unwrap_or(false);
        // A create that failed because another friring won the race is a create
        // that succeeded, so the answer is re-read rather than trusted.
        if created || self.network_exists() {
            return Ok(plan::NETWORK);
        }
        Err(refuse(format!(
            "friring's own container network '{}' is not there and could not be created, and \
             friring will not put a place on the network every other container on this Mac shares",
            plan::NETWORK
        )))
    }

    /// Whether `container network ls` lists friring's network.
    fn network_exists(&self) -> bool {
        self.tool_run(&["network", "ls"])
            .ok()
            .filter(ProbeOutput::ok)
            .is_some_and(|output| {
                output
                    .stdout
                    .lines()
                    .any(|line| line.split_whitespace().any(|field| field == plan::NETWORK))
            })
    }

    /// Make sure the image is there, building or pulling it when that is what
    /// its source allows.
    ///
    /// An amd64 image is not refused: it runs under Rosetta inside the guest.
    fn ensure_image(&self, profile: &str, source: &ImageSource) -> SandboxResult<()> {
        if self
            .tool_run(&["images", "inspect", source.reference()])
            .is_ok_and(|output| output.ok())
        {
            return Ok(());
        }
        let missing = || self.missing_image(profile, source);
        match source {
            // friring publishes no image, so a missing default is a refusal with
            // the build command rather than a pull of whatever a registry
            // resolves that name to.
            ImageSource::Default => Err(missing()),
            ImageSource::Named(reference) => match self.tool_run(&["images", "pull", reference]) {
                Ok(output) if output.ok() => Ok(()),
                _ => Err(missing()),
            },
            ImageSource::Built { tag, containerfile } => {
                let context = image::build_context(containerfile);
                match self.tool_run(&["build", "--tag", tag, "--file", containerfile, context]) {
                    Ok(output) if output.ok() => Ok(()),
                    _ => Err(missing()),
                }
            }
        }
    }

    /// The sentence for an image that is missing and cannot be fetched.
    ///
    /// The container engines' own wording, with this tool's commands: a build
    /// here needs its BuildKit instance running, which is one more thing to be
    /// told about rather than to discover.
    fn missing_image(&self, profile: &str, source: &ImageSource) -> SandboxError {
        let detail = match source {
            ImageSource::Default => format!(
                "the default sandbox image '{DEFAULT_IMAGE}' is not on this host. friring does \
                 not publish it — {DEFAULT_IMAGE_BUILD}"
            ),
            ImageSource::Named(reference) => format!(
                "the image '{reference}' could not be found or pulled by Apple's container CLI"
            ),
            ImageSource::Built { tag, containerfile } => format!(
                "the image '{tag}' could not be built from '{containerfile}' — a build needs \
                 Apple's own builder running: container builder start"
            ),
        };
        SandboxError::Refused {
            profile: profile.to_string(),
            detail,
        }
    }

    /// Adopt the place this plan names, or build it. Answers with its id.
    fn start_or_create(
        &self,
        plan: &InstancePlan,
        network: &str,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        let Some(existing) = self.inspect(&plan.name) else {
            return self.create(plan, network, refuse);
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
            .tool_run(&["start", &existing.id])
            .is_ok_and(|output| output.ok())
        {
            return Ok(existing.id);
        }
        // It will not start — a place whose VM went with a host reboot, or one
        // wedged. Take it away by name (the name is what the replacement needs)
        // and build again. `stop` first because a delete of a running container
        // is refused, and best effort because either may already be true.
        let _ = self.tool_run(&["stop", &existing.id]);
        let _ = self.tool_run(&["delete", &existing.id]);
        self.create(plan, network, refuse)
    }

    fn create(
        &self,
        plan: &InstancePlan,
        network: &str,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        let program = self.program()?;
        // The last gate before a VM exists, and the one that cannot be reasoned
        // around: this renderer puts every place on friring's own network, so a
        // plan asking for *no* network would be started with one. `plan_for`
        // refuses that profile long before here (`plan::egress_refusal`), and
        // this is what makes the argv renderer unable to swallow it silently if
        // some later caller composes a plan another way.
        if !plan::network_is_open(plan) {
            return Err(refuse(
                "this place asks for a network friring cannot give it here, and every place \
                 Apple's container CLI starts is on a network — so it is refused rather than \
                 started with one the profile did not ask for"
                    .to_string(),
            ));
        }
        // The mounts were decided when the plan was built, and that can be
        // minutes ago: an image pull or a build sits between the two, and every
        // source in the plan is a path some *other* place's agent may be writing
        // the whole time. So they are re-checked here, with nothing between the
        // check and the spawn but composing the argv — see [`MountCheck::check`]
        // for the window that remains.
        let home = self.host.home();
        let exists = |path: &str| self.host.path_exists(path);
        MountCheck {
            friring_db: None,
            home: home.as_deref(),
            exists: &exists,
            resolve: &dirs::place_mount_source,
        }
        .check(&plan.mounts, refuse)?;
        let argv = plan::create_argv(program, plan, network);
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let output = self.host.run(program, &args).map_err(|detail| {
            refuse(format!("Apple's container CLI could not be run: {detail}"))
        })?;
        if !output.ok() {
            return Err(refuse(format!(
                "Apple's container CLI could not create the sandbox container: {}",
                cli::first_line(&output.stderr, "the tool gave no reason")
            )));
        }
        // The tool is asked what it made rather than believed about what it
        // printed: friring named this container, so the name is the handle, and
        // one round trip confirms both the id it goes by and that friring's
        // owner label really landed on it — every later lookup and every removal
        // depends on that label being there.
        let made = self.inspect(&plan.name).ok_or_else(|| {
            refuse(format!(
                "Apple's container CLI did not describe the container '{}' it had just created, \
                 so there is nothing to record or to reach",
                plan.name
            ))
        })?;
        if !made.owned {
            return Err(refuse(format!(
                "the container '{}' was created without friring's own label, and friring only \
                 ever reuses or removes what it can prove it created",
                plan.name
            )));
        }
        if !valid_container_ref(&made.id) {
            return Err(refuse(format!(
                "Apple's container CLI answered with '{}', which is not a container reference \
                 friring can put on a command line",
                made.id
            )));
        }
        Ok(made.id)
    }

    /// Refuse this launch unless the place carries the agent it is about to run.
    ///
    /// A place runs the agent inside itself, so a missing binary is a pane that
    /// dies the instant it opens — taking with it the sign-in that
    /// `volume-login` does *in that pane*. Asked per launch rather than per
    /// place, because a place is shared by every session of its profile and
    /// those sessions need not run the same agent.
    ///
    /// # Errors
    ///
    /// The place has no such program on its `PATH`, or the tool would not
    /// answer — each with what to do about it.
    pub fn ensure_agent_program(
        &self,
        policy: &SandboxPolicy,
        place: &EnsuredPlace,
        program: &str,
    ) -> SandboxResult<()> {
        let refuse = |detail: String| SandboxError::Refused {
            profile: policy.profile.clone(),
            detail,
        };
        let source = image::resolve(policy)?;
        let unanswered = |detail: &str| {
            refuse(format!(
                "friring could not ask this profile's place whether it carries '{program}', so it \
                 will not start a session that may open on a dead pane: {detail}"
            ))
        };
        let asked = self.tool_run(&[
            "exec",
            &place.instance.external_id,
            SHELL,
            "-c",
            LOOKUP,
            LOOKUP_NAME,
            program,
        ]);
        match asked {
            // An absolute path is the whole question: `command -v` names a shell
            // builtin or a relative match without one, and neither is a program
            // tmux can run in there.
            Ok(output) if output.ok() && output.trimmed().starts_with('/') => Ok(()),
            Ok(output) => {
                // `command -v` says nothing at all when it finds nothing, so
                // anything on stderr came from the tool or from the image — an
                // `exec` that could not run rather than an agent that is not
                // installed. Reporting one as the other would send the user to
                // install something they already have.
                let said = cli::first_line(&output.stderr, "");
                if said.is_empty() {
                    Err(refuse(self.missing_agent(&source, place, program)))
                } else {
                    Err(unanswered(&said))
                }
            }
            Err(detail) => Err(unanswered(&detail)),
        }
    }

    /// What to do about a place that does not carry `program`.
    ///
    /// The install lands in the profile's synthetic home, which is friring's own
    /// directory on the host and outlives every container the profile rebuilds
    /// (ADR-28) — so one install and one sign-in per profile. It is done in a
    /// throwaway container of the same image, because the place's own network is
    /// whatever the profile granted it.
    fn missing_agent(&self, source: &ImageSource, place: &EnsuredPlace, program: &str) -> String {
        let image = source.reference();
        let tool = self.program().unwrap_or(cli::PROGRAM);
        // Only the default image is known to carry node, npm and a `PATH` that
        // reaches the profile's home, so only there can the whole command be
        // written out; anywhere else the shell is the honest tail.
        let (install, tail) = match source {
            ImageSource::Default => (
                format!("npm install -g <the package that provides '{program}'>"),
                String::new(),
            ),
            _ => (
                SHELL.to_string(),
                " — then run the agent's own installer in the shell that opens".to_string(),
            ),
        };
        // A command name with a separator in it is a host path, which is worth
        // saying out loud: a place mounts what the profile granted and nothing
        // else, so `/opt/homebrew/bin/claude` is not merely absent, it can never
        // be there.
        let named = if program.contains('/') {
            format!(
                " '{program}' is a path on the host, and a place has none of the host's \
                 filesystem it did not mount — name the agent by a plain command in agents.toml \
                 and put that command in the place."
            )
        } else {
            String::new()
        };
        format!(
            "the place for this profile has no '{program}' on PATH, and it runs the agent inside \
             itself, so this session would open on a pane that dies at once.{named} Install it \
             once into the profile's own home, which outlives every container it rebuilds: {tool} \
             run --rm -i -t --mount type=bind,source={},target={} {image} {install}{tail}",
            place.home_dir,
            crate::sandbox::container::CONTAINER_HOME,
        )
    }

    /// What the tool says about one container, or `None` when it has none.
    fn inspect(&self, name_or_id: &str) -> Option<Inspected> {
        let output = self
            .tool_run(&["inspect", name_or_id])
            .ok()
            .filter(ProbeOutput::ok)?;
        parse_containers(&output.stdout).into_iter().next()
    }

    /// Every container the tool reports, as garbage collection sees them.
    ///
    /// This CLI has no label filter of its own, so the filtering is friring's:
    /// each entry carries [`LiveContainer::owned`] read from friring's own
    /// label, and [`gc_plan`](crate::sandbox::container::gc::gc_plan) never
    /// names one whose `owned` is false.
    ///
    /// # Errors
    ///
    /// The tool is unavailable or would not list its containers.
    pub fn live_places(&self) -> SandboxResult<Vec<LiveContainer>> {
        let listed = self
            .tool_run(&["ls", "--all", "--format", "json"])
            .map_err(|reason| SandboxError::Unavailable {
                backend: self.kind(),
                reason,
            })?;
        if !listed.ok() {
            return Err(SandboxError::Unavailable {
                backend: self.kind(),
                reason: cli::first_line(&listed.stderr, "the tool would not list its containers"),
            });
        }
        Ok(parse_containers(&listed.stdout)
            .into_iter()
            .map(Inspected::into_live)
            .collect())
    }

    /// Carry out a [`GcPlan`]'s removals, answering with the ones that failed.
    ///
    /// Best effort and non-fatal: a container that will not go is one the next
    /// pass tries again. Each id is checked against friring's own label one
    /// final time, because this is the only function here that destroys
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
            // A running container cannot be deleted, and a stopped one cannot be
            // stopped: the stop is best effort and the delete is the verdict.
            let _ = self.tool_run(&["stop", id]);
            match self.tool_run(&["delete", id]) {
                Ok(output) if output.ok() => {}
                Ok(output) => failures.push(format!(
                    "{id}: {}",
                    cli::first_line(&output.stderr, "the tool gave no reason")
                )),
                Err(detail) => failures.push(format!("{id}: {detail}")),
            }
        }
        failures
    }

    /// Run the tool with these arguments, resolving the program once.
    fn tool_run(&self, args: &[&str]) -> Result<ProbeOutput, String> {
        let program = self.program().map_err(|error| error.to_string())?;
        self.host.run(program, args)
    }
}

/// The same lifecycle seam the container engines implement
/// (`crate::sandbox::place`), so a caller names one interface rather than one
/// accessor per tool.
///
/// [`relay_port`](PlaceBackend::relay_port) is left at its default refusal and
/// [`release_relay_ports`](PlaceBackend::release_relay_ports) at its default
/// no-op: this backend refuses every filtered network mode when the place is
/// ensured (see [`plan::egress_refusal`]), so nothing here ever composes a
/// relay to hand a port to.
impl PlaceBackend for AppleContainerBackend {
    fn ensure_place(&self, profile: &SandboxProfile) -> SandboxResult<EnsuredPlace> {
        AppleContainerBackend::ensure_place(self, profile)
    }

    fn ensure_agent_program(
        &self,
        policy: &SandboxPolicy,
        place: &EnsuredPlace,
        program: &str,
    ) -> SandboxResult<()> {
        AppleContainerBackend::ensure_agent_program(self, policy, place, program)
    }

    fn current_spec(&self, profile: &SandboxProfile) -> Option<String> {
        AppleContainerBackend::current_spec(self, profile)
    }

    fn engine_program(&self) -> SandboxResult<&str> {
        AppleContainerBackend::engine_program(self)
    }

    fn live_places(&self) -> SandboxResult<Vec<LiveContainer>> {
        AppleContainerBackend::live_places(self)
    }

    fn reap(&self, plan: &GcPlan) -> Vec<String> {
        AppleContainerBackend::reap(self, plan)
    }
}

impl SandboxBackend for AppleContainerBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::AppleContainer
    }

    fn probe(&self) -> Availability {
        self.details().availability.clone()
    }

    fn capabilities(&self) -> Caps {
        Caps {
            shape: SandboxShape::Place,
            // The VM has a size, and the CLI takes `--memory` / `--cpus`. A
            // build of it that does not is refused per profile rather than
            // claimed here (see `check_limits`).
            limits: true,
            // The one mode this backend can honour. Everything else is enforced
            // by a proxy outside the boundary that a place here cannot reach, or
            // by a route friring cannot prove it cut — see
            // [`plan::egress_refusal`], which is where a profile asking for one
            // of them is turned down.
            network_modes: &[NetworkMode::Full],
            // A place has no host filesystem to read. `host-minus-secrets` is a
            // statement about the machine friring runs on, and none of it is in
            // here unless the profile lists it.
            read_scopes: &[ReadScope::Workspace],
            persistent: true,
            // The macOS keychain does not cross a VM boundary (ADR-28), which is
            // the whole reason this backend gets a token or a per-profile login
            // instead of the host's own store.
            host_credentials: false,
            inner_agent_sandbox: InnerSandboxVerdict::Redundant,
            // Declared for the shape a place would use if it could reach the
            // proxy at all. Nothing here ever gets one: a launch whose mode
            // needs a proxy is refused before one is bound.
            proxy_transport: ProxyTransport::UnixSocket,
        }
    }

    /// Compose the command that runs **inside** the place.
    ///
    /// The place backend's half of the argv seam. Nothing here names the tool —
    /// reaching the place is the transport's job — so what comes out is what
    /// tmux *in* the container is asked to run, which for this backend is the
    /// agent's own argv: there is no relay to start beside it, because the
    /// socket a relay would forward to does not cross a VM boundary.
    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv> {
        if launch.policy.backend != self.kind() {
            return Err(SandboxError::Unsupported {
                backend: self.kind(),
                detail: format!(
                    "policy was resolved for '{}'; resolve it for apple-container first",
                    launch.policy.backend
                ),
            });
        }
        launch.validate()?;
        let refuse = |detail: String| SandboxError::Refused {
            profile: launch.policy.profile.clone(),
            detail,
        };
        // A launch with no place is a launch that never called `ensure`, and
        // returning the argv unchanged would run the agent on the *host* under a
        // profile that says it is in a VM. Refusing routes it through the
        // profile's own `allow_unsandboxed_fallback` switch instead, like every
        // other boundary friring will not grant.
        let place = launch.place.ok_or_else(|| {
            refuse(
                "this profile runs in an apple-container place, whose command is composed for the \
                 inside of the container; this launch was built without one"
                    .to_string(),
            )
        })?;
        if place.relay.is_some() {
            return Err(refuse(
                "this launch carries an egress relay, and a place here has nothing for one to \
                 forward to: friring's proxy listens on a unix socket that does not cross a VM \
                 boundary"
                    .to_string(),
            ));
        }
        match launch.egress() {
            Egress::Open => Ok(argv),
            // Both halves of the refusal are the policy's, so the sentence is
            // the same one `ensure_place` would have given — a launch that got
            // this far with a filtered mode is one that skipped the ensure.
            _ => Err(refuse(plan::egress_refusal(launch.policy).unwrap_or_else(
                || "this profile's network mode cannot be honoured here".to_string(),
            ))),
        }
    }

    fn ensure(&self, profile: &SandboxProfile) -> SandboxResult<SandboxInstance> {
        Ok(self.ensure_place(profile)?.instance)
    }
}

/// One container as the tool described it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Inspected {
    id: String,
    status: String,
    owned: bool,
    profile: Option<String>,
    spec: Option<String>,
}

impl Inspected {
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

/// Read the containers out of whatever JSON the tool printed.
///
/// Deliberately tolerant about *shape* and strict about *meaning*. This CLI
/// renders no Go templates the way the container engines do, so friring reads
/// its JSON — and that document is the tool's, not friring's, so the id and the
/// labels are looked for where either a flat or a nested rendering would put
/// them. What tolerance never does is invent ownership: a container with no
/// `dev.friring.sandbox` label is not friring's, so it is never reused, never
/// removed and never adopted.
fn parse_containers(raw: &str) -> Vec<Inspected> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    match value {
        serde_json::Value::Array(items) => items.iter().filter_map(parse_container).collect(),
        other => parse_container(&other).into_iter().collect(),
    }
}

/// One container out of one JSON object.
fn parse_container(value: &serde_json::Value) -> Option<Inspected> {
    let configuration = value.get("configuration");
    let field = |name: &str| {
        configuration
            .and_then(|c| c.get(name))
            .or_else(|| value.get(name))
    };
    let id = field("id")?.as_str()?.trim().to_string();
    if !valid_container_ref(&id) {
        // Nothing friring could address is spelled like this, and everything it
        // learns here reaches a command line as the argument after `exec`'s own
        // flags.
        return None;
    }
    let status = field("status")
        .or_else(|| field("state"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let label = |name: &str| {
        field("labels")?
            .get(name)?
            .as_str()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    Some(Inspected {
        id,
        status,
        owned: label(LABEL_OWNER).as_deref() == Some("1"),
        profile: label(LABEL_PROFILE),
        spec: label(LABEL_SPEC),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::backend::{PlaceLaunch, PlaceRelay, ProxyEndpoint};
    use crate::sandbox::probe::StubHost;
    use crate::session::SandboxPath;

    const PROGRAM: &str = "/usr/bin/container";
    // Apple's tool only runs on macOS, and the cases that build one of its places
    // name this host's own paths — unix-only in both directions.
    #[cfg(unix)]
    const IMAGE: &str = "friring/sandbox:1";

    fn profile(name: &str) -> SandboxProfile {
        let mut profile = SandboxProfile::new(name, vec![SandboxPath::workspace("~/dev/app")]);
        // The default mode is `allowlist`, which this backend cannot enforce;
        // every test that is not *about* that starts from the one it can.
        profile.network_mode = NetworkMode::Full;
        profile
    }

    fn resolved(mutate: impl FnOnce(&mut SandboxProfile)) -> SandboxPolicy {
        let mut profile = profile("dev");
        mutate(&mut profile);
        profile
            .resolve(SandboxBackendKind::AppleContainer, "/Users/u")
            .unwrap()
    }

    /// One container as the tool's JSON describes it.
    fn container_json(id: &str, status: &str, labels: &str) -> String {
        format!(
            "{{\"status\":\"{status}\",\"configuration\":{{\"id\":\"{id}\",\"labels\":{{{labels}}}}}}}"
        )
    }

    fn friring_labels(profile: &str, spec: &str) -> String {
        format!(
            "\"{LABEL_OWNER}\":\"1\",\"{LABEL_PROFILE}\":\"{profile}\",\"{LABEL_SPEC}\":\"{spec}\""
        )
    }

    /// Everything `container run --help` has to document for this backend to be
    /// available, plus the two limit flags.
    const RUN_HELP: &str = "-d, --detach --name <name> -l, --label <label> --mount <mount> \
                            --network <network> -m, --memory <memory> -c, --cpus <cpus>";

    /// An Apple Silicon Mac on macOS 26 with the tool installed and answering.
    fn host() -> StubHost {
        host_answering(RUN_HELP)
    }

    /// The same, with the `run --help` a test wants.
    ///
    /// Built from scratch rather than overridden: the stub answers with the
    /// *first* command it was given, so a second registration of that line would
    /// never be reached.
    fn host_answering(run_help: &str) -> StubHost {
        StubHost::macos(26, true)
            .with_binary("container")
            .with_command(
                &format!("{PROGRAM} --version"),
                ProbeOutput::success("container CLI version 0.5.0\n"),
            )
            .with_command(
                &format!("{PROGRAM} system status"),
                ProbeOutput::success("apiserver is running\n"),
            )
            .with_command(
                &format!("{PROGRAM} run --help"),
                ProbeOutput::success(run_help),
            )
            .with_command(
                &format!("{PROGRAM} exec --help"),
                ProbeOutput::success("-i, --interactive\n"),
            )
    }

    fn backend(host: StubHost) -> AppleContainerBackend {
        AppleContainerBackend::new(Arc::new(host))
    }

    /// A host that can see everything a place of `profile` is planned from: the
    /// workspace, and friring's own two directories.
    ///
    /// Those two are minted here rather than left to `plan_for`, because the
    /// stub answers "does this exist?" from a list and a place's own mounts are
    /// checked like every other source.
    #[cfg(unix)]
    fn host_for(profile: &str) -> StubHost {
        let (place, home) = dirs::create_place_dirs(profile).unwrap();
        host()
            .with_path("/Users/u/dev/app")
            .with_path(&place.display().to_string())
            .with_path(&home.display().to_string())
    }

    #[test]
    fn a_place_is_probed_where_it_would_run() {
        let backend = backend(host());
        assert!(backend.probe().is_available());
        assert_eq!(backend.kind(), SandboxBackendKind::AppleContainer);
        let caps = backend.capabilities();
        assert_eq!(caps.shape, SandboxShape::Place);
        assert!(caps.limits);
        assert!(caps.persistent);
        assert!(!caps.host_credentials);
        // The honest half: one network mode, because the proxy that enforces the
        // others cannot be reached from inside a VM.
        assert_eq!(caps.network_modes, [NetworkMode::Full]);
    }

    /// The failure this refusal prevents: a place backend whose `wrap` returned
    /// the argv unchanged would launch the agent on the *host*, under a profile
    /// that says it is in a VM.
    #[test]
    fn a_launch_composed_without_a_place_is_refused_rather_than_run() {
        let backend = backend(host());
        let policy = resolved(|_| {});
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let err = backend.wrap(vec!["claude".into()], &launch).unwrap_err();
        assert!(matches!(err, SandboxError::Refused { .. }), "{err}");
        assert!(err.to_string().contains("without one"), "{err}");
    }

    /// The one place this backend genuinely differs, and the one that would
    /// silently disable the firewall if it were got wrong.
    #[test]
    fn a_filtered_launch_is_refused_because_the_socket_does_not_cross_the_vm_boundary() {
        let backend = backend(host());
        let place = PlaceLaunch { relay: None };

        // An unrestricted `full` is the mode this backend can honour, and the
        // agent's argv crosses unchanged: there is no relay to start beside it.
        let open = resolved(|_| {});
        let launch = SandboxLaunch::new(&open, "/Users/u", "s1").with_place(place);
        assert_eq!(
            backend.wrap(vec!["claude".into()], &launch).unwrap(),
            ["claude"]
        );

        // A proxied one is refused rather than started believing it is filtered.
        let filtered = resolved(|profile| profile.network_mode = NetworkMode::Allowlist);
        let launch = SandboxLaunch::new(&filtered, "/Users/u", "s1")
            .with_place(place)
            .with_proxy(ProxyEndpoint::UnixSocket {
                host_path: "/data/pl/dev/abc/proxy.sock".to_string(),
                inside_path: "/data/pl/dev/abc/proxy.sock".to_string(),
            });
        let err = backend.wrap(vec!["claude".into()], &launch).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
        assert!(text.contains("virtual machine"), "{text}");
        assert!(text.contains("seatbelt"), "{text}");

        // And a launch that carries a relay for a place that can have none.
        let with_relay = SandboxLaunch::new(&open, "/Users/u", "s1").with_place(PlaceLaunch {
            relay: Some(PlaceRelay {
                program: "/usr/local/bin/friring-cli",
                port: 8118,
            }),
        });
        let err = backend
            .wrap(vec!["claude".into()], &with_relay)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing for one to forward to"), "{err}");
    }

    /// Every mode but an unrestricted `full` is turned down before a place is
    /// created — so no proxy is ever bound for a boundary that cannot use one.
    #[cfg(unix)]
    #[test]
    fn a_profile_this_backend_cannot_enforce_is_refused_before_anything_is_built() {
        dirs::cleanup_place("unenforceable");
        let backend = backend(host());
        for (mode, deny, needle) in [
            (NetworkMode::Allowlist, vec![], "virtual machine"),
            (
                NetworkMode::Full,
                vec!["evil.example".to_string()],
                "virtual machine",
            ),
            (NetworkMode::None, vec![], "no route off the machine"),
        ] {
            let mut asked = profile("unenforceable");
            asked.network_mode = mode;
            asked.network_deny = deny;
            let err = backend.ensure_place(&asked).unwrap_err();
            let text = err.to_string();
            assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
            assert!(text.contains(needle), "{mode}: {text}");
        }
        // Nothing was minted on the way to any of those refusals: a boundary
        // friring cannot apply is not one to build a place's tree for.
        assert!(!dirs::place_dir("unenforceable").is_some_and(|dir| dir.exists()));

        // And the last gate, where the argv is rendered: a plan asking for a
        // network friring cannot give here is refused rather than started on
        // friring's own, whatever composed it.
        let closed = resolved(|profile| profile.network_mode = NetworkMode::None);
        let plan = plan_instance(PlanInput {
            policy: &closed,
            image: IMAGE,
            place_dir: &dirs::place_dir("lastgate").unwrap().display().to_string(),
            home_dir: &dirs::place_home_dir("lastgate")
                .unwrap()
                .display()
                .to_string(),
            user: None,
            userns_keep_id: false,
            check: MountCheck {
                friring_db: None,
                home: None,
                exists: &|_| true,
                resolve: &|path: &str| Ok(path.to_string()),
            },
        })
        .unwrap();
        let err = backend
            .create(&plan, plan::NETWORK, &|detail| SandboxError::Refused {
                profile: "lastgate".to_string(),
                detail,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not ask for"), "{err}");
    }

    /// An ensure puts the place on friring's own network — creating it when it
    /// is not there — and reuses a running place rather than building a second.
    ///
    /// Nothing scripts a `run` here, so a create would fail the test: reuse has
    /// to be reuse.
    #[cfg(unix)]
    #[test]
    fn an_ensure_creates_frirings_network_and_reuses_a_running_place() {
        let profile = profile("lifecycle");
        let planned = backend(host_for("lifecycle")).plan_for(&profile).unwrap().0;
        let running = container_json(
            &planned.name,
            "running",
            &friring_labels("lifecycle", &planned.spec),
        );
        let common = host_for("lifecycle")
            .with_command(
                &format!("{PROGRAM} images inspect {IMAGE}"),
                ProbeOutput::success("{}"),
            )
            .with_command(
                &format!("{PROGRAM} inspect {}", planned.name),
                ProbeOutput::success(&running),
            );

        // The network is not there yet, so it is created before the place.
        let cold = common
            .clone()
            .with_command(
                &format!("{PROGRAM} network ls"),
                ProbeOutput::success("NAME\ndefault\n"),
            )
            .with_command(
                &format!("{PROGRAM} network create {}", plan::NETWORK),
                ProbeOutput::success(""),
            );
        let place = backend(cold).ensure_place(&profile).unwrap();
        assert_eq!(place.instance.external_id, planned.name);
        assert_eq!(place.instance.engine, SandboxBackendKind::AppleContainer);
        assert_eq!(place.instance.state, INSTANCE_STATE_RUNNING);
        assert_eq!(place.home_dir, planned.home_dir);
        // A place here never has a relay: there is nothing for one to reach.
        assert!(place.relay_program.is_none());

        // And when it is already there, nothing creates it a second time — the
        // `network create` is unscripted, so an attempt would fail the ensure.
        let warm = common.with_command(
            &format!("{PROGRAM} network ls"),
            ProbeOutput::success(format!("NAME\ndefault\n{}\n", plan::NETWORK)),
        );
        assert_eq!(
            backend(warm).ensure_place(&profile).unwrap().instance,
            place.instance
        );
        dirs::cleanup_place("lifecycle");
    }

    /// A host that remembers what the tool was asked, and lets a `run` change
    /// what a later `inspect` describes.
    ///
    /// `StubHost` answers the same thing however often it is asked, which
    /// cannot express a lifecycle: whether a stopped place was started or
    /// replaced is a question about the *sequence* of commands and about which
    /// container exists afterwards, not about any one answer.
    #[cfg(unix)]
    struct Recording {
        base: StubHost,
        log: std::sync::Mutex<Vec<String>>,
        /// What `inspect` describes once a `run` has succeeded — the container
        /// the recovery built, which is a different one.
        replacement: Option<String>,
        created: std::sync::Mutex<bool>,
    }

    #[cfg(unix)]
    impl Recording {
        fn new(base: StubHost, replacement: Option<String>) -> Arc<Self> {
            Arc::new(Self {
                base,
                log: std::sync::Mutex::new(Vec::new()),
                replacement,
                created: std::sync::Mutex::new(false),
            })
        }

        /// The lifecycle commands only: the probe's own `--version`,
        /// `system status` and the two `--help` reads are not what these tests
        /// are about.
        fn lifecycle(&self) -> Vec<String> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|call| {
                    matches!(call.split(' ').next(), Some("start" | "stop" | "delete"))
                        || (call.starts_with("run ") && !call.contains("--help"))
                })
                .map(|call| match call.starts_with("run ") {
                    // The create argv carries the whole mount plan; the verb is
                    // what this assertion is about.
                    true => "run".to_string(),
                    false => call.clone(),
                })
                .collect()
        }
    }

    #[cfg(unix)]
    impl ProbeHost for Recording {
        fn which(&self, program: &str) -> Option<String> {
            self.base.which(program)
        }

        fn home(&self) -> Option<String> {
            self.base.home()
        }

        fn path_exists(&self, path: &str) -> bool {
            self.base.path_exists(path)
        }

        fn read_file(&self, path: &str) -> Option<String> {
            self.base.read_file(path)
        }

        fn run(&self, program: &str, args: &[&str]) -> Result<ProbeOutput, String> {
            if program != PROGRAM {
                return self.base.run(program, args);
            }
            self.log.lock().unwrap().push(args.join(" "));
            let verb = args.first().copied().unwrap_or_default();
            if verb == "run" && args.get(1) != Some(&"--help") {
                *self.created.lock().unwrap() = true;
            }
            if verb == "inspect" && *self.created.lock().unwrap() {
                if let Some(json) = &self.replacement {
                    return Ok(ProbeOutput::success(json));
                }
            }
            self.base.run(program, args)
        }
    }

    /// An owned place that is not running, with everything an ensure needs
    /// around it. `id` is deliberately not the container's name: the name is
    /// friring's handle, the id is what the tool answers with.
    #[cfg(unix)]
    fn stopped_place(name: &str, id: &str) -> (InstancePlan, StubHost) {
        let planned = backend(host_for(name)).plan_for(&profile(name)).unwrap().0;
        let host = host_for(name)
            .with_command(
                &format!("{PROGRAM} network ls"),
                ProbeOutput::success(format!("{}\n", plan::NETWORK)),
            )
            .with_command(
                &format!("{PROGRAM} images inspect {IMAGE}"),
                ProbeOutput::success("{}"),
            )
            .with_command(
                &format!("{PROGRAM} inspect {}", planned.name),
                ProbeOutput::success(container_json(
                    id,
                    "stopped",
                    &friring_labels(name, &planned.spec),
                )),
            );
        (planned, host)
    }

    /// A stopped place friring owns is started again, and the session lands in
    /// the same one: a restart must not throw away the work in it.
    #[cfg(unix)]
    #[test]
    fn a_stopped_place_is_started_rather_than_rebuilt() {
        let (_planned, host) = stopped_place("restarted", "stoppedplace1");
        let host = Recording::new(
            host.with_command(
                &format!("{PROGRAM} start stoppedplace1"),
                ProbeOutput::success(""),
            ),
            None,
        );
        let place = AppleContainerBackend::new(host.clone())
            .ensure_place(&profile("restarted"))
            .unwrap();

        assert_eq!(place.instance.external_id, "stoppedplace1");
        assert_eq!(host.lifecycle(), ["start stoppedplace1"]);
        dirs::cleanup_place("restarted");
    }

    /// A place that will not start is one whose VM went with a host reboot.
    /// It is taken away by name and built again — `stop` first, because a
    /// delete of a running container is refused — and the session lands in the
    /// replacement rather than on a dead handle.
    #[cfg(unix)]
    #[test]
    fn a_place_that_will_not_start_is_replaced_and_the_new_id_is_the_one_recorded() {
        let (planned, host) = stopped_place("wedged", "wedgedplace1");
        let replacement = container_json(
            "freshplace2",
            "running",
            &friring_labels("wedged", &planned.spec),
        );
        let host = Recording::new(
            host.with_command(
                &format!("{PROGRAM} start wedgedplace1"),
                ProbeOutput::failure(1, "the container could not be started\n"),
            )
            .with_command(
                &format!("{PROGRAM} stop wedgedplace1"),
                ProbeOutput::success(""),
            )
            .with_command(
                &format!("{PROGRAM} delete wedgedplace1"),
                ProbeOutput::success(""),
            )
            .with_command_prefix(&format!("{PROGRAM} run "), ProbeOutput::success("")),
            Some(replacement),
        );
        let place = AppleContainerBackend::new(host.clone())
            .ensure_place(&profile("wedged"))
            .unwrap();

        assert_eq!(place.instance.external_id, "freshplace2");
        assert_eq!(
            host.lifecycle(),
            [
                "start wedgedplace1",
                "stop wedgedplace1",
                "delete wedgedplace1",
                "run"
            ]
        );
        dirs::cleanup_place("wedged");
    }

    /// And when the replacement will not build either, the ensure refuses with
    /// the tool's own reason rather than handing back the id it just deleted.
    #[cfg(unix)]
    #[test]
    fn a_replacement_that_will_not_build_refuses_the_ensure() {
        let (_planned, host) = stopped_place("doomed", "doomedplace1");
        let host = Recording::new(
            host.with_command(
                &format!("{PROGRAM} start doomedplace1"),
                ProbeOutput::failure(1, "the container could not be started\n"),
            )
            .with_command(
                &format!("{PROGRAM} stop doomedplace1"),
                ProbeOutput::success(""),
            )
            .with_command(
                &format!("{PROGRAM} delete doomedplace1"),
                ProbeOutput::success(""),
            )
            .with_command_prefix(
                &format!("{PROGRAM} run "),
                ProbeOutput::failure(1, "no space left on device\n"),
            ),
            None,
        );
        let err = AppleContainerBackend::new(host.clone())
            .ensure_place(&profile("doomed"))
            .unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
        assert!(text.contains("no space left on device"), "{text}");
        // Nothing after the create it could not do.
        assert_eq!(host.lifecycle().last().unwrap(), "run");
        dirs::cleanup_place("doomed");
    }

    /// friring only ever reuses or removes what it can prove it created, and the
    /// proof is its own label.
    #[cfg(unix)]
    #[test]
    fn a_container_that_is_not_frirings_is_neither_reused_nor_removed() {
        let profile = profile("squatted");
        let planned = backend(host_for("squatted")).plan_for(&profile).unwrap().0;
        let scripted = host_for("squatted")
            .with_command(
                &format!("{PROGRAM} network ls"),
                ProbeOutput::success(format!("{}\n", plan::NETWORK)),
            )
            .with_command(
                &format!("{PROGRAM} images inspect {IMAGE}"),
                ProbeOutput::success("{}"),
            )
            .with_command(
                &format!("{PROGRAM} inspect {}", planned.name),
                ProbeOutput::success(container_json(&planned.name, "running", "")),
            );
        let err = backend(scripted).ensure_place(&profile).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("was not created by friring"), "{text}");
        assert!(text.contains("neither reused nor removed"), "{text}");
        dirs::cleanup_place("squatted");
    }

    /// The tool is asked what it made rather than believed about what it
    /// printed, which is what catches a label that did not land — and every
    /// later lookup and removal depends on that label being there.
    #[cfg(unix)]
    #[test]
    fn a_created_container_without_frirings_label_is_refused() {
        let planned = backend(host_for("unlabelled"))
            .plan_for(&profile("unlabelled"))
            .unwrap()
            .0;
        let scripted = host_for("unlabelled")
            .with_command_prefix(&format!("{PROGRAM} run "), ProbeOutput::success(""))
            .with_command(
                &format!("{PROGRAM} inspect {}", planned.name),
                ProbeOutput::success(container_json(&planned.name, "running", "")),
            );
        let refuse = |detail: String| SandboxError::Refused {
            profile: "unlabelled".to_string(),
            detail,
        };
        let err = backend(scripted)
            .create(&planned, plan::NETWORK, &refuse)
            .unwrap_err()
            .to_string();
        assert!(err.contains("without friring's own label"), "{err}");
        dirs::cleanup_place("unlabelled");
    }

    /// bwrap refuses a launch whose writable roots enclose the program applying
    /// its boundary; a place is no different, and this CLI is that program.
    #[test]
    fn a_profile_that_hands_over_the_tool_binary_is_refused_before_anything_is_built() {
        let backend = backend(host());
        let mut granted = profile("handover");
        granted.paths = vec![SandboxPath::workspace("/usr/bin")];
        let err = backend.plan_for(&granted).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
        assert!(text.contains(PROGRAM), "{text}");
        assert!(
            text.contains("replace the program that applies its own boundary"),
            "{text}"
        );

        // Read-only is a different grant: nothing there can replace anything, so
        // this one gets as far as the ordinary mount checks.
        let mut readable = profile("handover");
        readable.paths = vec![SandboxPath::read_only("/usr/bin")];
        let text = backend.plan_for(&readable).unwrap_err().to_string();
        assert!(!text.contains("replace the program"), "{text}");
        dirs::cleanup_place("handover");
    }

    /// ADR-29 is absolute, and a place makes the read-only half matter: a
    /// read-only bind of the data directory would still carry the automation
    /// commands the *host* executes.
    #[cfg(unix)]
    #[test]
    fn no_mount_may_reach_the_data_directory() {
        // `host_for` mints this profile's place tree first, so the data
        // directory is there to be resolved — and it is resolved because a
        // test's temp root is itself symlinked on macOS, while this test is
        // about ADR-29 rather than about the symlink rule that would otherwise
        // refuse it first.
        let seen = host_for("adr29").with_path("/Users/u");
        let data = std::fs::canonicalize(dirs::data_dir().unwrap())
            .unwrap()
            .display()
            .to_string();
        let backend = backend(seen.with_path(&data));
        for path in [
            SandboxPath::workspace(&data),
            SandboxPath::read_only(&data),
            SandboxPath::workspace("~"),
            SandboxPath::read_only("~"),
        ] {
            let names_the_data_dir = path.path == data;
            let mut asked = profile("adr29");
            asked.paths = vec![path];
            let err = backend.plan_for(&asked).unwrap_err();
            let text = err.to_string();
            assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
            if names_the_data_dir {
                // Read-only is no defence: the database there holds automation
                // commands the *host* executes.
                assert!(text.contains("ADR-29"), "{text}");
            } else {
                // A home-wide grant is refused too — for whichever of the rules
                // it trips first on this host (the data directory it encloses,
                // a tmux socket directory, the engine's own control socket).
                assert!(text.contains("/Users/u"), "{text}");
            }
        }
        // And no shape a launchable profile can take mounts anything that
        // reaches the database or the tree friring keeps it in.
        let planned = backend.plan_for(&profile("adr29")).unwrap().0;
        let data = dirs::data_dir().unwrap().display().to_string();
        for mount in &planned.mounts {
            assert!(
                !dirs::encloses(&mount.source, &data),
                "mounted '{}', which reaches '{data}'",
                mount.source
            );
        }
        dirs::cleanup_place("adr29");
    }

    /// A limit friring cannot pass is a limit that is not enforced, and that is
    /// reported rather than accepted and ignored.
    #[test]
    fn a_limit_this_build_of_the_tool_cannot_take_is_refused() {
        let older = host_answering(
            "-d, --detach --name <name> -l, --label <label> --mount <mount> --network <network>",
        );
        let backend = backend(older);
        assert!(backend.probe().is_available());
        let mut capped = profile("capped");
        capped.memory_mb = Some(4096);
        let text = backend.plan_for(&capped).unwrap_err().to_string();
        assert!(text.contains("--memory"), "{text}");
        assert!(text.contains("could not pass"), "{text}");
    }

    /// Nothing may be ensured on a host without the tool, and the reason is the
    /// probe's own sentence rather than a later, stranger failure.
    #[test]
    fn an_unavailable_tool_refuses_before_it_touches_anything() {
        let bare = StubHost::macos(26, true);
        let err = backend(bare).ensure(&profile("nothing")).unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable { .. }), "{err}");
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    /// The transport addresses a place by the path this probe pinned — never a
    /// bare name, which an inherited `PATH` would get to choose.
    #[test]
    fn the_tool_the_transport_runs_is_the_one_the_probe_vetted() {
        assert_eq!(backend(host()).engine_program().unwrap(), PROGRAM);
        assert!(PROGRAM.starts_with('/'));
    }

    /// Everything the tool tells friring about a place ends up as the argument
    /// after `exec`'s flags, so nothing that could pass for a flag is accepted.
    #[test]
    fn only_a_reference_the_tool_could_have_minted_is_accepted() {
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
        // And a document naming one is not a container friring will act on.
        assert!(parse_containers(&container_json("-i", "running", "")).is_empty());
    }

    /// The tool's JSON is the tool's, so it is read where either rendering would
    /// put things — and never leniently enough to invent ownership.
    #[test]
    fn the_json_the_tool_prints_is_read_where_either_rendering_puts_it() {
        let nested = container_json("ctr1", "running", &friring_labels("dev", "aaaa"));
        let parsed = parse_containers(&nested);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].owned);
        assert!(parsed[0].running());
        assert_eq!(parsed[0].profile.as_deref(), Some("dev"));
        assert_eq!(parsed[0].spec.as_deref(), Some("aaaa"));

        // A flat rendering, in a list, with a state field spelled the other way.
        let flat = format!(
            "[{{\"id\":\"ctr2\",\"state\":\"STOPPED\",\"labels\":{{{}}}}}]",
            friring_labels("dev", "bbbb")
        );
        let parsed = parse_containers(&flat);
        assert_eq!(parsed[0].id, "ctr2");
        assert!(!parsed[0].running());

        // Somebody else's container, and something that is not JSON at all:
        // neither is ever friring's.
        let foreign = parse_containers(&container_json("ctr3", "running", "\"other\":\"1\""));
        assert!(!foreign[0].owned);
        assert!(foreign[0].profile.is_none());
        assert!(parse_containers("NAME  STATE\nctr  running").is_empty());
        assert!(parse_containers("").is_empty());
    }

    /// The only function here that destroys anything, over the shapes a planned
    /// id can turn out to have by the time it is reached.
    ///
    /// The load-bearing one is the second: the label is re-checked immediately
    /// before removal, so a container that lost friring's label is left alone and
    /// said out loud rather than removed.
    #[test]
    fn reaping_removes_only_what_is_still_frirings_and_reports_the_rest() {
        let inspect = |id: &str| format!("{PROGRAM} inspect {id}");
        let host = host()
            .with_command(
                &inspect("mine"),
                ProbeOutput::success(container_json(
                    "mine",
                    "running",
                    &friring_labels("dev", "a"),
                )),
            )
            .with_command(&format!("{PROGRAM} stop mine"), ProbeOutput::success(""))
            .with_command(&format!("{PROGRAM} delete mine"), ProbeOutput::success(""))
            // Not ours any more: no `stop` or `delete` is scripted, so one
            // reaching the tool would fail this test.
            .with_command(
                &inspect("theirs"),
                ProbeOutput::success(container_json("theirs", "running", "")),
            )
            .with_command(
                &inspect("vanished"),
                ProbeOutput::failure(1, "no such container\n"),
            )
            .with_command(
                &inspect("stuck"),
                ProbeOutput::success(container_json(
                    "stuck",
                    "running",
                    &friring_labels("dev", "a"),
                )),
            )
            .with_command(&format!("{PROGRAM} stop stuck"), ProbeOutput::success(""))
            .with_command(
                &format!("{PROGRAM} delete stuck"),
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

    /// What garbage collection sees, and the ownership it is allowed to assume.
    #[test]
    fn listing_places_reports_ownership_from_frirings_own_label() {
        let listed = format!(
            "[{},{}]",
            container_json("mine", "running", &friring_labels("dev", "aaaa")),
            container_json("theirs", "running", "\"com.example\":\"1\"")
        );
        let listing = host().with_command(
            &format!("{PROGRAM} ls --all --format json"),
            ProbeOutput::success(&listed),
        );
        let places = backend(listing).live_places().unwrap();
        assert_eq!(places.len(), 2);
        assert!(places[0].owned);
        assert_eq!(places[0].profile.as_deref(), Some("dev"));
        assert!(!places[1].owned);

        // A tool that would not answer is an error rather than an empty list:
        // "nothing is running" and "nothing could be asked" decide opposite
        // things in a collection pass.
        let mute = host().with_command(
            &format!("{PROGRAM} ls --all --format json"),
            ProbeOutput::failure(1, "the service is not running\n"),
        );
        let err = backend(mute).live_places().unwrap_err();
        assert!(err.to_string().contains("service is not running"), "{err}");
    }

    /// A place runs the agent inside itself, so a missing binary is refused with
    /// the command that puts one there — never a pane that dies at once.
    #[test]
    fn a_place_without_the_agent_is_refused_with_the_install_command() {
        let policy = resolved(|_| {});
        let place = EnsuredPlace {
            instance: SandboxInstance {
                profile: "dev".to_string(),
                engine: SandboxBackendKind::AppleContainer,
                external_id: "ctr1".to_string(),
                state: INSTANCE_STATE_RUNNING.to_string(),
            },
            home_dir: "/data/pl/dev/home".to_string(),
            relay_program: None,
        };
        let lookup = format!("{PROGRAM} exec ctr1 {SHELL} -c {LOOKUP} {LOOKUP_NAME} claude");

        // Not there: `command -v` says nothing at all, on neither stream.
        let missing = host().with_command(&lookup, ProbeOutput::failure(1, ""));
        let err = backend(missing)
            .ensure_agent_program(&policy, &place, "claude")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no 'claude' on PATH"), "{err}");
        assert!(err.contains("/data/pl/dev/home"), "{err}");
        assert!(err.contains("npm install -g"), "{err}");

        // An `exec` that could not run at all is reported as that instead —
        // "install the agent" would be the wrong fix.
        let broken = host().with_command(
            &lookup,
            ProbeOutput::failure(126, "exec: /bin/sh: no such file\n"),
        );
        let err = backend(broken)
            .ensure_agent_program(&policy, &place, "claude")
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not ask"), "{err}");

        // And a place that has it launches.
        let ready = host().with_command(
            &lookup,
            ProbeOutput::success("/home/agent/.npm-global/bin/claude\n"),
        );
        backend(ready)
            .ensure_agent_program(&policy, &place, "claude")
            .unwrap();
    }

    /// A plan's mounts are decided long before the container is made — an image
    /// pull or build sits in between — and every source in it is a path some
    /// other place's agent may be writing the whole time. So they are checked
    /// again with nothing between the check and the spawn.
    #[cfg(unix)]
    #[test]
    fn a_source_that_became_a_symlink_after_planning_is_refused_at_create() {
        let base = dirs::test_temp_base("apple-create-recheck");
        let repo = base.join("repo");
        let hooks = repo.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let (place, home) = dirs::create_place_dirs("apple-recheck").unwrap();
        let place_dir = place.display().to_string();
        let home_dir = home.display().to_string();
        let policy = resolved(|profile| {
            profile.name = "apple-recheck".to_string();
            profile.paths = vec![SandboxPath::workspace(repo.display().to_string())];
        });

        // Planned when everything was still what it claimed to be.
        let identity = |path: &str| Ok(path.to_string());
        let plan = plan_instance(PlanInput {
            policy: &policy,
            image: IMAGE,
            place_dir: &place_dir,
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

        let mut scripted = host()
            .with_command_prefix(&format!("{PROGRAM} run "), ProbeOutput::success(""))
            .with_command(
                &format!("{PROGRAM} inspect {}", plan.name),
                ProbeOutput::success(container_json(
                    &plan.name,
                    "running",
                    &friring_labels("apple-recheck", &plan.spec),
                )),
            );
        for path in [&repo.display().to_string(), &hooks.display().to_string()] {
            scripted = scripted.with_path(path);
        }
        let backend = backend(scripted.with_path(&place_dir).with_path(&home_dir));
        let refuse = |detail: String| SandboxError::Refused {
            profile: "apple-recheck".to_string(),
            detail,
        };
        assert_eq!(
            backend.create(&plan, plan::NETWORK, &refuse).unwrap(),
            plan.name
        );

        // And now a source changes meaning underneath the plan.
        std::fs::remove_dir(&hooks).unwrap();
        std::os::unix::fs::symlink(dirs::tmux_socket_root(), &hooks).unwrap();
        let err = backend
            .create(&plan, plan::NETWORK, &refuse)
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(err.contains(&hooks.display().to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&base);
        dirs::cleanup_place("apple-recheck");
    }
}
