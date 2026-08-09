//! Host inspection, injected so probing is testable without the tool installed.
//!
//! Every question a backend asks about its host goes through [`ProbeHost`]:
//! "is `bwrap` on `PATH`", "what does `/proc/sys/user/max_user_namespaces`
//! say", "what does `bwrap --version` print". Tests answer with the
//! test-only `StubHost`; production answers with [`LocalProbeHost`], or with
//! [`RemoteProbeHost`] when the agent runs on an SSH host or in a WSL distro —
//! a sandbox has to be probed where it will run, not where the TUI runs.
//!
//! The trait is synchronous. A remote probe therefore costs a round trip and
//! must not be called from the render path; backends cache their answer (see
//! [`crate::sandbox::backend::SandboxBackend::probe`]).

use std::process::Command;

use crate::session::{HostDef, HostKind};
use crate::shell::{posix_quote, ssh_command, wsl_command};

/// What one probe command produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeOutput {
    /// Exit status, or `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl ProbeOutput {
    /// A successful run with this stdout — the common case in tests.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failed run with this stderr, which is where every actionable probe
    /// message comes from.
    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Trimmed stdout — every probe wants the first line without its newline.
    pub fn trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// The machine a sandbox would run on, reduced to the four questions the
/// backends ask of it.
///
/// `Send + Sync` so a backend holding one can be shared across the async
/// runtime.
pub trait ProbeHost: Send + Sync {
    /// Where `program` resolves on the host's `PATH`, as an absolute path.
    ///
    /// The *path*, not the fact, because a backend that re-runs a bare name at
    /// launch lets whatever the tmux server's `PATH` points at apply the policy.
    /// A sandboxed agent with a writable home plants `~/.local/bin/bwrap` and
    /// the next launch runs it, unwrapped, as the host user. Seatbelt has never
    /// had the hole — it names `/usr/bin/sandbox-exec` outright — and
    /// [`crate::sandbox::bwrap::BwrapBackend`] closes it by resolving once, at
    /// probe time, and vetting the answer.
    fn which(&self, program: &str) -> Option<String>;

    /// The home directory of the user a sandbox would run as, when the host can
    /// be asked. `None` is "could not tell", not "there is none".
    ///
    /// Only used to vet where a sandbox tool lives: a `bwrap` under the home is
    /// a `bwrap` the sandboxed agent can rewrite.
    fn home(&self) -> Option<String>;

    /// Whether `path` exists on the host. Used to decide whether a secret is
    /// even there to hide — a bind-mount backend cannot overmount a path that
    /// does not exist, because the mount point would have to be created inside
    /// an already read-only root.
    fn path_exists(&self, path: &str) -> bool;

    /// Read a small text file (`/proc/sys/...`), `None` when it is absent or
    /// unreadable.
    fn read_file(&self, path: &str) -> Option<String>;

    /// Run `program args…` to completion and capture its output. `Err` means
    /// the process could not be started at all.
    fn run(&self, program: &str, args: &[&str]) -> Result<ProbeOutput, String>;
}

/// The kind of machine a sandbox would run on, as far as backend selection
/// cares. Distinguishes exactly what `docs/SANDBOX.md` §Backend selection
/// keys its ladder on and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    /// macOS. `apple_silicon` and `major` decide whether `apple-container` —
    /// Apple Silicon only, and macOS 26+ for isolated networks — is a rung.
    MacOs {
        apple_silicon: bool,
        major: u32,
    },
    Linux,
    /// A WSL2 distro. Linux with one decisive difference: all distros share a
    /// utility VM, a kernel and a network namespace, so per-sandbox egress
    /// control has to come from `bwrap` *inside* the distro.
    WslDistro,
    /// A native Windows binary, with no Unix userland to sandbox with.
    Windows,
    /// Nothing identifiable answered. Kept rather than guessed at: the ladder
    /// offers no rungs and says so.
    Unknown,
}

impl HostPlatform {
    /// The label the picker shows next to a rejected rung.
    pub fn label(self) -> String {
        match self {
            Self::MacOs {
                apple_silicon,
                major,
            } => {
                let cpu = if apple_silicon {
                    "Apple Silicon"
                } else {
                    "Intel"
                };
                format!("macOS {major} ({cpu})")
            }
            Self::Linux => "Linux".to_string(),
            Self::WslDistro => "WSL2 distro".to_string(),
            Self::Windows => "Windows".to_string(),
            Self::Unknown => "unknown host".to_string(),
        }
    }
}

/// Identify `host`.
///
/// Deliberately probes rather than trusting `cfg!`: the host a sandbox runs on
/// is not necessarily the host friring runs on, and the WSL/Linux distinction
/// cannot be made at compile time at all (a distro *is* Linux — the tell is the
/// kernel release string, which Microsoft's kernel stamps).
pub fn detect_platform(host: &dyn ProbeHost) -> HostPlatform {
    let Some(kernel) = host
        .run("uname", &["-s"])
        .ok()
        .filter(ProbeOutput::ok)
        .map(|o| o.trimmed().to_string())
    else {
        // No `uname` at all: either a native Windows host or something friring
        // has no vocabulary for. `cmd.exe` settles it.
        return if host.which("cmd.exe").is_some() || host.which("powershell.exe").is_some() {
            HostPlatform::Windows
        } else {
            HostPlatform::Unknown
        };
    };

    match kernel.as_str() {
        "Darwin" => HostPlatform::MacOs {
            apple_silicon: host
                .run("uname", &["-m"])
                .ok()
                .filter(ProbeOutput::ok)
                .is_some_and(|o| o.trimmed() == "arm64"),
            major: host
                .run("sw_vers", &["-productVersion"])
                .ok()
                .filter(ProbeOutput::ok)
                .and_then(|o| o.trimmed().split('.').next()?.parse().ok())
                .unwrap_or(0),
        },
        "Linux" => {
            // Microsoft's kernel release carries "microsoft" (WSL2) or "WSL"
            // (WSL1); the file is present on every Linux and costs one read.
            let release = host
                .read_file("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if release.contains("microsoft") || release.contains("wsl") {
                HostPlatform::WslDistro
            } else {
                HostPlatform::Linux
            }
        }
        _ => HostPlatform::Unknown,
    }
}

/// The machine friring itself is running on.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalProbeHost;

impl ProbeHost for LocalProbeHost {
    /// `crate::paths::which_on_path` answers the yes/no question; a sandbox
    /// needs the path itself, so the scan is repeated here rather than the
    /// shared helper's contract widened for one caller.
    fn which(&self, program: &str) -> Option<String> {
        let named = std::path::Path::new(program);
        if named.components().count() > 1 {
            // Already a path: it is what it is, and a relative one is refused
            // rather than resolved against a working directory nobody pinned.
            return (named.is_absolute() && named.exists()).then(|| program.to_string());
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.exists())
            .map(|candidate| candidate.display().to_string())
    }

    fn home(&self) -> Option<String> {
        crate::paths::home_dir().map(|h| h.display().to_string())
    }

    fn path_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn read_file(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<ProbeOutput, String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| format!("{program}: {e}"))?;
        Ok(ProbeOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// An off-local machine: an SSH host or a local WSL distro.
///
/// Every question becomes a one-line POSIX script run through the host's
/// launcher, so the two transports differ only in quoting — the same split
/// `git`'s remote helpers make: `ssh` space-joins its trailing arguments and
/// the remote login shell re-splits them (so the script is POSIX-quoted),
/// while `wsl.exe --exec` hands argv over verbatim (so it must **not** be).
#[derive(Debug, Clone)]
pub struct RemoteProbeHost {
    host: HostDef,
}

impl RemoteProbeHost {
    pub fn new(host: HostDef) -> Self {
        Self { host }
    }

    /// `sh -c <script>` on the host, quoted for whichever launcher applies.
    fn shell(&self, script: &str) -> Command {
        match self.host.kind {
            HostKind::Ssh => {
                let mut cmd = ssh_command(&self.host.destination, &self.host.ssh_opts);
                cmd.arg(posix_quote("sh"))
                    .arg(posix_quote("-c"))
                    .arg(posix_quote(script));
                cmd
            }
            HostKind::Wsl => {
                let mut cmd = wsl_command(&self.host.distro_name());
                cmd.arg("-e").arg("sh").arg("-c").arg(script);
                cmd
            }
        }
    }

    fn script(&self, script: &str) -> Result<ProbeOutput, String> {
        let output = self
            .shell(script)
            .output()
            .map_err(|e| format!("{}: {e}", self.host.name))?;
        Ok(ProbeOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

impl ProbeHost for RemoteProbeHost {
    fn which(&self, program: &str) -> Option<String> {
        // `command -v` prints the resolved path for anything on `PATH`; a
        // builtin or an alias prints something that is not one, and is refused
        // rather than handed to a backend as if it were a binary.
        self.script(&format!("command -v {}", posix_quote(program)))
            .ok()
            .filter(ProbeOutput::ok)
            .map(|o| o.trimmed().to_string())
            .filter(|resolved| resolved.starts_with('/'))
    }

    fn home(&self) -> Option<String> {
        self.script("printf %s \"$HOME\"")
            .ok()
            .filter(ProbeOutput::ok)
            .map(|o| o.stdout.trim().to_string())
            .filter(|home| home.starts_with('/'))
    }

    fn path_exists(&self, path: &str) -> bool {
        self.script(&format!("test -e {}", posix_quote(path)))
            .is_ok_and(|o| o.ok())
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.script(&format!("cat {}", posix_quote(path)))
            .ok()
            .filter(ProbeOutput::ok)
            .map(|o| o.stdout)
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<ProbeOutput, String> {
        let mut script = posix_quote(program);
        for arg in args {
            script.push(' ');
            script.push_str(&posix_quote(arg));
        }
        self.script(&script)
    }
}

/// A host whose every answer is scripted, for tests.
///
/// Nothing here touches the real machine: an unlisted binary is absent, an
/// unlisted file is missing, and an unscripted command fails to start. That
/// makes a probe test state exactly the host it is describing, and makes it
/// impossible for a developer's own installed tools to change the result.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct StubHost {
    /// `(program, resolved path)` — the path matters to the backends, so the
    /// stub answers with one rather than with a bare yes.
    on_path: Vec<(String, String)>,
    existing: Vec<String>,
    files: Vec<(String, String)>,
    commands: Vec<(String, ProbeOutput)>,
    home: Option<String>,
}

#[cfg(test)]
impl StubHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pretend `program` is on `PATH`, in the system location a package manager
    /// would put it.
    pub fn with_binary(mut self, program: &str) -> Self {
        self.on_path
            .push((program.to_string(), format!("/usr/bin/{program}")));
        self
    }

    /// Pretend `program` is on `PATH` at exactly `path` — the shape a test of
    /// "where did this binary come from?" needs.
    pub fn with_binary_at(mut self, program: &str, path: &str) -> Self {
        self.on_path.push((program.to_string(), path.to_string()));
        self
    }

    /// Pretend the host's home directory is `home`.
    pub fn with_home(mut self, home: &str) -> Self {
        self.home = Some(home.to_string());
        self
    }

    /// Pretend `path` exists.
    pub fn with_path(mut self, path: &str) -> Self {
        self.existing.push(path.to_string());
        self
    }

    /// Pretend `path` holds `contents`.
    pub fn with_file(mut self, path: &str, contents: &str) -> Self {
        self.files.push((path.to_string(), contents.to_string()));
        self
    }

    /// Answer `program args…` (matched on the space-joined command line) with
    /// `output`. Implies the program is on `PATH`.
    pub fn with_command(mut self, command_line: &str, output: ProbeOutput) -> Self {
        // A command line naming an absolute program is a *resolved* one, so it
        // adds nothing to `PATH`.
        if let Some(program) = command_line
            .split_whitespace()
            .next()
            .filter(|p| !p.contains('/'))
        {
            if !self.on_path.iter().any(|(p, _)| p == program) {
                self.on_path
                    .push((program.to_string(), format!("/usr/bin/{program}")));
            }
        }
        self.commands.push((command_line.to_string(), output));
        self
    }

    /// A Linux host with a working bubblewrap of `version`, installed where a
    /// package manager puts it.
    pub fn linux_with_bwrap(version: &str) -> Self {
        Self::new()
            .with_home("/home/u")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_command(
                "bwrap --version",
                ProbeOutput::success(format!("bubblewrap {version}\n")),
            )
            .with_command(
                "/usr/bin/bwrap --version",
                ProbeOutput::success(format!("bubblewrap {version}\n")),
            )
            .with_command("bwrap --ro-bind / / true", ProbeOutput::success(""))
            .with_command(
                "/usr/bin/bwrap --ro-bind / / true",
                ProbeOutput::success(""),
            )
    }

    /// A macOS host with `sandbox-exec` present.
    pub fn macos(major: u32, apple_silicon: bool) -> Self {
        Self::new()
            .with_home("/Users/u")
            .with_command("uname -s", ProbeOutput::success("Darwin\n"))
            .with_command(
                "uname -m",
                ProbeOutput::success(if apple_silicon { "arm64\n" } else { "x86_64\n" }),
            )
            .with_command(
                "sw_vers -productVersion",
                ProbeOutput::success(format!("{major}.1\n")),
            )
            .with_path("/usr/bin/sandbox-exec")
            .with_binary("sandbox-exec")
    }
}

#[cfg(test)]
impl ProbeHost for StubHost {
    fn which(&self, program: &str) -> Option<String> {
        self.on_path
            .iter()
            .find(|(p, _)| p == program)
            .map(|(_, path)| path.clone())
    }

    fn home(&self) -> Option<String> {
        self.home.clone()
    }

    fn path_exists(&self, path: &str) -> bool {
        self.existing.iter().any(|p| p == path)
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.clone())
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<ProbeOutput, String> {
        let line = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };
        self.commands
            .iter()
            .find(|(c, _)| c == &line)
            .map(|(_, o)| o.clone())
            .ok_or_else(|| format!("{line}: No such file or directory"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_macos_with_cpu_and_major_version() {
        let host = StubHost::macos(26, true);
        assert_eq!(
            detect_platform(&host),
            HostPlatform::MacOs {
                apple_silicon: true,
                major: 26,
            }
        );
        assert_eq!(
            detect_platform(&StubHost::macos(15, false)),
            HostPlatform::MacOs {
                apple_silicon: false,
                major: 15,
            }
        );
    }

    #[test]
    fn detects_linux_and_separates_wsl_by_kernel_release() {
        assert_eq!(
            detect_platform(&StubHost::linux_with_bwrap("0.11.0")),
            HostPlatform::Linux
        );
        let wsl = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file(
                "/proc/sys/kernel/osrelease",
                "5.15.153.1-microsoft-standard-WSL2\n",
            );
        assert_eq!(detect_platform(&wsl), HostPlatform::WslDistro);
    }

    #[test]
    fn a_host_without_uname_is_windows_only_when_it_proves_it() {
        let windows = StubHost::new().with_binary("cmd.exe");
        assert_eq!(detect_platform(&windows), HostPlatform::Windows);
        // Nothing answered at all: not guessed at.
        assert_eq!(detect_platform(&StubHost::new()), HostPlatform::Unknown);
    }

    #[test]
    fn an_unrecognised_kernel_is_unknown_rather_than_assumed() {
        let bsd = StubHost::new().with_command("uname -s", ProbeOutput::success("FreeBSD\n"));
        assert_eq!(detect_platform(&bsd), HostPlatform::Unknown);
        assert_eq!(HostPlatform::Unknown.label(), "unknown host");
        assert_eq!(
            HostPlatform::MacOs {
                apple_silicon: true,
                major: 26
            }
            .label(),
            "macOS 26 (Apple Silicon)"
        );
    }

    #[test]
    fn stub_answers_only_what_it_was_told() {
        let host = StubHost::new()
            .with_binary("bwrap")
            .with_file("/proc/sys/user/max_user_namespaces", "0\n")
            .with_path("/usr/bin/sandbox-exec");
        // The stub answers with a path, because that is what a backend must
        // pin: a bare name is re-resolved through whatever `PATH` is live.
        assert_eq!(host.which("bwrap").as_deref(), Some("/usr/bin/bwrap"));
        assert!(host.which("docker").is_none());
        assert!(host.path_exists("/usr/bin/sandbox-exec"));
        assert!(!host.path_exists("/usr/bin/bwrap"));
        assert_eq!(
            host.read_file("/proc/sys/user/max_user_namespaces")
                .as_deref(),
            Some("0\n")
        );
        assert!(host.read_file("/etc/passwd").is_none());
        assert!(host.run("docker", &["info"]).is_err());
    }

    #[test]
    fn remote_probe_quotes_per_transport() {
        let ssh = RemoteProbeHost::new(HostDef {
            name: "devbox".into(),
            destination: "me@devbox".into(),
            ..Default::default()
        });
        let args: Vec<String> = ssh
            .shell("test -e /home/me/.ssh")
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // ssh re-splits its trailing tokens in the remote login shell, so the
        // script survives only as one quoted word.
        assert!(args.iter().any(|a| a == "'test -e /home/me/.ssh'"));

        let wsl = RemoteProbeHost::new(HostDef::wsl("Ubuntu"));
        let cmd = wsl.shell("test -e /home/me/.ssh");
        assert_eq!(cmd.get_program().to_string_lossy(), "wsl.exe");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // `--exec` passes argv verbatim: pre-quoting would arrive literally.
        assert!(args.iter().any(|a| a == "test -e /home/me/.ssh"));
        assert!(args.iter().any(|a| a == "-e"));
    }
}
