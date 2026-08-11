//! Tests for the `wsl-distro` place backend.
//!
//! **Nothing here registers, imports, hardens or destroys a distro.** Every
//! `wsl.exe` call goes through the injected [`StubHost`], which answers only
//! what a test scripted and fails anything else — so the lifecycle, the argv and
//! every refusal are exercised on a machine with no WSL at all, and a command
//! this module gets wrong fails the test rather than reaching a real distro.

use std::sync::Arc;

use super::*;
use crate::sandbox::backend::{PlaceLaunch, PlaceRelay, ProxyEndpoint};
use crate::sandbox::probe::StubHost;
use crate::session::{SandboxPath, SandboxPolicy};

/// Where `wsl.exe` lives on a Windows host, spelled with forward slashes
/// (Windows takes either) so the stub does not read it as a bare program name.
const WSL_EXE: &str = "C:/Windows/System32/wsl.exe";

/// The template a profile clones, and the distro friring registers from it.
const TEMPLATE: &str = "Ubuntu-24.04";
const DISTRO: &str = "friring-sbx-dev";

/// `wsl --list --verbose` on a host with a template and no friring distro.
const LIST: &str = "  NAME             STATE           VERSION\n\
                    * Ubuntu-24.04     Running         2\n";

/// The same host, once friring has registered this profile's distro.
const LIST_WITH_DISTRO: &str = "  NAME               STATE           VERSION\n\
                                * Ubuntu-24.04       Running         2\n\
                                \x20 friring-sbx-dev    Stopped         2\n";

/// The bytes `wsl.exe` really writes: UTF-16LE behind a byte-order mark, which
/// a lossy UTF-8 read turns into this.
///
/// Shared with [`super::plan`]'s own tests, because getting the encoding wrong
/// is what a decoder test is for.
pub(super) fn as_utf16(text: &str) -> String {
    let mut bytes: Vec<u8> = vec![0xff, 0xfe];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn ok16(text: &str) -> ProbeOutput {
    ProbeOutput::success(as_utf16(text))
}

/// A Windows host with a Store WSL, listing `list`.
///
/// The listing is a parameter rather than something a test overrides
/// afterwards: the stub answers with the *first* scripted match, so a later
/// entry for the same command line would silently never be reached — and a test
/// about an existing distro would quietly exercise registering a fresh one.
fn windows_listing(list: &str) -> StubHost {
    StubHost::new()
        .with_home("C:/Users/me")
        // No `uname`, and `cmd.exe` is what settles the platform.
        .with_binary("cmd.exe")
        .with_binary_at(WSL, WSL_EXE)
        .with_command(
            &format!("{WSL_EXE} --version"),
            ok16("WSL version: 2.3.26.0\nKernel version: 5.15.167.4-1\n"),
        )
        .with_command(&format!("{WSL_EXE} --list --verbose"), ok16(list))
}

/// A Windows host with one WSL2 distro to clone and none of friring's.
fn windows() -> StubHost {
    windows_listing(LIST)
}

/// The four commands that register a distro.
fn clones(host: StubHost) -> StubHost {
    host.with_command_prefix(
        &format!("{WSL_EXE} --export {TEMPLATE} "),
        ProbeOutput::success(""),
    )
    .with_command_prefix(
        &format!("{WSL_EXE} --import {DISTRO} "),
        ProbeOutput::success(""),
    )
    // Exact, not a prefix: the hardening script *is* the boundary, so a change
    // to it must fail this test rather than be answered by a wildcard.
    .with_command(
        &format!(
            "{WSL_EXE} -d {DISTRO} -u root --exec sh -c {}",
            plan::harden_script("dev")
        ),
        ProbeOutput::success(""),
    )
    .with_command(
        &format!("{WSL_EXE} --terminate {DISTRO}"),
        ProbeOutput::success(""),
    )
}

/// A distro whose `/etc/wsl.conf` is still the one friring wrote.
fn hardened(host: StubHost) -> StubHost {
    host.with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec cat {}", plan::WSL_CONF),
        ProbeOutput::success(plan::WSL_CONF_CONTENTS),
    )
}

/// What the inside of a usable distro answers: a home, a bubblewrap, and the
/// one path the test profile grants.
fn inside(host: StubHost) -> StubHost {
    host.with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec sh -c printf %s \"$HOME\""),
        ProbeOutput::success("/root\n"),
    )
    .with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec sh -c command -v bwrap"),
        ProbeOutput::success("/usr/bin/bwrap\n"),
    )
    .with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec test -e /root/dev/app"),
        ProbeOutput::success(""),
    )
}

/// A distro that starts.
fn runs(host: StubHost) -> StubHost {
    host.with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec true"),
        ProbeOutput::success(""),
    )
}

/// A distro carrying friring's marker for this profile.
fn marked(host: StubHost) -> StubHost {
    host.with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec cat {}", plan::MARKER_FILE),
        ProbeOutput::success("dev\n"),
    )
}

/// Everything registering a fresh distro needs.
fn registers(host: StubHost) -> StubHost {
    inside(hardened(clones(host)))
}

/// Everything adopting one friring already registered needs.
fn adopted(host: StubHost) -> StubHost {
    marked(runs(inside(hardened(host))))
}

fn backend(host: StubHost) -> WslDistroBackend {
    WslDistroBackend::new(Arc::new(host))
}

fn profile() -> SandboxProfile {
    let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
    // The default is `allowlist`, which a WSL place cannot enforce — every test
    // that is not about that refusal starts from a mode it can.
    profile.network_mode = NetworkMode::None;
    profile.image = Some(TEMPLATE.to_string());
    profile
}

fn policy_for(profile: &SandboxProfile) -> SandboxPolicy {
    profile
        .resolve(SandboxBackendKind::WslDistro, "/root")
        .unwrap()
}

/// Drop the place tree a registering test minted under the unit-test data
/// directory.
fn cleanup() {
    dirs::cleanup_place("dev");
}

#[test]
fn a_windows_host_with_a_wsl2_distro_offers_the_backend() {
    let backend = backend(windows());
    assert!(backend.probe().is_available());
    assert_eq!(backend.probe().message(), "WSL 2.3");
    assert_eq!(backend.details().version, Some((2, 3)));
    assert_eq!(backend.details().program.as_deref(), Some(WSL_EXE));
    assert_eq!(backend.kind(), SandboxBackendKind::WslDistro);
}

/// Every capability a WSL place cannot honour is declared unavailable rather
/// than accepted and ignored: a memory cap is set once for the utility VM every
/// distro shares, and a filtered network mode is enforced by a proxy the distro
/// has no route to.
#[test]
fn capabilities_say_what_one_shared_utility_vm_cannot_do() {
    let caps = backend(windows()).capabilities();
    assert_eq!(caps.shape, SandboxShape::Place);
    assert!(caps.persistent);
    assert!(!caps.limits, "a per-distro cap does not exist");
    assert!(!caps.host_credentials);
    assert_eq!(caps.network_modes, [NetworkMode::None, NetworkMode::Full]);
    assert!(!caps.network_modes.contains(&NetworkMode::Allowlist));
    assert_eq!(caps.read_scopes, [ReadScope::Workspace]);
}

/// The rung is reachable only from the Windows side, and the message says what
/// to use instead — which is the rung *above* it on the same ladder.
#[test]
fn the_backend_refuses_every_host_that_is_not_windows() {
    let inside = StubHost::new()
        .with_command("uname -s", ProbeOutput::success("Linux\n"))
        .with_file(
            "/proc/sys/kernel/osrelease",
            "5.15.153.1-microsoft-standard-WSL2\n",
        );
    let message = backend(inside).probe().message();
    assert!(message.contains("Windows-side operation"), "{message}");
    assert!(message.contains("bwrap"), "{message}");

    let linux = backend(StubHost::linux_with_bwrap("0.11.0"));
    assert!(linux.probe().message().contains("needs Windows"));
    let mac = backend(StubHost::macos(26, true));
    assert!(mac.probe().message().contains("needs Windows"));
}

/// Four probe verdicts a user can act on, and the one that must never be
/// papered over: WSL1 has no utility VM, so there is nothing to isolate with.
#[test]
fn probe_failures_name_the_command_that_fixes_them() {
    let bare = StubHost::new()
        .with_binary("cmd.exe")
        .with_home("C:/Users/me");
    let message = backend(bare).probe().message();
    assert!(message.contains("WSL is not installed"), "{message}");
    assert!(message.contains("wsl --install"), "{message}");

    // The legacy inbox WSL has no `--version` at all, and no VHD export.
    let legacy = StubHost::new()
        .with_home("C:/Users/me")
        .with_binary("cmd.exe")
        .with_binary_at(WSL, WSL_EXE)
        .with_command(
            &format!("{WSL_EXE} --version"),
            ProbeOutput::failure(1, "Invalid command line argument: --version\n"),
        );
    let message = backend(legacy).probe().message();
    assert!(message.contains("older than 2.0"), "{message}");
    assert!(message.contains("wsl --update"), "{message}");

    // WSL1 is refused by name, because that is the sentence a user can act on.
    let wsl1 = StubHost::new()
        .with_home("C:/Users/me")
        .with_binary("cmd.exe")
        .with_binary_at(WSL, WSL_EXE)
        .with_command(
            &format!("{WSL_EXE} --version"),
            ok16("WSL version: 2.3.26.0\n"),
        )
        .with_command(
            &format!("{WSL_EXE} --list --verbose"),
            ok16("  NAME      STATE       VERSION\n* Legacy    Stopped     1\n"),
        );
    let message = backend(wsl1).probe().message();
    assert!(message.contains("WSL1 is not supported"), "{message}");
    assert!(message.contains("wsl --set-version"), "{message}");

    // And a host with WSL and nothing to clone says which command installs one.
    let empty = StubHost::new()
        .with_home("C:/Users/me")
        .with_binary("cmd.exe")
        .with_binary_at(WSL, WSL_EXE)
        .with_command(
            &format!("{WSL_EXE} --version"),
            ok16("WSL version: 2.3.26.0\n"),
        )
        .with_command(
            &format!("{WSL_EXE} --list --verbose"),
            ok16("  NAME      STATE       VERSION\n"),
        );
    let message = backend(empty).probe().message();
    assert!(message.contains("no WSL distro"), "{message}");
    assert!(message.contains("wsl --install -d"), "{message}");
}

/// `wsl.exe` registers and destroys distros, so a copy the sandboxed agent
/// could replace is a boundary the sandboxed agent chooses — the rule
/// bubblewrap and the container engines apply to themselves.
#[test]
fn a_wsl_exe_the_agent_could_rewrite_is_never_the_boundary() {
    let planted = StubHost::new()
        .with_home("C:/Users/me")
        .with_binary("cmd.exe")
        .with_binary_at(WSL, "C:/Users/me/bin/wsl.exe");
    let backend = backend(planted);
    let message = backend.probe().message();
    assert!(message.contains("C:/Users/me/bin/wsl.exe"), "{message}");
    assert!(message.contains("could replace it"), "{message}");
    assert!(backend.details().program.is_none());
}

/// The whole registration, in the order that makes it a boundary: export the
/// template as a VHD, import it as a distro of its own, write the hardened
/// `/etc/wsl.conf` and the marker in one script, stop the distro so WSL reads
/// that file when it next starts, and check it stuck.
///
/// Every command is scripted exactly or by prefix, and the stub fails anything
/// else — so a step this backend gets wrong fails here rather than on a real
/// machine.
#[test]
fn registering_a_distro_hardens_it_before_anything_runs_in_it() {
    let backend = backend(registers(windows()));
    let ensured = backend.ensure_distro(&profile()).unwrap();

    assert_eq!(ensured.distro, DISTRO);
    assert_eq!(ensured.instance.external_id, DISTRO);
    assert_eq!(ensured.instance.engine, SandboxBackendKind::WslDistro);
    assert_eq!(ensured.instance.profile, "dev");
    assert_eq!(ensured.instance.state, INSTANCE_STATE_RUNNING);
    // `$HOME` is the distro's, not Windows': the profile's `~` means nothing
    // else inside a place.
    assert_eq!(ensured.home, "/root");
    assert_eq!(ensured.bwrap_program, "/usr/bin/bwrap");
    assert!(ensured.warnings.is_empty(), "{:?}", ensured.warnings);
    cleanup();
}

/// A second launch of the same profile reuses the distro — and the ownership
/// check is what makes that safe, because the name alone is something anyone
/// can `wsl --import` under.
#[test]
fn an_existing_distro_is_adopted_only_when_its_marker_says_it_is_ours() {
    // Nothing here scripts an export or an import, so a second registration
    // would fail this test rather than quietly clone the template again.
    let ensured = backend(adopted(windows_listing(LIST_WITH_DISTRO)))
        .ensure_distro(&profile())
        .unwrap();
    assert_eq!(ensured.distro, DISTRO);

    // A distro of that name that friring did not register is neither reused nor
    // destroyed: it starts, and it carries no marker.
    let stranger = runs(inside(hardened(windows_listing(LIST_WITH_DISTRO))));
    let err = backend(stranger).ensure_distro(&profile()).unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
    assert!(text.contains("not registered by friring"), "{text}");
    assert!(text.contains("neither reused nor destroyed"), "{text}");

    // And one friring registered for a *different* profile says whose it is.
    let other = runs(inside(hardened(windows_listing(LIST_WITH_DISTRO)))).with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec cat {}", plan::MARKER_FILE),
        ProbeOutput::success("other\n"),
    );
    let text = backend(other)
        .ensure_distro(&profile())
        .unwrap_err()
        .to_string();
    assert!(text.contains("for the profile 'other'"), "{text}");

    // A distro that will not start at all is reported as that, rather than as
    // one carrying no marker: the two need different things from the user.
    let dead = inside(hardened(windows_listing(LIST_WITH_DISTRO))).with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec true"),
        ProbeOutput::failure(
            1,
            as_utf16("The Windows Subsystem for Linux instance has terminated.\n"),
        ),
    );
    let text = backend(dead)
        .ensure_distro(&profile())
        .unwrap_err()
        .to_string();
    assert!(text.contains("would not start"), "{text}");
    assert!(text.contains("has terminated"), "{text}");
}

/// `/etc/wsl.conf` *is* the boundary: with automount back on, the Windows
/// filesystem — friring's data directory and the database ADR-29 keeps out of
/// every sandbox with it — is inside the place; with interop back on, a process
/// in there can run a Windows binary outside the VM entirely.
///
/// So a distro whose configuration is no longer the one friring wrote is
/// refused as **tampering**, which no `allow_unsandboxed_fallback` may convert
/// into a launch on the host.
#[test]
fn a_distro_whose_hardening_was_undone_is_refused_as_tampering() {
    let meddled = marked(runs(inside(windows_listing(LIST_WITH_DISTRO)))).with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec cat {}", plan::WSL_CONF),
        ProbeOutput::success("[automount]\nenabled = true\n"),
    );
    let err = backend(meddled).ensure_distro(&profile()).unwrap_err();
    assert!(err.is_tampering(), "{err}");
    let text = err.to_string();
    assert!(
        text.contains("Windows filesystem out of the place"),
        "{text}"
    );
    assert!(text.contains("wsl --unregister"), "{text}");
    cleanup();
}

/// The template decides what a place is made of, so the two shapes that would
/// quietly weaken it are refused: a WSL1 template is not a boundary at all, and
/// cloning another profile's sandbox would copy that profile's whole filesystem
/// — its agent's login included — into a second one (ADR-28).
#[test]
fn a_template_that_is_wsl1_or_another_sandbox_is_refused() {
    let wsl1 = registers(windows_listing(
        "  NAME             STATE       VERSION\n\
         * Ubuntu-24.04     Stopped     1\n\
         \x20 Modern           Stopped     2\n",
    ));
    let text = backend(wsl1)
        .ensure_distro(&profile())
        .unwrap_err()
        .to_string();
    assert!(text.contains("WSL1 is not supported"), "{text}");
    assert!(text.contains("wsl --set-version"), "{text}");

    let mut cloning_a_sandbox = profile();
    cloning_a_sandbox.image = Some("friring-sbx-other".to_string());
    let host = registers(windows_listing(
        "  NAME                 STATE      VERSION\n\
         * friring-sbx-other    Stopped    2\n",
    ));
    let text = backend(host)
        .ensure_distro(&cloning_a_sandbox)
        .unwrap_err()
        .to_string();
    assert!(
        text.contains("one of friring's own sandbox distros"),
        "{text}"
    );
    assert!(text.contains("ADR-28"), "{text}");

    // A template that is simply not there says what is.
    let mut missing = profile();
    missing.image = Some("Debian".to_string());
    let text = backend(registers(windows()))
        .ensure_distro(&missing)
        .unwrap_err()
        .to_string();
    assert!(text.contains("'Debian', which is not registered"), "{text}");
    assert!(text.contains(TEMPLATE), "{text}");
    cleanup();
}

/// Everything a WSL place cannot enforce is refused before a distro is
/// registered, rather than accepted and quietly ignored.
#[test]
fn a_profile_asking_for_what_wsl_cannot_enforce_is_refused_before_anything_is_built() {
    let backend = backend(registers(windows()));

    let mut capped = profile();
    capped.memory_mb = Some(4096);
    let text = backend.ensure_distro(&capped).unwrap_err().to_string();
    assert!(text.contains("global to the utility VM"), "{text}");
    assert!(text.contains(".wslconfig"), "{text}");

    let mut built = profile();
    built.image = None;
    built.containerfile = Some("packaging/sandbox/Containerfile".to_string());
    let text = backend.ensure_distro(&built).unwrap_err().to_string();
    assert!(text.contains("not built from a containerfile"), "{text}");

    // A filtered mode is the one that would be dangerous rather than merely
    // untrue: a sandbox started believing it is proxied has no filter at all.
    for (mode, deny) in [
        (NetworkMode::Allowlist, Vec::new()),
        (NetworkMode::Full, vec!["evil.example".to_string()]),
    ] {
        let mut filtered = profile();
        filtered.network_mode = mode;
        filtered.network_deny = deny;
        let text = backend.ensure_distro(&filtered).unwrap_err().to_string();
        assert!(text.contains("egress proxy"), "{mode}: {text}");
        assert!(
            text.contains("Windows side of the utility VM"),
            "{mode}: {text}"
        );
        assert!(text.contains("bwrap backend"), "{mode}: {text}");
    }

    // And the two modes it *can* enforce are not refused with them: `none` is
    // bubblewrap's own namespace inside the distro, and `full` is the utility
    // VM's network with nothing to take back.
    let mut open = profile();
    open.network_mode = NetworkMode::Full;
    assert!(backend.ensure_distro(&open).is_ok());
    cleanup();
}

/// A distro is one filesystem and one identity; bubblewrap inside it is what
/// applies the profile's paths and its network mode. Without one the agent
/// would see all of the distro read-write whatever the profile says, so the
/// launch is refused with the command that fixes it.
#[test]
fn a_distro_without_bubblewrap_is_refused_with_the_command_that_installs_one() {
    let host = hardened(clones(windows())).with_command(
        &format!("{WSL_EXE} -d {DISTRO} --exec sh -c printf %s \"$HOME\""),
        ProbeOutput::success("/root\n"),
    );
    let text = backend(host)
        .ensure_distro(&profile())
        .unwrap_err()
        .to_string();
    assert!(text.contains("carries no bubblewrap"), "{text}");
    assert!(text.contains("one network namespace"), "{text}");
    assert!(text.contains("apt install bubblewrap"), "{text}");

    // And one the agent could rewrite is refused for the reason a rewritable
    // `wsl.exe` is: it applies the boundary.
    let planted = hardened(clones(windows()))
        .with_command(
            &format!("{WSL_EXE} -d {DISTRO} --exec sh -c printf %s \"$HOME\""),
            ProbeOutput::success("/root\n"),
        )
        .with_command(
            &format!("{WSL_EXE} -d {DISTRO} --exec sh -c command -v bwrap"),
            ProbeOutput::success("/root/.local/bin/bwrap\n"),
        );
    let text = backend(planted)
        .ensure_distro(&profile())
        .unwrap_err()
        .to_string();
    assert!(text.contains("/root/.local/bin/bwrap"), "{text}");
    assert!(text.contains("would run unwrapped"), "{text}");
    cleanup();
}

/// Repositories belong on the distro's ext4. A Windows-side path is warned
/// about rather than refused — and the warning says the part that would
/// otherwise be a mystery, that the hardened distro mounts no Windows drive at
/// all.
#[test]
fn a_windows_side_path_comes_back_as_a_warning() {
    let mut on_windows = profile();
    on_windows.paths = vec![
        SandboxPath::workspace("/mnt/c/Users/me/repo"),
        SandboxPath::workspace("~/dev/app"),
    ];
    let ensured = backend(registers(windows()))
        .ensure_distro(&on_windows)
        .unwrap();
    assert_eq!(ensured.warnings.len(), 1, "{:?}", ensured.warnings);
    assert!(ensured.warnings[0].contains("/mnt/c/Users/me/repo"));
    assert!(ensured.warnings[0].contains("automount is off"));
    cleanup();
}

/// The command that runs inside the distro is a **bubblewrap** command line,
/// because a distro is a filesystem boundary and not a network one: all WSL
/// distros share one kernel and one network namespace, and `--unshare-net`
/// inside the distro is the only per-sandbox one there is.
#[test]
fn the_in_distro_command_is_bubblewrap_around_the_agent() {
    let backend = backend(registers(windows()));
    let profile = profile();
    let ensured = backend.ensure_distro(&profile).unwrap();
    let policy = policy_for(&profile);
    let launch = SandboxLaunch::new(&policy, "/root", "s1")
        .with_workspace("/root/dev/app")
        .with_place(PlaceLaunch { relay: None });
    let argv = backend
        .wrap(vec!["claude".into(), "--resume".into()], &launch)
        .unwrap();

    assert_eq!(argv[0], ensured.bwrap_program);
    // The profile's path is bound at exactly itself — identical absolute paths,
    // which is what keeps a git linked worktree and an agent's per-project
    // state working.
    assert!(argv
        .windows(3)
        .any(|w| w[0] == "--bind" && w[1] == "/root/dev/app" && w[2] == "/root/dev/app"));
    // `none` is enforced by the namespace, not by the distro.
    assert!(argv.contains(&"--unshare-net".to_string()));
    // The agent's own argv is appended after bubblewrap's separator, unchanged.
    let end = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(&argv[end + 1..], ["claude", "--resume"]);
    // Nothing here names wsl.exe: reaching the distro is the transport's job.
    assert!(!argv.iter().any(|token| token.contains("wsl.exe")));
    cleanup();
}

/// Bubblewrap binds a path without `-try`, so one that is not in the distro is
/// a pane that dies with a mount error the moment it opens. Inside a place that
/// is the *default* case — a scratch or signal directory friring minted on the
/// Windows side is not in there at all — so it is named, with the way in.
#[test]
fn a_path_that_is_not_in_the_distro_refuses_the_launch_instead_of_dying_in_the_pane() {
    let backend = backend(registers(windows()));
    let profile = profile();
    backend.ensure_distro(&profile).unwrap();
    let policy = policy_for(&profile);
    // Exactly the shape a policy backend's launch carries: a per-session
    // scratch directory on the host that no distro has ever seen.
    let scratch = dirs::session_scratch_dir("s1")
        .unwrap()
        .display()
        .to_string();
    let launch = SandboxLaunch::new(&policy, "/root", "s1")
        .with_tmp_dir(&scratch)
        .with_place(PlaceLaunch { relay: None });
    let err = backend.wrap(vec!["claude".into()], &launch).unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, SandboxError::Refused { .. }), "{text}");
    assert!(text.contains(&scratch), "{text}");
    assert!(text.contains("place's own filesystem"), "{text}");
    assert!(text.contains("mounts no Windows drive"), "{text}");
    cleanup();
}

/// ADR-29 in a WSL place: the database is on the Windows filesystem, and a
/// distro friring registers mounts none of it.
#[test]
fn the_database_cannot_be_reached_from_a_distro_friring_registered() {
    // The hardening is the mechanism, and it is checked where it is written.
    assert!(plan::WSL_CONF_CONTENTS.contains("[automount]"));
    assert!(plan::WSL_CONF_CONTENTS.contains("enabled = false"));

    let backend = backend(registers(windows()));
    let profile = profile();
    backend.ensure_distro(&profile).unwrap();
    let policy = policy_for(&profile);
    let db = "C:/Users/me/AppData/Local/friring/friring.db";
    let launch = SandboxLaunch::new(&policy, "/root", "s1")
        .with_friring_db(db)
        .with_place(PlaceLaunch { relay: None });
    let argv = backend.wrap(vec!["claude".into()], &launch).unwrap();

    // Nothing in the composed command names the database, its directory, or any
    // Windows drive: there is no path from inside the distro to either.
    for named in [db, "C:/Users/me/AppData", "/mnt/c"] {
        assert!(
            !argv.iter().any(|token| token.contains(named)),
            "{named} reached the sandbox: {argv:?}"
        );
    }
    cleanup();
}

/// A launch composed without an ensured distro would run the agent on the
/// *host* under a profile that says otherwise — the same refusal the container
/// backend makes, for the same reason.
#[test]
fn a_launch_without_a_place_or_without_an_ensure_is_refused_rather_than_run() {
    let backend = backend(registers(windows()));
    let profile = profile();
    let policy = policy_for(&profile);

    let err = backend
        .wrap(
            vec!["claude".into()],
            &SandboxLaunch::new(&policy, "/root", "s1"),
        )
        .unwrap_err();
    assert!(matches!(err, SandboxError::Refused { .. }), "{err}");
    assert!(err.to_string().contains("without one"), "{err}");

    // A place, but no ensure in this run: friring does not know which
    // bubblewrap applies the profile inside the distro, and composing without
    // one is a distro the agent sees all of.
    let err = backend
        .wrap(
            vec!["claude".into()],
            &SandboxLaunch::new(&policy, "/root", "s1").with_place(PlaceLaunch { relay: None }),
        )
        .unwrap_err();
    assert!(err.to_string().contains("has not ensured"), "{err}");

    // And a policy resolved for another backend is refused before anything else
    // is considered.
    let elsewhere = profile.resolve(SandboxBackendKind::Bwrap, "/root").unwrap();
    let err = backend
        .wrap(
            vec!["claude".into()],
            &SandboxLaunch::new(&elsewhere, "/root", "s1"),
        )
        .unwrap_err();
    assert!(matches!(err, SandboxError::Unsupported { .. }), "{err}");
    cleanup();
}

/// The launch half of the egress refusal. A relay or a proxied mode reaching
/// this far means somebody wired one anyway, and starting the agent would leave
/// it believing it is filtered.
#[test]
fn a_filtered_launch_is_refused_at_the_wrap_as_well_as_at_the_ensure() {
    let backend = backend(registers(windows()));
    let mut filtered = profile();
    filtered.network_mode = NetworkMode::Allowlist;
    filtered.network_allow = vec!["api.anthropic.com".to_string()];
    let policy = policy_for(&filtered);
    let socket = ProxyEndpoint::UnixSocket {
        host_path: "C:/data/proxy.sock".to_string(),
        inside_path: "/run/friring/proxy.sock".to_string(),
    };
    let err = backend
        .wrap(
            vec!["claude".into()],
            &SandboxLaunch::new(&policy, "/root", "s1")
                .with_proxy(socket)
                .with_place(PlaceLaunch { relay: None }),
        )
        .unwrap_err();
    assert!(err.to_string().contains("egress proxy"), "{err}");

    // And a launch that carries a relay for a place that has nowhere to reach
    // one from is refused rather than started.
    let open = profile();
    let policy = policy_for(&open);
    let err = backend
        .wrap(
            vec!["claude".into()],
            &SandboxLaunch::new(&policy, "/root", "s1").with_place(PlaceLaunch {
                relay: Some(PlaceRelay {
                    program: "/usr/bin/friring-cli",
                    port: 8118,
                }),
            }),
        )
        .unwrap_err();
    assert!(err.to_string().contains("nowhere to reach one"), "{err}");
}

/// `wsl --unregister` destroys a distro's whole filesystem, so both halves of
/// ownership are re-checked immediately before it runs — and a distro that will
/// not say who it belongs to is left alone and reported rather than removed.
#[test]
fn reaping_destroys_only_a_distro_that_still_says_it_is_friring_s() {
    let ours = backend(marked(windows_listing(LIST_WITH_DISTRO)).with_command(
        &format!("{WSL_EXE} --unregister {DISTRO}"),
        ProbeOutput::success(""),
    ));

    // Listed by name alone — the enumeration starts nothing — and the marker is
    // read where it is worth a start, which is on the way to destroying one.
    assert_eq!(ours.live_places().unwrap(), [DISTRO]);
    assert_eq!(ours.owner_of(DISTRO).as_deref(), Some("dev"));
    assert!(ours.reap(DISTRO).is_ok());

    // A distro that is not friring's by name is never named to `--unregister`;
    // the stub scripts no such command, so one reaching it would fail here.
    let error = ours.reap(TEMPLATE).unwrap_err();
    assert!(error.contains("left alone"), "{error}");

    // Nor one that carries no marker — including a distro that will not start,
    // which is what "friring could not ask" looks like.
    let silent = backend(windows_listing(LIST_WITH_DISTRO));
    assert_eq!(silent.live_places().unwrap(), [DISTRO]);
    assert_eq!(silent.owner_of(DISTRO), None);
    let error = silent.reap(DISTRO).unwrap_err();
    assert!(error.contains("carries no friring marker"), "{error}");
}

/// Nothing may be ensured on a host without WSL, and the reason is the probe's
/// own sentence rather than a later, stranger failure.
#[test]
fn an_unavailable_wsl_refuses_before_it_touches_anything() {
    let bare = StubHost::new()
        .with_home("C:/Users/me")
        .with_binary("cmd.exe");
    let err = backend(bare).ensure(&profile()).unwrap_err();
    assert!(matches!(err, SandboxError::Unavailable { .. }), "{err}");
    assert!(err.to_string().contains("WSL is not installed"), "{err}");
}
