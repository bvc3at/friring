//! Transport seam for the tmux backend.
//!
//! The tmux control-mode protocol is identical whether tmux runs on the local
//! machine, on a remote host reached over SSH, or inside a sandbox place (see
//! [`crate::agent::control_mode`]). The *only* thing that differs is how the
//! `tmux` process is launched: a bare `Command::new("tmux")` locally,
//! `ssh <dest> tmux …` remotely, `<engine> exec -i <container> tmux …` for a
//! place.
//!
//! [`TmuxTransport`] captures exactly that difference and nothing else. It builds
//! [`Command`]s; it never touches I/O, threading, or the protocol.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Result};

use crate::session::SANDBOX_BACKEND_PREFIX;
use crate::shell::{posix_quote, ssh_command, wsl_command};

/// The local multiplexer binary: `psmux` on Windows (a native, drop-in tmux
/// replacement with an identical control-mode wire protocol), `tmux` elsewhere.
/// psmux also installs `tmux`/`pmux` aliases, but `psmux` is the canonical name.
pub const DEFAULT_MUX: &str = if cfg!(windows) { "psmux" } else { "tmux" };

/// The multiplexer inside a sandbox place. Always real `tmux`: a place is a
/// Linux container running an image that ships tmux, so — unlike
/// [`DEFAULT_MUX`], which follows the *host* OS — a native-Windows friring
/// driving a place still speaks tmux, and keeps the `send-keys -H` hex
/// keystroke path rather than psmux's fallback.
const PLACE_MUX: &str = "tmux";

/// Longest container reference this will build a command from. Engine ids are
/// 64 hex characters and engine names are short; anything longer is not one.
const MAX_CONTAINER_REF: usize = 128;

/// Characters a container reference may carry after the first: the container
/// engines' own name grammar (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`), which the ids they
/// mint also satisfy.
fn is_container_ref_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// A running sandbox **place**, reduced to what reaching its tmux needs
/// (ADR-26).
///
/// Built once from the profile's live instance
/// ([`crate::sandbox::SandboxInstance`]) plus the engine path the backend
/// resolved at probe time, then handed to [`TmuxTransport::Sandbox`] and to the
/// `for_place` constructors in [`crate::agent::tmux`]. The fields are private
/// because [`Place::new`] is what vets them, and the transport builds a command
/// line from them with no further checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    engine: String,
    container: String,
    profile: String,
}

impl Place {
    /// Vet the three values a place is addressed by.
    ///
    /// `engine` must be **absolute**. The engine runs on the *host*, and its
    /// socket is commonly root-equivalent, so re-resolving a bare `docker`
    /// through whatever `PATH` friring inherited would let anything that can
    /// write a `PATH` directory choose what starts the boundary — the hole
    /// [`crate::sandbox::probe::ProbeHost::which`] exists to close for `bwrap`,
    /// and the reason `/usr/bin/sandbox-exec` is named outright.
    ///
    /// `container` must be a container name or id. It reaches the command line
    /// as the argument right after the engine's own flags, so a value that
    /// could pass for one (`-i`, `--rm`) is refused rather than trusted to the
    /// engine's parser, and so is anything the engines could not have minted.
    ///
    /// `profile` only names the backend (`sandbox:<profile>`), which is
    /// persisted in `sessions.backend_type` and compared as a string, so it
    /// just has to be a single printable word.
    pub fn new(engine: &str, container: &str, profile: &str) -> Result<Self> {
        if !Path::new(engine).is_absolute() {
            bail!(
                "sandbox engine {engine:?} must be an absolute path: resolve it once where the \
                 backend was probed, so an inherited PATH cannot choose what runs the place"
            );
        }
        let container_ok = !container.is_empty()
            && container.len() <= MAX_CONTAINER_REF
            && container.starts_with(|c: char| c.is_ascii_alphanumeric())
            && container.chars().all(is_container_ref_char);
        if !container_ok {
            bail!(
                "sandbox container reference {container:?} is not a container name or id \
                 (a letter or digit, then letters, digits, `_`, `.` or `-`)"
            );
        }
        if profile.is_empty() || profile.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("sandbox profile name {profile:?} cannot name a backend");
        }
        Ok(Self {
            engine: engine.to_string(),
            container: container.to_string(),
            profile: profile.to_string(),
        })
    }

    /// The absolute engine binary (`docker`, `podman`, Apple's `container`).
    pub fn engine(&self) -> &str {
        &self.engine
    }

    /// The engine's own handle for the running place.
    pub fn container(&self) -> &str {
        &self.container
    }

    /// The sandbox profile the place was built for.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The backend name a place-backed session persists in `backend_type`:
    /// `sandbox:<profile>`, mirroring `ssh:<host>` / `wsl:<distro>`.
    pub fn backend_name(&self) -> String {
        format!("{SANDBOX_BACKEND_PREFIX}{}", self.profile)
    }

    /// Build `<engine> exec -i <container> tmux -L <socket> <args…>`.
    ///
    /// Three deliberate differences from the SSH/WSL arms, each of which would
    /// be a silent breakage the other way round:
    ///
    /// - **No quoting.** `ssh` joins its remote argv into one string that the
    ///   remote login shell re-splits, which is why every token there is
    ///   [`posix_quote`]d. An engine `exec` takes an argv and `execve`s it: no
    ///   shell is involved, so a quoted token would arrive *with* its quotes.
    ///   `list-windows -F '#{pane_id}|…'` would then produce output whose first
    ///   field is `'%1` rather than `%1`, and discovery would silently find
    ///   nothing.
    /// - **`-i`, never `-t`.** `-i` keeps stdin open, which is what carries the
    ///   control-mode protocol. `-t` would allocate a pty for the exec'd
    ///   process, and the engine then translates its output (`\n` → `\r\n`) —
    ///   corrupting a protocol that is parsed line by line.
    /// - **No `-e`/`--env`.** The engine's argv is on the *host* process table.
    ///   Session environment reaches the place through the tmux window instead
    ///   (`new-window -e`, sent over the control-mode connection), so the
    ///   proxy credential never lands anywhere `/proc/<pid>/cmdline` can be
    ///   read from. That is also why identity env has to be set on the window:
    ///   `exec` gives the process the *image's* environment, not friring's, so
    ///   nothing is inherited from here.
    fn exec_command(&self, socket: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.engine);
        cmd.arg("exec").arg("-i").arg(&self.container);
        cmd.arg(PLACE_MUX).arg("-L").arg(socket).args(args);
        cmd
    }
}

/// How to launch the multiplexer for a backend: directly, wrapped in `ssh`,
/// inside a local WSL distro via `wsl.exe`, or inside a sandbox place via its
/// container engine.
#[derive(Debug, Clone)]
pub enum TmuxTransport {
    /// Run the multiplexer ([`DEFAULT_MUX`]) on the local machine.
    Local,
    /// Run the multiplexer on a remote host over SSH. `destination` is an ssh
    /// target (resolved via the user's `~/.ssh/config`); `ssh_opts` are extra
    /// flags (e.g. `-o ControlMaster=auto`) inserted before the destination;
    /// `mux` is the remote multiplexer binary (defaults to `tmux`, but a host in
    /// `hosts.toml` can set `multiplexer = "psmux"` for a Windows target).
    Ssh {
        destination: String,
        ssh_opts: Vec<String>,
        mux: String,
    },
    /// Run the multiplexer inside a local WSL distro via `wsl.exe -d <distro>`.
    /// This is the SSH variant minus the network: `wsl.exe` forwards the
    /// whitespace-free tokens used here to the in-distro shell like `ssh` does
    /// (see [`crate::shell::wsl_command`] for the exact forwarding model), so
    /// the same control-mode protocol and POSIX quoting apply. `mux` is the
    /// in-distro multiplexer binary (`tmux`).
    Wsl { distro: String, mux: String },
    /// Run the multiplexer **inside a sandbox place** — a container reached
    /// with `<engine> exec -i <container> tmux …`.
    ///
    /// ADR-26: a place backend *is* a transport rather than an argv wrap, so
    /// tmux runs in the place and everything above this seam — control mode,
    /// discovery, adoption, input, scrollback, the `tb-` window naming — is the
    /// SSH path's, unchanged. What differs is only the launch prefix, and the
    /// quoting it does *not* need (see `Place::exec_command`).
    ///
    /// A WSL-distro place is deliberately not this variant: it is reached by
    /// [`TmuxTransport::Wsl`], which already does exactly that job.
    Sandbox(Place),
}

/// Environment variables a tmux/psmux server reads to resolve a *nested*
/// client's default target. If friring is itself launched inside a tmux/psmux
/// pane, these leak into the multiplexer subcommands it spawns and make a bare
/// `-t <session>` resolve against the *outer* session instead of the friring
/// socket — on psmux this surfaces as `set-option -t friring` failing with
/// `no server running on 'friring__friring'` (psmux concatenates
/// `PSMUX_TARGET_SESSION = <socket>__<session>`). Stripping them makes friring's
/// explicit `-L <socket> -t <session>` always target its own server, whether the
/// host OS is Windows (psmux) or Unix (friring launched from inside tmux).
///
/// The list itself lives in the pure-data layer because a sandbox launch strips
/// the same variables for a different reason — see
/// [`crate::session::MUX_NESTING_ENV`].
const MUX_NESTING_ENV: &[&str] = crate::session::MUX_NESTING_ENV;

/// Remove the multiplexer-nesting env vars (see [`MUX_NESTING_ENV`]) from `cmd`
/// so a multiplexer subcommand never inherits an outer pane's target context.
pub(crate) fn strip_mux_nesting_env(cmd: &mut Command) {
    for var in MUX_NESTING_ENV {
        cmd.env_remove(var);
    }
}

impl TmuxTransport {
    /// The transport for a sandbox place, from its already-vetted [`Place`].
    pub fn sandbox(place: Place) -> Self {
        TmuxTransport::Sandbox(place)
    }

    /// Build a [`Command`] running `<mux> -L <socket> <args…>`, wrapped in `ssh`
    /// for the remote variant and in `<engine> exec` for a place.
    ///
    /// For the SSH and WSL variants the remote command tokens are re-split by
    /// the remote login shell, so each token is shell-escaped to survive intact.
    /// Simple tokens (the binary name, `-L`, the socket name) pass through
    /// unquoted. A place's engine takes an argv rather than a command string, so
    /// its tokens are passed through verbatim — quoting them would deliver the
    /// quotes (see `Place::exec_command`).
    ///
    /// Nesting env vars are stripped (see `strip_mux_nesting_env`) so the
    /// command targets friring's own server even when friring runs inside a pane.
    pub fn tmux_command(&self, socket: &str, args: &[&str]) -> Command {
        let mut cmd = match self {
            TmuxTransport::Local => {
                let mut cmd = Command::new(DEFAULT_MUX);
                cmd.arg("-L").arg(socket).args(args);
                cmd
            }
            TmuxTransport::Ssh {
                destination,
                ssh_opts,
                mux,
            } => Self::prefixed(ssh_command(destination, ssh_opts), mux, socket, args),
            TmuxTransport::Wsl { distro, mux } => {
                Self::prefixed(wsl_command(distro), mux, socket, args)
            }
            TmuxTransport::Sandbox(place) => place.exec_command(socket, args),
        };
        if self.forwards_stdin() {
            // No tmux command friring runs one-shot reads stdin — but every
            // launcher *forwards* it, and `Command::status()` inherits it. A
            // caller in the TUI process would hand `ssh` / `wsl.exe` /
            // `<engine> exec -i` the terminal ratatui is reading from, and the
            // launcher would eat keystrokes out from under it. The control-mode
            // client overrides this with its own pipe (`ControlMode::start`),
            // which is the one command that does read stdin.
            cmd.stdin(Stdio::null());
        }
        strip_mux_nesting_env(&mut cmd);
        cmd
    }

    /// Append `<mux> -L <socket> <args…>` to a launcher command (`ssh …` or
    /// `wsl.exe …`), POSIX-quoting each token so the host's login shell
    /// re-splits them intact. Shared by the SSH and WSL arms, which differ only
    /// in the launcher prefix.
    fn prefixed(mut cmd: Command, mux: &str, socket: &str, args: &[&str]) -> Command {
        cmd.arg(posix_quote(mux));
        cmd.arg(posix_quote("-L"));
        cmd.arg(posix_quote(socket));
        for a in args {
            cmd.arg(posix_quote(a));
        }
        cmd
    }

    /// Whether this transport reaches the multiplexer through a launch prefix
    /// (SSH, WSL or a sandbox place) rather than running it directly on the
    /// local machine.
    ///
    /// What this decides is where the *multiplexer* lives, and therefore which
    /// machine's shell paths are real: the local `$SHELL` means nothing to a
    /// tmux server in a container. It deliberately does **not** decide whether
    /// a window command needs a login shell — that is
    /// [`needs_login_shell`](Self::needs_login_shell), which a place answers
    /// the other way.
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            TmuxTransport::Ssh { .. } | TmuxTransport::Wsl { .. } | TmuxTransport::Sandbox(_)
        )
    }

    /// Whether a window command has to be wrapped in a **login** shell for the
    /// launched process to find its binary.
    ///
    /// True for SSH and WSL: agents are commonly installed under `~/.local/bin`,
    /// which only the user's login profile puts on `PATH`, and a non-login shell
    /// there leaves the pane dying instantly on "command not found".
    ///
    /// False for a place, and not by omission. `<engine> exec` runs the process
    /// with the **image's** environment, so the `PATH` the image author declared
    /// is already in force and there is no profile to source — while `sh -l`
    /// inside a container commonly *replaces* it, because `/etc/profile` on the
    /// mainstream base images assigns a fixed system `PATH`. Wrapping here would
    /// take away the one `PATH` that is guaranteed to name the agent.
    ///
    /// False for psmux hosts for a third reason: a Windows host has no
    /// `/bin/sh` to wrap with at all.
    pub fn needs_login_shell(&self) -> bool {
        match self {
            TmuxTransport::Local | TmuxTransport::Sandbox(_) => false,
            TmuxTransport::Ssh { .. } | TmuxTransport::Wsl { .. } => !self.uses_psmux(),
        }
    }

    /// Whether the launcher forwards friring's own stdin to the far end — true
    /// for every prefixed transport, false for a bare local `tmux`. See the use
    /// in [`tmux_command`](Self::tmux_command).
    fn forwards_stdin(&self) -> bool {
        !matches!(self, TmuxTransport::Local)
    }

    /// The place this transport reaches, or `None` for every other variant.
    pub fn place(&self) -> Option<&Place> {
        match self {
            TmuxTransport::Sandbox(place) => Some(place),
            _ => None,
        }
    }

    /// The multiplexer binary this transport launches: [`DEFAULT_MUX`] locally,
    /// the per-host `multiplexer` for an SSH host / WSL distro (`tmux` for a
    /// distro, so [`uses_psmux`](Self::uses_psmux) is `false` and the in-distro
    /// tmux keeps the `-H` hex keystroke path), or real `tmux` inside a place.
    pub fn mux(&self) -> &str {
        match self {
            TmuxTransport::Local => DEFAULT_MUX,
            TmuxTransport::Ssh { mux, .. } | TmuxTransport::Wsl { mux, .. } => mux,
            TmuxTransport::Sandbox(_) => PLACE_MUX,
        }
    }

    /// Whether the multiplexer is psmux (the native-Windows tmux clone). psmux
    /// lacks tmux's `send-keys -H` hex flag, so the keystroke-encoding path
    /// branches on this — see [`crate::agent::control_mode::send_keys_commands`].
    pub fn uses_psmux(&self) -> bool {
        self.mux() == "psmux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program_and_args(cmd: &Command) -> (String, Vec<String>) {
        let prog = cmd.get_program().to_string_lossy().into_owned();
        let args = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        (prog, args)
    }

    #[test]
    fn local_builds_bare_mux() {
        let t = TmuxTransport::Local;
        let cmd = t.tmux_command("friring", &["has-session", "-t", "friring"]);
        let (prog, args) = program_and_args(&cmd);
        assert_eq!(prog, DEFAULT_MUX);
        assert_eq!(args, ["-L", "friring", "has-session", "-t", "friring"]);
    }

    #[test]
    fn ssh_wraps_mux_with_opts_and_destination() {
        let t = TmuxTransport::Ssh {
            destination: "me@devbox".into(),
            ssh_opts: vec!["-o".into(), "ControlMaster=auto".into()],
            mux: "tmux".into(),
        };
        let cmd = t.tmux_command("friring", &["has-session", "-t", "friring"]);
        let (prog, args) = program_and_args(&cmd);
        assert_eq!(prog, "ssh");
        // User opts, then the always-appended fail-fast hardening
        // (crate::shell::SSH_HARDENING_OPTS), then the destination + remote cmd.
        let mut expected: Vec<String> = vec!["-o".into(), "ControlMaster=auto".into()];
        expected.extend(
            crate::shell::SSH_HARDENING_OPTS
                .iter()
                .map(|s| s.to_string()),
        );
        expected.extend(
            [
                "me@devbox",
                "tmux",
                "-L",
                "friring",
                "has-session",
                "-t",
                "friring",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        assert_eq!(args, expected);
    }

    #[test]
    fn ssh_honors_custom_multiplexer() {
        let t = TmuxTransport::Ssh {
            destination: "me@winbox".into(),
            ssh_opts: vec![],
            mux: "psmux".into(),
        };
        let cmd = t.tmux_command("friring", &["has-session"]);
        let (prog, args) = program_and_args(&cmd);
        assert_eq!(prog, "ssh");
        let mut expected: Vec<String> = crate::shell::SSH_HARDENING_OPTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.extend(
            ["me@winbox", "psmux", "-L", "friring", "has-session"]
                .iter()
                .map(|s| s.to_string()),
        );
        assert_eq!(args, expected);
    }

    #[test]
    fn wsl_wraps_mux_with_distro() {
        let t = TmuxTransport::Wsl {
            distro: "Ubuntu".into(),
            mux: "tmux".into(),
        };
        let cmd = t.tmux_command("friring", &["has-session", "-t", "friring"]);
        let (prog, args) = program_and_args(&cmd);
        assert_eq!(prog, "wsl.exe");
        // A Unix caller passes `--cd /` (see `shell::wsl_command`) so wsl.exe
        // doesn't inherit a caller cwd missing from — or mangled into — the
        // target distro.
        #[cfg(unix)]
        let prefix: &[&str] = &["-d", "Ubuntu", "--cd", "/"];
        #[cfg(not(unix))]
        let prefix: &[&str] = &["-d", "Ubuntu"];
        let expected: Vec<&str> = prefix
            .iter()
            .copied()
            .chain(["tmux", "-L", "friring", "has-session", "-t", "friring"])
            .collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn tmux_command_strips_nesting_env() {
        let cmd = TmuxTransport::Local.tmux_command("friring", &["has-session"]);
        // Removed vars surface in get_envs() as (key, None).
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for var in MUX_NESTING_ENV {
            assert!(
                removed.contains(&var.to_string()),
                "expected nesting env `{var}` to be removed"
            );
        }
    }

    // --- sandbox place (ADR-26: a place is a transport) ---
    //
    // Nothing here starts a container: every assertion is about the command
    // line friring *would* run, which is the whole of what this seam decides.

    // Unix-only with the place they address: a native Windows host is offered no
    // sandbox at all (`crate::sandbox::select::NATIVE_WINDOWS`), so a `Place` is
    // not a thing that exists there — and its engine path would not be absolute.
    /// A fabricated place. The engine path is a plausible install location and
    /// the container is a name friring's own `ensure` would mint; neither is
    /// touched by these tests.
    #[cfg(unix)]
    fn place() -> Place {
        Place::new("/usr/local/bin/docker", "friring-dev-1a2b3c", "dev").unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_builds_engine_exec_argv() {
        let t = TmuxTransport::sandbox(place());
        let cmd = t.tmux_command("friring", &["has-session", "-t", "friring"]);
        let (prog, args) = program_and_args(&cmd);
        assert_eq!(prog, "/usr/local/bin/docker");
        assert_eq!(
            args,
            [
                "exec",
                "-i",
                "friring-dev-1a2b3c",
                "tmux",
                "-L",
                "friring",
                "has-session",
                "-t",
                "friring",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn every_engine_takes_the_same_shape() {
        for engine in [
            "/usr/local/bin/docker",
            "/usr/bin/podman",
            "/usr/local/bin/container",
        ] {
            let t = TmuxTransport::sandbox(Place::new(engine, "ctr", "dev").unwrap());
            let (prog, args) = program_and_args(&t.tmux_command("friring", &["-V"]));
            assert_eq!(prog, engine);
            assert_eq!(args, ["exec", "-i", "ctr", "tmux", "-L", "friring", "-V"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_passes_tokens_through_verbatim() {
        // An engine `exec` takes an argv and never a shell, so the SSH arm's
        // POSIX quoting would be delivered *as quotes*. The discovery format
        // string is the case that fails silently: `'#{pane_id}|…'` would make
        // every parsed pane id start with a quote, and `discover()` would report
        // an empty server.
        let awkward = [
            "list-windows",
            "-F",
            "#{pane_id}|#{window_name}|#{pane_dead}",
            "-c",
            "/Users/me/My Repos/app",
        ];
        let sandbox = TmuxTransport::sandbox(place());
        let (_, args) = program_and_args(&sandbox.tmux_command("friring", &awkward));
        // `exec -i <container> tmux -L <socket>`, then the caller's own tokens.
        assert_eq!(&args[6..], awkward);

        // The SSH arm does the opposite, and must keep doing it.
        let ssh = TmuxTransport::Ssh {
            destination: "me@devbox".into(),
            ssh_opts: vec![],
            mux: "tmux".into(),
        };
        let (_, ssh_args) = program_and_args(&ssh.tmux_command("friring", &awkward));
        assert!(
            ssh_args.contains(&"'#{pane_id}|#{window_name}|#{pane_dead}'".to_string()),
            "{ssh_args:?}"
        );
        assert!(
            ssh_args.contains(&"'/Users/me/My Repos/app'".to_string()),
            "{ssh_args:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_never_carries_environment_in_argv() {
        // The engine's argv is on the *host* process table. Session environment
        // — which for a sandboxed launch includes the egress proxy's bearer
        // token — goes through the tmux window instead, over the control-mode
        // connection. A `-e`/`--env` growing into this prefix would undo that.
        let t = TmuxTransport::sandbox(place());
        let (_, args) = program_and_args(&t.tmux_command("friring", &["-V"]));
        assert!(!args.iter().any(|a| a == "-e" || a == "--env"), "{args:?}");
        // `-t` would give the exec'd process a pty, and the engine would then
        // translate `\n` to `\r\n` through a line-delimited protocol.
        assert!(!args.contains(&"-t".to_string()), "{args:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_place_is_addressed_by_values_that_were_vetted() {
        // The engine runs on the host with a root-equivalent socket: a bare
        // name would be re-resolved through whatever PATH friring inherited.
        assert!(Place::new("docker", "ctr", "dev").is_err());
        // The container reference lands right after the engine's own flags.
        for bad in ["", "-i", "--rm", "ctr; rm -rf /", "a b", "über"] {
            assert!(
                Place::new("/usr/bin/docker", bad, "dev").is_err(),
                "accepted {bad:?}"
            );
        }
        assert!(Place::new("/usr/bin/docker", &"a".repeat(129), "dev").is_err());
        // What the engines really mint: a name, and a 64-hex id.
        assert!(Place::new("/usr/bin/docker", "friring-dev.1_x", "dev").is_ok());
        assert!(Place::new("/usr/bin/docker", &"0f".repeat(32), "dev").is_ok());
        // The profile names the backend, so it has to be one word.
        assert!(Place::new("/usr/bin/docker", "ctr", "").is_err());
        assert!(Place::new("/usr/bin/docker", "ctr", "my profile").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_place_names_its_backend_like_a_host_does() {
        assert_eq!(place().backend_name(), "sandbox:dev");
        assert_eq!(place().engine(), "/usr/local/bin/docker");
        assert_eq!(place().container(), "friring-dev-1a2b3c");
        assert_eq!(place().profile(), "dev");
    }

    #[cfg(unix)]
    #[test]
    fn a_place_runs_tmux_whatever_the_host_runs() {
        // `DEFAULT_MUX` follows the *host* OS, so a native-Windows friring
        // would otherwise send `psmux` into a Linux container — and take the
        // psmux keystroke path for a server that has `send-keys -H`.
        let t = TmuxTransport::sandbox(place());
        assert_eq!(t.mux(), "tmux");
        assert!(!t.uses_psmux());
        assert_eq!(t.place().map(Place::container), Some("friring-dev-1a2b3c"));
        assert!(TmuxTransport::Local.place().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_place_is_reached_like_a_host_but_needs_no_login_shell() {
        let sandbox = TmuxTransport::sandbox(place());
        // Reached through a launch prefix: the local `$SHELL` means nothing to
        // a tmux server in a container.
        assert!(sandbox.is_remote());
        // …but its `PATH` comes from the image, and `sh -l` inside a container
        // commonly replaces it with `/etc/profile`'s.
        assert!(!sandbox.needs_login_shell());
        assert!(TmuxTransport::Ssh {
            destination: "h".into(),
            ssh_opts: vec![],
            mux: "tmux".into(),
        }
        .needs_login_shell());
        assert!(TmuxTransport::Wsl {
            distro: "Ubuntu".into(),
            mux: "tmux".into(),
        }
        .needs_login_shell());
        // A Windows SSH host has no `/bin/sh` to wrap with; local inherits the
        // user's interactive PATH already.
        assert!(!TmuxTransport::Ssh {
            destination: "h".into(),
            ssh_opts: vec![],
            mux: "psmux".into(),
        }
        .needs_login_shell());
        assert!(!TmuxTransport::Local.needs_login_shell());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_bare_local_mux_keeps_frirings_stdin() {
        // Every launcher forwards stdin, and `Command::status()` inherits it —
        // so a caller in the TUI process would hand the terminal ratatui reads
        // from to `ssh` / `wsl.exe` / `<engine> exec -i`.
        assert!(!TmuxTransport::Local.forwards_stdin());
        for t in [
            TmuxTransport::Ssh {
                destination: "h".into(),
                ssh_opts: vec![],
                mux: "tmux".into(),
            },
            TmuxTransport::Wsl {
                distro: "Ubuntu".into(),
                mux: "tmux".into(),
            },
            TmuxTransport::sandbox(place()),
        ] {
            assert!(t.forwards_stdin(), "{t:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn nesting_env_is_stripped_on_every_transport() {
        // A no-op for a place (`exec` gives the process the image's
        // environment, not friring's), but the contract is one contract.
        for t in [
            TmuxTransport::Local,
            TmuxTransport::Ssh {
                destination: "h".into(),
                ssh_opts: vec![],
                mux: "tmux".into(),
            },
            TmuxTransport::sandbox(place()),
        ] {
            let cmd = t.tmux_command("friring", &["has-session"]);
            let removed: Vec<String> = cmd
                .get_envs()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
            for var in MUX_NESTING_ENV {
                assert!(removed.contains(&var.to_string()), "{t:?} kept {var}");
            }
        }
    }

    #[test]
    fn uses_psmux_reflects_mux_binary() {
        assert_eq!(TmuxTransport::Local.uses_psmux(), cfg!(windows));
        assert!(TmuxTransport::Ssh {
            destination: "h".into(),
            ssh_opts: vec![],
            mux: "psmux".into(),
        }
        .uses_psmux());
        assert!(!TmuxTransport::Ssh {
            destination: "h".into(),
            ssh_opts: vec![],
            mux: "tmux".into(),
        }
        .uses_psmux());
        // A WSL distro runs Linux `tmux`, which supports the `-H` hex flag, so
        // it must NOT take the psmux keystroke-encoding path.
        let wsl = TmuxTransport::Wsl {
            distro: "Ubuntu".into(),
            mux: "tmux".into(),
        };
        assert_eq!(wsl.mux(), "tmux");
        assert!(!wsl.uses_psmux());
    }

    #[cfg(unix)]
    #[test]
    fn is_remote_reflects_variant() {
        assert!(!TmuxTransport::Local.is_remote());
        assert!(TmuxTransport::Ssh {
            destination: "h".into(),
            ssh_opts: vec![],
            mux: "tmux".into(),
        }
        .is_remote());
        assert!(TmuxTransport::Wsl {
            distro: "Ubuntu".into(),
            mux: "tmux".into(),
        }
        .is_remote());
        assert!(TmuxTransport::sandbox(place()).is_remote());
    }
}
