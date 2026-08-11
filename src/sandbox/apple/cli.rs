//! What Apple's `container` CLI is on this host, and what it lets friring do.
//!
//! The probe answers four questions in the order they stop being fixable, so
//! the first refusal a user sees is the one they can act on last:
//!
//! 1. **The machine.** Containerization.framework starts one arm64 virtual
//!    machine per container, so this is macOS on Apple Silicon or it is nothing
//!    — Rosetta translates an amd64 *image* inside that VM, never the VM itself.
//! 2. **macOS 26.** Below it the tool cannot create a network, so every place
//!    would share the one every other container on the Mac is on. friring
//!    refuses that rather than granting it quietly (`docs/SANDBOX.md`
//!    §`apple-container`).
//! 3. **The tool.** Installed, resolved to an absolute path, and not sitting
//!    anywhere a sandboxed agent could rewrite — the rule
//!    [`crate::sandbox::dirs::rewritable_root`] applies to every binary that
//!    *is* a boundary, and this one asks the system service for the VM.
//! 4. **The service.** `container system status`, which is this tool's "is the
//!    daemon running" and carries its own actionable message.
//!
//! Then the flag surface. friring builds a place out of five options
//! ([`REQUIRED_RUN_FLAGS`]) and reads them out of `container run --help` rather
//! than assuming them: this CLI is young and its options move, and a flag that
//! turns out not to exist has to be a probe failure naming the flag, never a
//! create that fails halfway with the tool's own parser error. Nothing here
//! starts, pulls or builds anything — `--version`, `system status` and two
//! `--help` texts are the whole probe.

use std::collections::BTreeSet;

use crate::sandbox::backend::Availability;
use crate::sandbox::dirs;
use crate::sandbox::probe::{detect_platform, HostPlatform, ProbeHost, ProbeOutput};

/// The name looked up on `PATH`.
///
/// A lookup key and never what is executed: the probe resolves it once to an
/// absolute path and vets the answer, exactly as the container engines and
/// bubblewrap do. The tool that asks for the VM decides what the boundary is, so
/// a copy the sandboxed agent can rewrite is a boundary the sandboxed agent
/// chooses.
pub const PROGRAM: &str = "container";

/// The macOS major version that can create a network (`container network
/// create`).
pub const MACOS_NETWORKS: u32 = 26;

/// The options friring's `run` is built from, checked against `run --help`.
///
/// Each one is load-bearing rather than convenient: `--detach` because a place
/// outlives the command that made it, `--name` because friring addresses a place
/// by the name it minted, `--label` because friring must be able to prove a
/// container is its own before it reuses or removes one, `--mount` because a
/// place is worth nothing without the profile's paths at their own paths, and
/// `--network` because friring puts every place on a network of its own.
pub const REQUIRED_RUN_FLAGS: &[&str] = &["detach", "name", "label", "mount", "network"];

/// What the probe learned, cached with the availability it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleDetails {
    pub availability: Availability,
    /// The absolute path the probe resolved and vetted, or `None` when the tool
    /// is unusable. This — never the bare name — is what is executed.
    pub program: Option<String>,
    /// The tool's own version string, for the picker's detail line.
    pub version: Option<String>,
    /// macOS major version of the host the tool runs on, or `0` when it could
    /// not be read.
    pub macos_major: u32,
    /// Every long option `container run --help` documents, without the `--`.
    flags: BTreeSet<String>,
}

impl AppleDetails {
    /// Whether `container run --help` documents `--<flag>`.
    ///
    /// Asked again at plan time for the options only some profiles need
    /// (`--memory`, `--cpus`): a limit friring cannot pass is a limit that is
    /// not enforced, and `docs/SANDBOX.md` §Failure modes wants that reported
    /// rather than accepted and ignored.
    pub fn documents(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }
}

/// Ask `host` about Apple's container CLI.
///
/// Every question goes through [`ProbeHost`], so this is testable against a
/// machine with nothing installed — which is also the only way it *can* be
/// tested, since nothing in this crate's tests may start a virtual machine.
pub fn probe(host: &dyn ProbeHost) -> AppleDetails {
    let unavailable = |availability, macos_major| AppleDetails {
        availability,
        program: None,
        version: None,
        macos_major,
        flags: BTreeSet::new(),
    };

    let platform = detect_platform(host);
    let HostPlatform::MacOs {
        apple_silicon,
        major,
    } = platform
    else {
        return unavailable(
            Availability::unavailable(format!(
                "Apple's container CLI is macOS-only; this host is {}",
                platform.label()
            )),
            0,
        );
    };
    if !apple_silicon {
        // Nothing to install and nothing to upgrade: Containerization runs an
        // arm64 guest, and Rosetta translates an amd64 image *inside* one.
        return unavailable(
            Availability::needs_fix(
                "Apple's container CLI needs Apple Silicon: it starts an arm64 virtual machine \
                 per container, and Rosetta translates an amd64 image inside that VM rather than \
                 the VM itself",
                "on an Intel Mac use seatbelt, or docker/podman for a place",
            ),
            major,
        );
    }
    if major < MACOS_NETWORKS {
        // The immovable one, so it is reported before anything that could be
        // installed. friring gives every place a network of its own, and
        // `container network create` is how — without it a place shares the
        // network every other container on this Mac is on, which is a boundary
        // friring will not grant silently.
        return unavailable(
            Availability::needs_fix(
                format!(
                    "Apple's container CLI cannot create a network below macOS {MACOS_NETWORKS}, \
                     and friring puts every place on a network of its own rather than the one \
                     every other container on this Mac shares; this host is macOS {major}"
                ),
                format!(
                    "upgrade to macOS {MACOS_NETWORKS}, or use seatbelt here and docker/podman \
                     for a place"
                ),
            ),
            major,
        );
    }

    let Some(program) = host.which(PROGRAM) else {
        return unavailable(
            Availability::needs_fix(
                "Apple's container CLI is not installed",
                "install it: https://github.com/apple/container",
            ),
            major,
        );
    };
    // Judged on both spellings, for the reason the container engines are: a name
    // on `PATH` under a system prefix that is really a symlink into a writable
    // one is the obvious way past a check that only read the name.
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
        return unavailable(
            Availability::needs_fix(
                format!(
                    "Apple's container CLI resolves to {where_from}, inside '{root}' — a \
                     sandboxed agent could replace it and the next launch would run whatever it \
                     planted"
                ),
                "install it system-wide and take the writable copy off PATH",
            ),
            major,
        );
    }

    let version = match host.run(&program, &["--version"]) {
        Ok(output) if output.ok() => parse_version(output.trimmed()),
        Ok(output) => {
            return unavailable(
                Availability::needs_fix(
                    first_line(
                        &output.stderr,
                        "Apple's container CLI would not report a version",
                    ),
                    "reinstall it: https://github.com/apple/container",
                ),
                major,
            )
        }
        Err(detail) => {
            return unavailable(
                Availability::needs_fix(detail, "reinstall it: https://github.com/apple/container"),
                major,
            )
        }
    };

    // The analogue of "the Docker daemon is not running": this CLI talks to a
    // launchd service, and everything below it fails with a connection error
    // that says nothing actionable.
    match host.run(&program, &["system", "status"]) {
        Ok(output) if output.ok() => {}
        Ok(output) => {
            let said = first_line(&output.stderr, "");
            let reason = if said.is_empty() {
                first_line(
                    &output.stdout,
                    "Apple's container system service is not running",
                )
            } else {
                said
            };
            return unavailable(
                Availability::needs_fix(reason, format!("start it: {PROGRAM} system start")),
                major,
            );
        }
        Err(detail) => {
            return unavailable(
                Availability::needs_fix(detail, format!("start it: {PROGRAM} system start")),
                major,
            )
        }
    }

    let flags = match host.run(&program, &["run", "--help"]) {
        Ok(output) if output.ok() => documented_flags(&output.stdout),
        _ => BTreeSet::new(),
    };
    let missing: Vec<&str> = REQUIRED_RUN_FLAGS
        .iter()
        .copied()
        .filter(|flag| !flags.contains(*flag))
        .collect();
    if !missing.is_empty() {
        let named: Vec<String> = missing.iter().map(|flag| format!("--{flag}")).collect();
        return unavailable(
            Availability::needs_fix(
                format!(
                    "this build of Apple's container CLI does not document {} on '{PROGRAM} run', \
                     and friring builds a place out of those options — one it cannot pass is a \
                     place that is not what the profile says",
                    named.join(", ")
                ),
                "upgrade Apple's container CLI: https://github.com/apple/container",
            ),
            major,
        );
    }
    // The transport reaches a place with `<tool> exec -i <container> tmux …`;
    // `-i` is what carries the control-mode protocol, and without it every
    // session in a place opens on a pane that never speaks.
    let interactive = host
        .run(&program, &["exec", "--help"])
        .ok()
        .filter(ProbeOutput::ok)
        .is_some_and(|output| output.stdout.to_ascii_lowercase().contains("interactive"));
    if !interactive {
        return unavailable(
            Availability::needs_fix(
                format!(
                    "this build of Apple's container CLI does not document an interactive \
                     '{PROGRAM} exec', and that is how friring reaches the tmux inside a place"
                ),
                "upgrade Apple's container CLI: https://github.com/apple/container",
            ),
            major,
        );
    }

    let named = match &version {
        Some(version) => format!("Apple container {version}"),
        None => "Apple container".to_string(),
    };
    AppleDetails {
        // The limitation is on the picker's own line rather than only in the
        // launch refusal: this backend's boundary is the strongest on macOS and
        // its egress control is the weakest, and a user choosing it deserves to
        // read the second half before they choose.
        availability: Availability::available(format!(
            "{named} (macOS {major}; only network 'full' can be enforced in a place)"
        )),
        program: Some(program),
        version,
        macos_major: major,
        flags,
    }
}

/// The version out of whatever `--version` printed.
///
/// Lenient on purpose: the first token that looks like a dotted number. An
/// unparseable line costs the picker a detail, never the backend.
fn parse_version(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()) && token.contains('.'))
        .map(|token| {
            token
                .trim_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_string()
        })
        .filter(|version| !version.is_empty())
}

/// Every `--long-option` a help text mentions, without the dashes.
///
/// A scan rather than a parse: help output wraps, groups and decorates, and the
/// only question asked of it is whether a flag is named at all.
fn documented_flags(text: &str) -> BTreeSet<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut flags = BTreeSet::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == '-' && bytes[i + 1] == '-' && bytes[i + 2].is_ascii_alphabetic() {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == '-') {
                end += 1;
            }
            flags.insert(
                bytes[start..end]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase(),
            );
            i = end;
            continue;
        }
        i += 1;
    }
    flags
}

/// The first non-empty line of a tool's output, which is where its own
/// actionable message is.
pub(super) fn first_line(text: &str, fallback: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::probe::StubHost;

    /// One option line each, in the shape a CLI of this kind prints them —
    /// assembled rather than written out whole, so a test that is about a
    /// *missing* option takes one line away instead of matching a formatted
    /// blob that rustfmt is free to re-wrap.
    const RUN_FLAG_LINES: &[&str] = &[
        "  -d, --detach            Run the container detached",
        "      --name <name>       Assign a name",
        "  -l, --label <label>     Add a key=value label",
        "      --mount <mount>     Add a mount",
        "  -v, --volume <volume>   Bind mount a volume",
        "      --network <network> Attach to a network",
        "  -m, --memory <memory>   Memory limit",
        "  -c, --cpus <cpus>       CPU count",
        "  -e, --env <env>         Set an environment variable",
    ];

    /// `container run --help`, optionally without the line naming one option.
    fn run_help(without: Option<&str>) -> String {
        let mut text = "USAGE: container run [<options>] <image>\nOPTIONS:\n".to_string();
        for line in RUN_FLAG_LINES
            .iter()
            .filter(|line| !without.is_some_and(|flag| line.contains(flag)))
        {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    const EXEC_HELP: &str = "USAGE: container exec [<options>] <container-id> <arguments> ...\n\
                             OPTIONS:\n\
                             \x20 -i, --interactive   Keep stdin open\n\
                             \x20 -t, --tty           Allocate a pty\n";

    /// An Apple Silicon Mac on macOS 26 with the tool installed and answering.
    fn host() -> StubHost {
        host_answering(&run_help(None), EXEC_HELP)
    }

    /// The same, with the two help texts a test wants.
    ///
    /// Built from scratch rather than overridden: the stub answers with the
    /// *first* command it was given, so a second registration of one line would
    /// never be reached.
    fn host_answering(run: &str, exec: &str) -> StubHost {
        StubHost::macos(26, true)
            .with_binary(PROGRAM)
            .with_command(
                &format!("/usr/bin/{PROGRAM} --version"),
                ProbeOutput::success("container CLI version 0.5.0 (build: release)\n"),
            )
            .with_command(
                &format!("/usr/bin/{PROGRAM} system status"),
                ProbeOutput::success("apiserver is running\n"),
            )
            .with_command(
                &format!("/usr/bin/{PROGRAM} run --help"),
                ProbeOutput::success(run),
            )
            .with_command(
                &format!("/usr/bin/{PROGRAM} exec --help"),
                ProbeOutput::success(exec),
            )
    }

    #[test]
    fn a_supported_mac_with_the_tool_running_is_available() {
        let details = probe(&host());
        assert!(details.availability.is_available());
        assert_eq!(details.program.as_deref(), Some("/usr/bin/container"));
        assert_eq!(details.version.as_deref(), Some("0.5.0"));
        assert_eq!(details.macos_major, 26);
        // The picker's own line says what this backend cannot do, because its
        // egress story is the weakest part of the strongest boundary on macOS.
        let message = details.availability.message();
        assert!(message.contains("Apple container 0.5.0"), "{message}");
        assert!(message.contains("only network 'full'"), "{message}");
        // The optional flags are readable for the plan that needs them.
        assert!(details.documents("memory"));
        assert!(details.documents("cpus"));
        assert!(!details.documents("cap-drop"));
    }

    #[test]
    fn the_wrong_machine_says_which_half_is_wrong() {
        let linux = probe(&StubHost::linux_with_bwrap("0.11.0"));
        assert!(
            linux.availability.message().contains("macOS-only"),
            "{linux:?}"
        );

        let intel = probe(&StubHost::macos(26, false).with_binary(PROGRAM));
        let message = intel.availability.message();
        assert!(message.contains("Apple Silicon"), "{message}");
        // Rosetta is the thing people expect to rescue this, so it is answered
        // rather than left to be tried.
        assert!(message.contains("Rosetta"), "{message}");
        assert!(intel.program.is_none());
    }

    /// The macOS bar is reported before anything installable, because it is the
    /// one the user cannot fix by installing something.
    #[test]
    fn below_macos_26_the_reason_is_the_network_and_it_precedes_the_install() {
        let old = probe(&StubHost::macos(15, true));
        let message = old.availability.message();
        assert!(message.contains("macOS 26"), "{message}");
        assert!(message.contains("network of its own"), "{message}");
        assert!(message.contains("macOS 15"), "{message}");
        assert!(!message.contains("not installed"), "{message}");
    }

    #[test]
    fn a_missing_tool_and_a_stopped_service_both_say_what_to_do() {
        let bare = probe(&StubHost::macos(26, true));
        assert!(bare.availability.message().contains("not installed"));
        assert!(bare.program.is_none());

        let stopped = StubHost::macos(26, true)
            .with_binary(PROGRAM)
            .with_command(
                &format!("/usr/bin/{PROGRAM} --version"),
                ProbeOutput::success("container CLI version 0.5.0\n"),
            )
            .with_command(
                &format!("/usr/bin/{PROGRAM} system status"),
                ProbeOutput::failure(1, "apiserver is not running and not registered\n"),
            );
        let message = probe(&stopped).availability.message();
        assert!(message.contains("apiserver is not running"), "{message}");
        assert!(message.contains("container system start"), "{message}");
    }

    /// The tool is young and its options move. A flag friring builds a place out
    /// of that this build does not have has to be a probe failure naming the
    /// flag — not a `run` that fails halfway with the tool's own parser error.
    #[test]
    fn a_cli_missing_an_option_friring_builds_a_place_from_is_unavailable() {
        let unlabelled = host_answering(&run_help(Some("--label")), EXEC_HELP);
        let message = probe(&unlabelled).availability.message();
        assert!(message.contains("--label"), "{message}");
        assert!(message.contains("does not document"), "{message}");

        // And the transport's own requirement: `exec -i` is what carries the
        // control-mode protocol into the place.
        let without_i = host_answering(
            &run_help(None),
            "USAGE: container exec <container-id> <arguments> ...\n",
        );
        let message = probe(&without_i).availability.message();
        assert!(message.contains("interactive"), "{message}");
    }

    /// The rule every binary that *is* a boundary follows: a copy the sandboxed
    /// agent could rewrite is a boundary the sandboxed agent chooses.
    #[test]
    fn a_tool_the_sandbox_could_replace_is_refused() {
        let planted =
            StubHost::macos(26, true).with_binary_at(PROGRAM, "/Users/u/.local/bin/container");
        let message = probe(&planted).availability.message();
        assert!(
            message.contains("/Users/u/.local/bin/container"),
            "{message}"
        );
        assert!(message.contains("could replace it"), "{message}");
    }

    #[test]
    fn help_text_and_version_are_read_leniently() {
        assert_eq!(
            parse_version("container CLI version 0.5.0"),
            Some("0.5.0".to_string())
        );
        assert_eq!(
            parse_version("0.1.0-beta.2"),
            Some("0.1.0-beta.2".to_string())
        );
        assert_eq!(parse_version("no version here"), None);

        let flags = documented_flags("-d, --detach  x\n--mount <m>\n  --dns-domain <d>\n-- \n");
        assert!(flags.contains("detach"));
        assert!(flags.contains("mount"));
        assert!(flags.contains("dns-domain"));
        assert!(!flags.contains(""));
    }
}
