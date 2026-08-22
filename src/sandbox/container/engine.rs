//! Which container engine is here, and what it can be told about itself.
//!
//! Docker and Podman are one backend implementation because their command lines
//! agree on everything this feature uses — `run`, `exec`, `inspect`, `ps
//! --filter label=`, `--mount type=bind`, `--network none`, `--memory`,
//! `--cpus`. They disagree about exactly two things, and both are probed rather
//! than assumed: the `info` template that names a version, and how a container
//! is made to run as the *host* user (`docs/SANDBOX.md` §`docker`/`podman`).
//!
//! Podman rootless is the better default on a shared or remote host — no
//! daemon, no root socket, and a compromise inside the sandbox reaches a
//! user-namespaced process rather than one talking to a root-owned socket —
//! which is why the Linux ladder puts it above Docker.

use crate::sandbox::backend::Availability;
use crate::sandbox::dirs;
use crate::sandbox::probe::{detect_platform, HostPlatform, ProbeHost};
use crate::session::SandboxBackendKind;

/// One container engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerEngine {
    Docker,
    Podman,
}

impl ContainerEngine {
    /// The engine behind a backend kind, or `None` for a kind that is not one.
    pub fn from_kind(kind: SandboxBackendKind) -> Option<Self> {
        match kind {
            SandboxBackendKind::Docker => Some(Self::Docker),
            SandboxBackendKind::Podman => Some(Self::Podman),
            _ => None,
        }
    }

    pub fn kind(self) -> SandboxBackendKind {
        match self {
            Self::Docker => SandboxBackendKind::Docker,
            Self::Podman => SandboxBackendKind::Podman,
        }
    }

    /// The name looked up on `PATH`. A lookup key and never what is executed:
    /// the probe resolves it to an absolute path once and vets the answer, for
    /// the same reason bubblewrap does (see
    /// [`crate::sandbox::dirs::rewritable_root`]) — the
    /// CLI is what asks for the isolation, so a copy the sandboxed agent can
    /// rewrite is a boundary the sandboxed agent chooses.
    pub fn program(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }

    /// The `info` template that yields `version|rootless`.
    ///
    /// The one place the two CLIs genuinely differ in this feature: Docker
    /// reports rootlessness as an entry in a list of security options, Podman as
    /// a boolean of its own.
    fn info_format(self) -> &'static str {
        match self {
            Self::Docker => "{{.ServerVersion}}|{{.SecurityOptions}}",
            Self::Podman => "{{.Version.Version}}|{{.Host.Security.Rootless}}",
        }
    }

    fn install_fix(self) -> &'static str {
        match self {
            Self::Docker => {
                "install Docker Engine or Docker Desktop: https://docs.docker.com/get-docker/"
            }
            Self::Podman => "install Podman: https://podman.io/docs/installation",
        }
    }

    /// What to do about an engine that is installed and not answering.
    fn unreachable_fix(self) -> &'static str {
        match self {
            Self::Docker => {
                "start the Docker daemon (open Docker Desktop, or: sudo systemctl start docker)"
            }
            Self::Podman => {
                "start the Podman service (podman machine start on macOS/Windows, or check \
                 'podman info')"
            }
        }
    }
}

impl std::fmt::Display for ContainerEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.program())
    }
}

/// What the probe learned about one engine, cached alongside its availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDetails {
    pub availability: Availability,
    /// The absolute path the probe resolved and vetted, or `None` when the
    /// engine is unusable. This — never the bare name — is what is executed.
    pub program: Option<String>,
    /// The engine's own version string, for the picker's detail line.
    pub version: Option<String>,
    /// Whether the engine runs without a root-owned daemon socket. Decides how a
    /// container is given the host user's identity, which is what makes a bind
    /// mount writable from inside.
    pub rootless: bool,
    /// `uid:gid` of the user friring is running as, when the host could be
    /// asked.
    ///
    /// A container that writes a bind-mounted repository has to do it as this
    /// user or the files it creates are owned by somebody else — and the egress
    /// socket friring binds `0o600` is only connectable by it. Under a rootful
    /// engine that is `--user`; under a rootless one the mapping already does
    /// it and passing `--user` would break it (see
    /// [`EngineDetails::userns_keep_id`]).
    pub user: Option<String>,
}

impl EngineDetails {
    /// Whether the container needs `--userns=keep-id` to see the host user's
    /// uid as itself.
    ///
    /// Rootless Podman maps the host user to *container root* by default, so
    /// bind-mounted files show up owned by `nobody` and `--user <host uid>`
    /// names a sub-uid rather than the user. `keep-id` maps the host user to the
    /// same uid inside, which is what makes an identical-path bind mount
    /// writable and the `0o600` proxy socket connectable with no widening at
    /// all. Rootless Docker does the same mapping without offering the flag, so
    /// there the container's root *is* the unprivileged host user.
    pub fn userns_keep_id(&self, engine: ContainerEngine) -> bool {
        self.rootless && engine == ContainerEngine::Podman
    }

    /// The `--user` argument for a `run`, or `None` when the engine's own
    /// mapping already gives the container the host user's identity — which is
    /// every rootless engine, where naming a uid would name a *sub*-uid.
    pub fn run_as_user(&self) -> Option<&str> {
        if self.rootless {
            return None;
        }
        self.user.as_deref()
    }
}

/// Ask `host` about `engine`: is it installed, is it answering, and who would a
/// container run as.
///
/// Every question goes through [`ProbeHost`], so this is testable against a
/// machine with nothing installed — and works unchanged against an SSH host,
/// where the engine that matters is the remote one.
pub fn probe(engine: ContainerEngine, host: &dyn ProbeHost) -> EngineDetails {
    let unavailable = |availability| EngineDetails {
        availability,
        program: None,
        version: None,
        rootless: false,
        user: None,
    };

    // Windows is a supported host for these two and only these two: it is the
    // only isolation a native Windows binary has (`docs/SANDBOX.md` §Backend
    // catalogue), so an unknown platform is the only one refused outright.
    if detect_platform(host) == HostPlatform::Unknown {
        return unavailable(Availability::unavailable(format!(
            "friring cannot identify this host, so it will not offer {engine} on it"
        )));
    }

    let Some(program) = host.which(engine.program()) else {
        return unavailable(Availability::needs_fix(
            format!("{engine} is not installed"),
            engine.install_fix(),
        ));
    };
    // The CLI is what asks for the isolation. A copy inside a directory the
    // sandboxed agent can write is a boundary the sandboxed agent chooses, and
    // falling back to the next `PATH` entry would still be running whatever an
    // attacker arranged to be found.
    //
    // Judged on both spellings: a name on `PATH` under a system prefix that is
    // really a symlink into a writable one is the obvious way past a check that
    // only reads the name. A path this friring cannot resolve — a remote host's,
    // which is not on this filesystem — leaves the literal check standing rather
    // than refusing an engine over a question it could not ask.
    let home = host.home();
    let resolved = dirs::canonical(&program).filter(|resolved| *resolved != program);
    let planted = [Some(program.clone()), resolved]
        .into_iter()
        .flatten()
        .find_map(|path| dirs::rewritable_root(&path, home.as_deref()).map(|root| (path, root)));
    if let Some((path, root)) = planted {
        let where_from = if path == program {
            format!("'{program}'")
        } else {
            format!("'{program}' and from there to '{path}'")
        };
        return unavailable(Availability::needs_fix(
            format!(
                "{engine} resolves to {where_from}, inside '{root}' — a sandboxed agent could \
                 replace it and the next launch would run whatever it planted"
            ),
            format!("install {engine} system-wide and take the writable copy off PATH"),
        ));
    }

    let output = match host.run(&program, &["info", "--format", engine.info_format()]) {
        Ok(output) => output,
        Err(detail) => {
            return unavailable(Availability::needs_fix(detail, engine.unreachable_fix()))
        }
    };
    if !output.ok() {
        // The engine's own first line is the actionable half ("Cannot connect to
        // the Docker daemon…", "Cannot connect to Podman"), so it is quoted
        // rather than replaced.
        let reason = output
            .stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("the engine did not answer 'info'")
            .trim()
            .to_string();
        return unavailable(Availability::needs_fix(reason, engine.unreachable_fix()));
    }

    let (version, rootless) = parse_info(output.trimmed());
    let user = probe_user(host);
    let detail = match (&version, rootless) {
        (Some(version), true) => format!("{engine} {version} (rootless)"),
        (Some(version), false) => format!("{engine} {version}"),
        (None, true) => format!("{engine} (rootless)"),
        (None, false) => engine.to_string(),
    };
    EngineDetails {
        availability: Availability::available(detail),
        program: Some(program),
        version,
        rootless,
        user,
    }
}

/// Split `version|rootless` as either engine renders it.
///
/// Docker prints its security options as a list, so the rootless tell is the
/// substring `rootless` in `[name=seccomp,… name=rootless]`; Podman prints the
/// boolean itself. Both are read leniently: an unparseable half costs a detail
/// in the picker, never the backend.
fn parse_info(raw: &str) -> (Option<String>, bool) {
    let (version, rest) = raw.split_once('|').unwrap_or((raw, ""));
    let version = version.trim();
    let version = (!version.is_empty() && version != "<no value>").then(|| version.to_string());
    let rest = rest.trim().to_ascii_lowercase();
    (version, rest.contains("rootless") || rest == "true")
}

/// `uid:gid` of the user friring runs as.
///
/// Asked of the host rather than read from the process, because the host that
/// matters is the one the engine runs on — and because it keeps this module
/// free of a platform-specific dependency for one integer. A host with no `id`
/// (Windows) answers `None`, and the container then runs as whatever the image
/// declares.
fn probe_user(host: &dyn ProbeHost) -> Option<String> {
    let read = |flag: &str| {
        host.run("id", &[flag])
            .ok()
            .filter(|output| output.ok())
            .map(|output| output.trimmed().to_string())
            .filter(|value| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()))
    };
    Some(format!("{}:{}", read("-u")?, read("-g")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::probe::{ProbeOutput, StubHost};

    /// A Linux host with `engine` installed and answering.
    fn host_with(engine: ContainerEngine, info: &str) -> StubHost {
        StubHost::new()
            .with_home("/home/u")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary(engine.program())
            .with_command("id -u", ProbeOutput::success("1000\n"))
            .with_command("id -g", ProbeOutput::success("1000\n"))
            .with_command(
                &format!("/usr/bin/{} info --format {}", engine, engine.info_format()),
                ProbeOutput::success(info),
            )
    }

    #[test]
    fn a_rootful_docker_is_available_and_runs_as_the_host_user() {
        let details = probe(
            ContainerEngine::Docker,
            &host_with(
                ContainerEngine::Docker,
                "27.1.1|[name=seccomp,profile=builtin]\n",
            ),
        );
        assert!(details.availability.is_available());
        assert_eq!(details.availability.message(), "docker 27.1.1");
        assert_eq!(details.program.as_deref(), Some("/usr/bin/docker"));
        assert!(!details.rootless);
        // Rootful: the container is given the host user's numeric identity, so
        // an identical-path bind mount is writable and the proxy socket is
        // connectable without widening its mode.
        assert_eq!(details.run_as_user(), Some("1000:1000"));
        assert!(!details.userns_keep_id(ContainerEngine::Docker));
    }

    #[test]
    fn a_rootless_podman_maps_the_user_instead_of_naming_it() {
        let details = probe(
            ContainerEngine::Podman,
            &host_with(ContainerEngine::Podman, "5.2.2|true\n"),
        );
        assert!(details.availability.is_available());
        assert_eq!(details.availability.message(), "podman 5.2.2 (rootless)");
        assert!(details.rootless);
        // `--user` would name a sub-uid under a rootless mapping; `keep-id` is
        // what makes the host user mean itself inside.
        assert_eq!(details.run_as_user(), None);
        assert!(details.userns_keep_id(ContainerEngine::Podman));
        // Rootless docker has no such flag: its container root already *is* the
        // unprivileged host user.
        let docker = probe(
            ContainerEngine::Docker,
            &host_with(
                ContainerEngine::Docker,
                "27.1.1|[name=seccomp name=rootless]\n",
            ),
        );
        assert!(docker.rootless);
        assert!(!docker.userns_keep_id(ContainerEngine::Docker));
    }

    #[test]
    fn a_missing_engine_and_a_stopped_one_both_say_what_to_do() {
        let bare = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n");
        let details = probe(ContainerEngine::Docker, &bare);
        assert!(!details.availability.is_available());
        assert!(details.availability.message().contains("not installed"));
        assert!(details.program.is_none());

        let stopped = StubHost::new()
            .with_home("/home/u")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("docker")
            .with_command(
                &format!(
                    "/usr/bin/docker info --format {}",
                    ContainerEngine::Docker.info_format()
                ),
                ProbeOutput::failure(
                    1,
                    "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the \
                     docker daemon running?\n",
                ),
            );
        let message = probe(ContainerEngine::Docker, &stopped)
            .availability
            .message();
        assert!(
            message.contains("Cannot connect to the Docker daemon"),
            "{message}"
        );
        assert!(message.contains("start the Docker daemon"), "{message}");
    }

    /// The engine CLI is the thing that applies the isolation, so a copy the
    /// sandbox could rewrite is refused exactly as a rewritable `bwrap` is.
    #[test]
    fn an_engine_binary_the_sandbox_could_replace_is_refused() {
        let host = StubHost::new()
            .with_home("/home/u")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary_at("podman", "/home/u/.local/bin/podman");
        let details = probe(ContainerEngine::Podman, &host);
        assert!(!details.availability.is_available());
        let message = details.availability.message();
        assert!(message.contains("/home/u/.local/bin/podman"), "{message}");
        assert!(message.contains("could replace it"), "{message}");
    }

    #[test]
    fn info_is_read_leniently() {
        assert_eq!(
            parse_info("27.1.1|[name=rootless]"),
            (Some("27.1.1".to_string()), true)
        );
        assert_eq!(
            parse_info("5.2.2|false"),
            (Some("5.2.2".to_string()), false)
        );
        assert_eq!(parse_info("<no value>|"), (None, false));
        assert_eq!(parse_info(""), (None, false));
    }
}
