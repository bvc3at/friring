//! Which image a place runs, what that image has to provide, and how it comes
//! to exist.
//!
//! A place runs the agent **inside** itself (ADR-26), so its image is the whole
//! userland a session gets. Four things have to be in there, and the fourth is
//! the one friring's own image deliberately leaves out:
//!
//! 1. `tmux` — the transport runs tmux in the place rather than on the host.
//! 2. `/bin/sh` — the relay launcher and the keepalive command are POSIX sh.
//! 3. `friring-cli`, whenever the profile filters egress — the in-namespace
//!    relay ([`RELAY_BINARY`](super::RELAY_BINARY)).
//! 4. **The agent's own CLI.** The default image carries none, on purpose:
//!    baking one in pins a vendor's release into an image contract that has no
//!    way to follow it, and choosing *which* vendors to carry is not friring's
//!    to make — it is agent-neutral by design, and every runtime added to the
//!    image is inside every boundary built from it. An agent arrives instead
//!    through a one-time install into the profile's synthetic home, which is
//!    friring's own directory and outlives every container built for that
//!    profile (the same directory the profile's login lives in, ADR-28), or
//!    through an `image` / `containerfile` of the user's own.
//!
//! [`ContainerBackend::ensure_agent_program`] is what keeps (4) from being
//! discovered as a dead pane: a launch whose agent is not in the place is
//! refused with the command that puts one there.
//! `packaging/sandbox/Containerfile` is this same list written from the other
//! side, and `the_default_image_and_the_containerfile_describe_one_image` holds
//! the two together.
//!
//! friring publishes no image: the default one is defined declaratively by that
//! Containerfile and built under a documented tag, so nothing in the code
//! depends on a registry friring would have to keep, and nobody's agent ends up
//! running whatever a name resolved to today. The consequence is deliberate: a
//! missing default image is a **refusal with the build command**, not a silent
//! pull. A tag the *user* named in their profile is theirs, and is pulled if the
//! engine does not have it.

use crate::sandbox::backend::{SandboxError, SandboxResult};
use crate::sandbox::dirs;
use crate::sandbox::launcher::SHELL;
use crate::session::SandboxPolicy;

use super::{ContainerBackend, EnsuredPlace, CONTAINER_HOME};

/// The image a profile gets when it names none.
///
/// Registry-less on purpose — a bare `name:tag` an engine can only satisfy
/// locally. Built from `packaging/sandbox/Containerfile`; the tag's number moves
/// when what a place may assume about that file changes (the tools it provides,
/// the home and user it declares), so an old image is never silently reused
/// under a new contract.
pub const DEFAULT_IMAGE: &str = "friring/sandbox:1";

/// How to build the default image, quoted verbatim in the refusal that needs it.
pub const DEFAULT_IMAGE_BUILD: &str =
    "build it once: docker build -t friring/sandbox:1 - < packaging/sandbox/Containerfile \
     (podman build … for podman)";

/// Where a one-time agent install lands inside a place, relative to its home.
///
/// The npm global prefix the default image points into the home, and the
/// `~/.local/bin` that `pip --user`, `uv` and most `curl | sh` installers write
/// to. Named here because [`ContainerBackend::ensure_agent_program`] sends the
/// user to install an agent into the profile's home, and that instruction is
/// only true while the image puts these directories on `PATH` — which is what
/// the conformance test over `packaging/sandbox/Containerfile` checks.
pub const INSTALL_BIN_DIRS: &[&str] = &[".npm-global/bin", ".local/bin"];

/// Asks a place where a program is, with the name as a positional parameter.
///
/// `command -v` is POSIX and answers with an absolute path for anything on
/// `PATH`. The name arrives as `"$1"` rather than spliced into the script
/// because it comes from the user's `agents.toml`: a shell command built by
/// concatenation is one an unusual agent name gets to rewrite.
const LOOKUP: &str = "command -v \"$1\"\n";

/// `$0` for that shell, so a `ps` inside the place says what the process is.
const LOOKUP_NAME: &str = "friring-agent-lookup";

/// Where an image comes from for one profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// [`DEFAULT_IMAGE`]: friring's own, built locally from the repository's
    /// Containerfile.
    Default,
    /// A reference the profile named. The user's choice, so the engine may pull
    /// it.
    Named(String),
    /// Built from the profile's own `containerfile`, under a tag derived from
    /// the profile.
    Built { tag: String, containerfile: String },
}

impl ImageSource {
    /// What a `run` names.
    pub fn reference(&self) -> &str {
        match self {
            Self::Default => DEFAULT_IMAGE,
            Self::Named(reference) => reference,
            Self::Built { tag, .. } => tag,
        }
    }

    /// Whether the engine may fetch this from a registry when it is missing.
    /// Only a reference the user wrote: friring's own default is built, and a
    /// silent pull of `friring/sandbox:1` would reach whatever Docker Hub
    /// resolves that to.
    pub fn may_pull(&self) -> bool {
        matches!(self, Self::Named(_))
    }

    /// The sentence for an image that is missing and cannot be fetched.
    pub fn missing(&self, profile: &str) -> SandboxError {
        let detail = match self {
            Self::Default => format!(
                "the default sandbox image '{DEFAULT_IMAGE}' is not on this host. friring does \
                 not publish it — {DEFAULT_IMAGE_BUILD}"
            ),
            Self::Named(reference) => {
                format!("the image '{reference}' could not be found or pulled")
            }
            Self::Built { tag, containerfile } => {
                format!("the image '{tag}' could not be built from '{containerfile}'")
            }
        };
        SandboxError::Refused {
            profile: profile.to_string(),
            detail,
        }
    }
}

/// Which image this policy's place runs.
///
/// `image` and `containerfile` are mutually exclusive — the profile validator
/// refuses both — so the order here is a preference only in the sense that a
/// stored row friring did not write cannot make it ambiguous.
///
/// # Errors
///
/// A `containerfile` that is not an absolute path: the build's context is its
/// own directory, and a relative path would resolve against whatever working
/// directory friring happens to have.
pub fn resolve(policy: &SandboxPolicy) -> SandboxResult<ImageSource> {
    if let Some(containerfile) = policy.containerfile.as_deref().map(str::trim) {
        if !containerfile.is_empty() {
            if !containerfile.starts_with('/') {
                return Err(SandboxError::Refused {
                    profile: policy.profile.clone(),
                    detail: format!(
                        "the containerfile '{containerfile}' is not an absolute path, and its \
                         directory is the build context — a relative one would resolve against \
                         whichever directory friring was started in"
                    ),
                });
            }
            return Ok(ImageSource::Built {
                tag: built_tag(&policy.profile),
                containerfile: containerfile.to_string(),
            });
        }
    }
    match policy.image.as_deref().map(str::trim) {
        Some(reference) if !reference.is_empty() => Ok(ImageSource::Named(reference.to_string())),
        _ => Ok(ImageSource::Default),
    }
}

/// The tag a profile's own containerfile is built under.
fn built_tag(profile: &str) -> String {
    format!("friring-sbx-{}:latest", dirs::sanitize_component(profile))
}

/// The build's context directory: the containerfile's own parent.
pub fn build_context(containerfile: &str) -> &str {
    match containerfile.rfind('/') {
        Some(0) => "/",
        Some(cut) => &containerfile[..cut],
        None => ".",
    }
}

impl ContainerBackend {
    /// Refuse this launch unless the place carries the agent it is about to
    /// run.
    ///
    /// The one item of the image contract friring cannot supply itself (see the
    /// module docs). Without this check the launch composes, the transport opens
    /// a window inside the place, and tmux runs a command that is not there: a
    /// pane that dies the instant it appears, taking with it the sign-in that
    /// `volume-login` does *in that pane*. So it is refused instead, with the
    /// command that puts an agent in the place for good — and the profile's own
    /// `allow_unsandboxed_fallback` decides what happens next, exactly as it
    /// does for an image that is not there at all.
    ///
    /// Asked per launch rather than folded into
    /// [`ensure_place`](ContainerBackend::ensure_place), because a place is
    /// shared by every session of its profile and those sessions may run
    /// different agents — the place is right and one launch into it is not.
    ///
    /// Asked of the *place* rather than of the image, because the supported
    /// route to an agent is an install into the profile's synthetic home, and
    /// that home is a mount rather than a layer of the image.
    ///
    /// # Errors
    ///
    /// The place has no such program on its `PATH`, or the engine would not
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
        let source = resolve(policy)?;
        let unanswered = |detail: &str| {
            refuse(format!(
                "friring could not ask this profile's place whether it carries '{program}', so it \
                 will not start a session that may open on a dead pane: {detail}"
            ))
        };

        let asked = self.engine_run(&[
            "exec",
            &place.instance.external_id,
            SHELL,
            "-c",
            LOOKUP,
            LOOKUP_NAME,
            program,
        ]);
        match asked {
            // The place answered with an absolute path, which is the whole
            // question: `command -v` names a shell builtin or a relative match
            // without one, and neither is a program tmux can run in there.
            Ok(output) if output.ok() && output.trimmed().starts_with('/') => Ok(()),
            Ok(output) => {
                // `command -v` says nothing at all when it finds nothing, so
                // anything on stderr came from the engine or from the image —
                // an `exec` that could not run (no `/bin/sh`, an image built
                // for another architecture) rather than an agent that is not
                // installed. Reporting one as the other would send the user to
                // install something they already have.
                let engine_said = super::first_line(&output.stderr, "");
                if engine_said.is_empty() {
                    Err(refuse(self.missing_agent(&source, place, program)))
                } else {
                    Err(unanswered(&engine_said))
                }
            }
            Err(detail) => Err(unanswered(&detail)),
        }
    }

    /// What to do about a place that does not carry `program`.
    ///
    /// Two routes, and which one leads is decided by whose image it is: friring
    /// owns the default one and can say why it is empty, while an image the user
    /// named is theirs to add to. Both end in something runnable — a refusal
    /// that only reports the problem is how this feature would look broken
    /// rather than unfinished.
    fn missing_agent(&self, source: &ImageSource, place: &EnsuredPlace, program: &str) -> String {
        let image = source.reference();
        // Only the default image is known to have node, npm and a `PATH` that
        // reaches the profile's home, so only there can the whole command be
        // written out; anywhere else the shell is the honest tail.
        let (install, tail) = match source {
            ImageSource::Default => (
                self.install_command(
                    place,
                    image,
                    &format!("npm install -g <the package that provides '{program}'>"),
                ),
                String::new(),
            ),
            _ => (
                self.install_command(place, image, SHELL),
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
        match source {
            ImageSource::Default => format!(
                "the image '{image}' has no '{program}' on PATH, and a place runs the agent \
                 inside itself, so this session would open on a pane that dies at once.{named} \
                 friring's own image carries no agent CLI by design; install yours once into this \
                 profile's home, which lives outside the image and survives every container this \
                 profile rebuilds: {install}{tail}. Or point this profile's 'image' or \
                 'containerfile' at one that already carries '{program}' — \
                 packaging/sandbox/Containerfile states what a place's image must provide."
            ),
            _ => format!(
                "the image '{image}' has no '{program}' on PATH, and a place runs the agent \
                 inside itself, so this session would open on a pane that dies at once.{named} \
                 Add '{program}' to that image — packaging/sandbox/Containerfile states what a \
                 place's image must provide — or, where that image's PATH reaches this profile's \
                 home, install it there once: {install}{tail}."
            ),
        }
    }

    /// The command that puts an agent into this profile's home for good.
    ///
    /// A throwaway container rather than the place itself, and both reasons
    /// bite: the place's network is whatever the profile granted it, which for
    /// every mode but an unrestricted `full` is nothing an installer can use;
    /// and an install performed *in the image* is built for the image's
    /// architecture, which is routinely not the host's (a linux/arm64 place on
    /// an Apple Silicon machine, an amd64 one under emulation). The mount is the
    /// same synthetic home the place gets, at the same path, so what lands there
    /// is what the next launch finds on `PATH`.
    ///
    /// The identity flags are the ones the place itself was created with, for
    /// the same reason: files written as another user are files the sandbox
    /// cannot run.
    fn install_command(&self, place: &EnsuredPlace, image: &str, installer: &str) -> String {
        let details = self.details();
        let engine = details
            .program
            .as_deref()
            .unwrap_or_else(|| self.engine().program());
        let mut argv: Vec<String> = ["run", "--rm", "-it"]
            .iter()
            .map(|t| t.to_string())
            .collect();
        if let Some(user) = details.run_as_user() {
            argv.push("--user".to_string());
            argv.push(user.to_string());
        }
        if details.userns_keep_id(self.engine()) {
            argv.push("--userns".to_string());
            argv.push("keep-id".to_string());
        }
        argv.push("--mount".to_string());
        argv.push(format!(
            "type=bind,src={},dst={CONTAINER_HOME}",
            place.home_dir
        ));
        argv.push(image.to_string());
        argv.push(installer.to_string());
        format!("{engine} {}", argv.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::sandbox::container::ContainerEngine;
    use crate::sandbox::probe::{ProbeOutput, StubHost};
    use crate::session::{
        SandboxBackendKind, SandboxInstance, SandboxPath, SandboxProfile, SandboxShape,
    };

    /// Where the probe resolves an engine a package manager installed.
    const PROGRAM: &str = "/usr/bin/podman";
    const CONTAINER: &str = "friring-sbx-dev-0123456789ab";
    const HOME: &str = "/home/u/.local/share/friring/sandbox/pl/dev/home";

    fn resolved(mutate: impl FnOnce(&mut SandboxProfile)) -> SandboxPolicy {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        mutate(&mut profile);
        profile
            .resolve(SandboxBackendKind::Podman, "/home/u")
            .unwrap()
    }

    /// A Linux host whose podman is installed and answering.
    ///
    /// `rootless` is the one difference that reaches an install command: a
    /// rootful engine gives a container the host user's identity by naming it,
    /// a rootless one by mapping it, and an install made as the wrong user
    /// leaves the place files it cannot run.
    fn engine_host(rootless: bool) -> StubHost {
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
                ProbeOutput::success(format!("5.2.2|{rootless}\n")),
            )
    }

    /// The common case: rootless podman, which is the Linux ladder's preference.
    fn host() -> StubHost {
        engine_host(true)
    }

    /// The command line `ensure_agent_program` asks the place, as
    /// [`StubHost`] joins one.
    fn lookup(program: &str) -> String {
        format!("{PROGRAM} exec {CONTAINER} {SHELL} -c {LOOKUP} {LOOKUP_NAME} {program}")
    }

    fn backend(host: StubHost) -> ContainerBackend {
        ContainerBackend::new(ContainerEngine::Podman, Arc::new(host))
    }

    fn place() -> EnsuredPlace {
        EnsuredPlace {
            instance: SandboxInstance {
                profile: "dev".to_string(),
                engine: SandboxBackendKind::Podman,
                external_id: CONTAINER.to_string(),
                state: "running".to_string(),
            },
            home_dir: HOME.to_string(),
            relay_program: None,
        }
    }

    #[test]
    fn a_profile_naming_nothing_gets_the_repository_image() {
        let source = resolve(&resolved(|_| {})).unwrap();
        assert_eq!(source, ImageSource::Default);
        assert_eq!(source.reference(), DEFAULT_IMAGE);
        // Never pulled: friring publishes no registry image, so the honest
        // failure names the build rather than reaching for whatever that
        // reference resolves to on a registry.
        assert!(!source.may_pull());
        let missing = source.missing("dev").to_string();
        assert!(missing.contains("does not publish it"), "{missing}");
        assert!(
            missing.contains("packaging/sandbox/Containerfile"),
            "{missing}"
        );
    }

    #[test]
    fn a_named_image_is_the_users_and_may_be_pulled() {
        let source = resolve(&resolved(|p| p.image = Some("ghcr.io/me/dev:2".into()))).unwrap();
        assert_eq!(source.reference(), "ghcr.io/me/dev:2");
        assert!(source.may_pull());
    }

    #[test]
    fn a_containerfile_is_built_under_its_own_tag_from_its_own_directory() {
        let source = resolve(&resolved(|p| {
            p.containerfile = Some("/srv/images/dev/Containerfile".into())
        }))
        .unwrap();
        assert_eq!(source.reference(), "friring-sbx-dev:latest");
        assert!(!source.may_pull());
        assert_eq!(
            build_context("/srv/images/dev/Containerfile"),
            "/srv/images/dev"
        );
        assert_eq!(build_context("/Containerfile"), "/");

        // A relative containerfile would take its context from wherever friring
        // was started, which is not something a profile can mean.
        let err = resolve(&resolved(|p| {
            p.containerfile = Some("images/Containerfile".into())
        }))
        .unwrap_err();
        assert!(err.to_string().contains("not an absolute path"), "{err}");
    }

    /// The failure this refusal prevents: a place whose image has no agent runs
    /// tmux, opens a window, and the window's command is not there — a pane that
    /// dies the instant it appears, taking the login that happens *in that pane*
    /// with it.
    #[test]
    fn a_place_without_the_agent_is_refused_with_the_command_that_installs_one() {
        let rootless = backend(host().with_command(
            &lookup("claude"),
            // What `command -v` does when it finds nothing: exit 1, say nothing.
            ProbeOutput::failure(1, ""),
        ));
        let policy = resolved(|_| {});
        let err = rootless
            .ensure_agent_program(&policy, &place(), "claude")
            .unwrap_err();
        assert!(matches!(err, SandboxError::Refused { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("has no 'claude' on PATH"), "{text}");
        assert!(text.contains(DEFAULT_IMAGE), "{text}");
        // The whole command, not a hint: the engine the probe pinned, the
        // profile's own home at the path the place mounts it, and the image the
        // install has to be built inside.
        assert!(
            text.contains(&format!(
                "{PROGRAM} run --rm -it --userns keep-id --mount \
                 type=bind,src={HOME},dst={CONTAINER_HOME} {DEFAULT_IMAGE} npm install -g"
            )),
            "{text}"
        );
        // And the other route, which is what the profile's own knobs are for.
        assert!(text.contains("'image' or 'containerfile'"), "{text}");

        // A rootful engine names the host user instead of mapping it, and the
        // install has to be made by the same user the place runs as.
        let rootful = backend(
            engine_host(false).with_command(&lookup("claude"), ProbeOutput::failure(1, "")),
        );
        let text = rootful
            .ensure_agent_program(&policy, &place(), "claude")
            .unwrap_err()
            .to_string();
        assert!(text.contains("--user 1000:1000"), "{text}");
        assert!(!text.contains("keep-id"), "{text}");
    }

    /// The place answers with a path and the launch goes ahead — including for
    /// an agent installed into the profile's home, which is the route the
    /// refusal above sends people down.
    #[test]
    fn a_place_that_carries_the_agent_launches() {
        let installed = format!("{CONTAINER_HOME}/{}/claude\n", INSTALL_BIN_DIRS[0]);
        let backend = backend(
            host()
                .with_command(&lookup("claude"), ProbeOutput::success(installed))
                .with_command(&lookup("codex"), ProbeOutput::success("/usr/bin/codex\n")),
        );
        let policy = resolved(|_| {});
        backend
            .ensure_agent_program(&policy, &place(), "claude")
            .unwrap();
        backend
            .ensure_agent_program(&policy, &place(), "codex")
            .unwrap();
    }

    /// `command -v` answers a shell builtin with a bare word and a relative
    /// match with a relative path. Neither is a program tmux can run in a place,
    /// and treating one as an answer would put the dead pane back.
    #[test]
    fn only_an_absolute_answer_counts_as_the_agent() {
        let policy = resolved(|_| {});
        for answer in ["claude", "./claude", ""] {
            let backend = backend(host().with_command(
                &lookup("claude"),
                ProbeOutput::success(format!("{answer}\n")),
            ));
            let err = backend
                .ensure_agent_program(&policy, &place(), "claude")
                .unwrap_err();
            assert!(err.to_string().contains("has no 'claude' on PATH"), "{err}");
        }
    }

    /// An image the user named is theirs to add to, so the refusal leads with
    /// that — and the home-install route is offered with the condition it
    /// actually depends on, because only friring's own image is known to put the
    /// profile's home on `PATH`.
    #[test]
    fn a_users_own_image_is_told_to_carry_the_agent_itself() {
        let backend = backend(host().with_command(&lookup("claude"), ProbeOutput::failure(1, "")));
        let policy = resolved(|p| p.image = Some("ghcr.io/me/dev:2".into()));
        let text = backend
            .ensure_agent_program(&policy, &place(), "claude")
            .unwrap_err()
            .to_string();
        assert!(text.contains("ghcr.io/me/dev:2"), "{text}");
        assert!(text.contains("Add 'claude' to that image"), "{text}");
        assert!(text.contains("where that image's PATH reaches"), "{text}");
        // The shell, because nothing is known about what that image can install
        // with — but still the whole command around it.
        assert!(
            text.contains(&format!(
                "--mount type=bind,src={HOME},dst={CONTAINER_HOME} ghcr.io/me/dev:2 {SHELL}"
            )),
            "{text}"
        );
    }

    /// An agent named by a host path is not merely absent from the place: a
    /// place mounts what the profile granted and nothing else, so that name can
    /// never resolve in there. It is still *asked* first — a user's own image
    /// may well carry the agent at exactly that path, and refusing a working
    /// configuration on the strength of a slash would be a regression.
    #[test]
    fn an_agent_named_by_a_host_path_says_so_but_is_asked_first() {
        let policy = resolved(|_| {});
        let missing = backend(host().with_command(
            &lookup("/opt/homebrew/bin/claude"),
            ProbeOutput::failure(1, ""),
        ));
        let text = missing
            .ensure_agent_program(&policy, &place(), "/opt/homebrew/bin/claude")
            .unwrap_err()
            .to_string();
        assert!(text.contains("is a path on the host"), "{text}");
        assert!(text.contains("plain command in agents.toml"), "{text}");

        let carried = backend(host().with_command(
            &lookup("/usr/local/bin/claude"),
            ProbeOutput::success("/usr/local/bin/claude\n"),
        ));
        carried
            .ensure_agent_program(&policy, &place(), "/usr/local/bin/claude")
            .unwrap();
    }

    /// An `exec` that could not run at all — no `/bin/sh`, an image built for
    /// another architecture, an engine that went away mid-launch — is a
    /// different failure with a different fix, and answering it with "install
    /// the agent" would send the user after something they already have. It
    /// still fails closed: friring will not start a session it could not check.
    #[test]
    fn an_engine_that_will_not_answer_refuses_without_blaming_the_agent() {
        let policy = resolved(|_| {});
        let broken = backend(host().with_command(
            &lookup("claude"),
            ProbeOutput::failure(255, "exec format error\n"),
        ));
        let text = broken
            .ensure_agent_program(&policy, &place(), "claude")
            .unwrap_err()
            .to_string();
        assert!(
            text.contains("could not ask this profile's place"),
            "{text}"
        );
        assert!(text.contains("exec format error"), "{text}");
        assert!(!text.contains("npm install"), "{text}");

        // The engine could not be run at all: same verdict, its own words.
        let gone = backend(host());
        let text = gone
            .ensure_agent_program(&policy, &place(), "claude")
            .unwrap_err()
            .to_string();
        assert!(
            text.contains("could not ask this profile's place"),
            "{text}"
        );
        assert!(!text.contains("npm install"), "{text}");
    }

    /// The two halves of one contract: what this module says a place's image
    /// provides, and what `packaging/sandbox/Containerfile` actually builds.
    /// They shipped disagreeing once — the file carried no agent while this
    /// module said it did — which is a broken feature rather than a wrong
    /// comment, so the agreement is a test.
    #[test]
    fn the_default_image_and_the_containerfile_describe_one_image() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("packaging")
            .join("sandbox")
            .join("Containerfile");
        let text = std::fs::read_to_string(&path).expect("the repository's Containerfile");
        // What the *build* does, with the prose stripped: a comment may name an
        // agent (it documents how to install one) and an instruction may not.
        let instructions: String = text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        // The command friring quotes when the image is missing is the command
        // this file documents — and because that command carries the tag, it is
        // also how the tag friring looks for stays the tag the file builds.
        let build = DEFAULT_IMAGE_BUILD
            .split_once(": ")
            .expect("the build sentence names a command")
            .1;
        let build = build.split_once(" (").map_or(build, |(cmd, _)| cmd);
        assert!(build.contains(DEFAULT_IMAGE), "{build}");
        assert!(text.contains(build), "the Containerfile documents {build}");

        // The three things a place needs from any image and this one provides.
        assert!(instructions.contains("tmux"), "a place runs tmux inside it");
        assert!(
            instructions.contains("friring-cli"),
            "a filtered profile reaches the proxy through friring-cli in the image"
        );
        assert!(
            instructions.contains(&format!("HOME={CONTAINER_HOME}")),
            "the image's home is where a place mounts the profile's own"
        );

        // The fourth is deliberately absent, and this is what fails if an agent
        // is ever baked in without the module docs above following.
        for agent in ["claude", "codex", "opencode", "aider", "copilot"] {
            assert!(
                !instructions.contains(agent),
                "the default image carries no agent CLI, and an instruction names {agent}"
            );
        }

        // The refusal sends the user to install an agent into the profile's
        // home; that only works while the image looks for one there.
        let path_line = instructions
            .lines()
            .find(|line| line.contains("PATH="))
            .expect("the image declares a PATH");
        for dir in INSTALL_BIN_DIRS {
            assert!(
                path_line.contains(&format!("{CONTAINER_HOME}/{dir}")),
                "{CONTAINER_HOME}/{dir} is where an install lands, and PATH is {path_line}"
            );
        }
    }

    /// A place is the only shape this contract is about: a policy backend runs
    /// the host's own agent binary, which is why nothing here is reached from
    /// one.
    #[test]
    fn the_image_contract_belongs_to_the_place_shape() {
        assert_eq!(
            SandboxBackendKind::Podman.shape(),
            Some(SandboxShape::Place)
        );
        assert!(SandboxShape::Place.supports_image());
        assert!(!SandboxShape::Policy.supports_image());
    }
}
