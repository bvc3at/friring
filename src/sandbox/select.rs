//! Backend selection: the `auto` ladder, and what it rejected on the way down.
//!
//! `docs/SANDBOX.md` §Backend selection fixes one order per OS, chosen so the
//! default favours startup latency, credential passthrough and zero image
//! maintenance. The result is never just a winner: every rung passed over
//! carries the probe's reason, because the session-creation step shows what is
//! available *and why the rest is not*.

use crate::sandbox::backend::{Availability, SandboxError, SandboxResult};
use crate::sandbox::probe::HostPlatform;
use crate::session::SandboxBackendKind;

/// The per-OS order `backend = "auto"` walks.
///
/// The WSL row is the doc's "Windows via WSL transport": `bwrap` inside the
/// distro first, because a distro's `--unshare-net` is the *only* per-sandbox
/// network namespace available there — all WSL distros share one utility VM,
/// one kernel and one network namespace, so nothing at the Windows layer can
/// firewall a single sandbox.
///
/// A **native Windows** host has no rungs at all, for the reason
/// [`NATIVE_WINDOWS`] gives. That is not the same as "no engine installed":
/// docker and podman may both be running, and neither can give this host a
/// boundary friring is willing to call one.
pub fn ladder(platform: HostPlatform) -> &'static [SandboxBackendKind] {
    use SandboxBackendKind::*;
    match platform {
        // `apple-container` needs Apple Silicon, and macOS 26 for isolated
        // networks — below either bar it is not a rung at all, so the ladder
        // does not offer it and the picker has nothing to explain away.
        HostPlatform::MacOs {
            apple_silicon: true,
            major,
        } if major >= 26 => &[Seatbelt, AppleContainer, Docker, Podman],
        HostPlatform::MacOs { .. } => &[Seatbelt, Docker, Podman],
        // Podman before Docker: rootless by default, no daemon and no root
        // socket, which matters most on the shared and remote hosts where
        // Linux sandboxes actually run.
        HostPlatform::Linux => &[Bwrap, Podman, Docker],
        HostPlatform::WslDistro => &[Bwrap, WslDistro, Docker],
        HostPlatform::Windows | HostPlatform::Unknown => &[],
    }
}

/// Why a native Windows host is offered no sandbox, and where its user gets
/// one instead.
///
/// The engines are installable there and the CLI runs, so this is a deliberate
/// withdrawal rather than a missing dependency. A place mounts every path at
/// **exactly its host path** (`docs/SANDBOX.md` §Identical absolute paths),
/// which is what keeps a git linked worktree pointing at its main repository
/// and an agent finding the transcript it keyed by project path — and a Linux
/// container cannot mount `C:\Users\me\repo` at `C:\Users\me\repo`. Nothing
/// about a translated path is a boundary friring can promise, so it says so
/// instead of building one that silently means something else.
///
/// WSL2 is not a workaround here, it is the supported shape: friring runs
/// inside the distro as a Linux binary, its paths are the distro's own, and the
/// whole Linux ladder applies (see [`HostPlatform::WslDistro`]).
pub const NATIVE_WINDOWS: &str = "friring does not sandbox on a native Windows host: a sandbox \
                                  mounts every path at exactly its host path, and no container \
                                  engine can do that with a Windows path. Run friring inside WSL2 \
                                  — there its paths are the distro's own and the Linux backends \
                                  (bwrap first) apply";

/// One rung the ladder passed over, with the probe's verdict verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedRung {
    pub backend: SandboxBackendKind,
    pub availability: Availability,
}

/// What selection decided, and everything it looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub platform: HostPlatform,
    /// What the profile asked for — [`SandboxBackendKind::Auto`] or a pin.
    pub requested: SandboxBackendKind,
    /// The winner, or `None` when nothing on the ladder was available.
    pub chosen: Option<SandboxBackendKind>,
    /// Rungs skipped because they were unavailable, in ladder order. A rung
    /// *below* the winner is not here: it was not rejected, only outranked.
    pub rejected: Vec<RejectedRung>,
}

impl Selection {
    /// The chosen backend, or an error naming every rung that failed and how to
    /// fix it.
    pub fn backend(&self) -> SandboxResult<SandboxBackendKind> {
        match self.chosen {
            Some(backend) => Ok(backend),
            None => Err(SandboxError::Unavailable {
                backend: self.requested,
                reason: self.rejection_summary(),
            }),
        }
    }

    /// Every rejection as one `backend: reason — fix` line per rung, for the
    /// picker's detail area and for the launch error.
    ///
    /// A host with no rungs has nothing to list, and "no sandbox backend exists
    /// for …" reads like a probe that came back empty. Native Windows says what
    /// it is instead: a decision, with the way to get a boundary anyway.
    pub fn rejection_summary(&self) -> String {
        if self.rejected.is_empty() {
            if self.platform == HostPlatform::Windows {
                return NATIVE_WINDOWS.to_string();
            }
            return format!("no sandbox backend exists for {}", self.platform.label());
        }
        self.rejected
            .iter()
            .map(|r| format!("{}: {}", r.backend, r.availability.message()))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Resolve `requested` against `platform`, asking `probe` about each candidate.
///
/// `probe` is injected so selection is testable against every host shape
/// without any of those hosts existing. It is called at most once per backend
/// and only until a winner is found, which is what keeps `auto` cheap.
///
/// A pinned backend is still probed: picking `bwrap` on a machine without it
/// must fail with bubblewrap's own actionable message, not with a silent
/// fallback to something the user did not choose. The one host where a pin is
/// not probed at all is native Windows — an installed engine there would answer
/// "available" to a question friring is not asking any more
/// ([`NATIVE_WINDOWS`]), and a pin must be refused for the same reason `auto`
/// finds nothing.
pub fn select_backend<P>(
    requested: SandboxBackendKind,
    platform: HostPlatform,
    mut probe: P,
) -> Selection
where
    P: FnMut(SandboxBackendKind) -> Availability,
{
    if platform == HostPlatform::Windows {
        return Selection {
            platform,
            requested,
            chosen: None,
            rejected: Vec::new(),
        };
    }
    let candidates: Vec<SandboxBackendKind> = if requested == SandboxBackendKind::Auto {
        ladder(platform).to_vec()
    } else {
        vec![requested]
    };

    let mut rejected = Vec::new();
    for backend in candidates {
        let availability = probe(backend);
        if availability.is_available() {
            return Selection {
                platform,
                requested,
                chosen: Some(backend),
                rejected,
            };
        }
        rejected.push(RejectedRung {
            backend,
            availability,
        });
    }
    Selection {
        platform,
        requested,
        chosen: None,
        rejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A probe that answers from a list and records what it was asked, so a
    /// test can assert the ladder stopped where it should have.
    fn probing<'a>(
        available: &'static [SandboxBackendKind],
        asked: &'a RefCell<Vec<SandboxBackendKind>>,
    ) -> impl FnMut(SandboxBackendKind) -> Availability + 'a {
        move |backend| {
            asked.borrow_mut().push(backend);
            if available.contains(&backend) {
                Availability::available("stub")
            } else {
                Availability::needs_fix(format!("{backend} is not installed"), "install it")
            }
        }
    }

    #[test]
    fn macos_ladder_depends_on_cpu_and_version() {
        let modern = HostPlatform::MacOs {
            apple_silicon: true,
            major: 26,
        };
        assert_eq!(
            ladder(modern),
            [
                SandboxBackendKind::Seatbelt,
                SandboxBackendKind::AppleContainer,
                SandboxBackendKind::Docker,
                SandboxBackendKind::Podman,
            ]
        );
        // Intel, or an older macOS, drops the Apple container rung entirely.
        for older in [
            HostPlatform::MacOs {
                apple_silicon: false,
                major: 26,
            },
            HostPlatform::MacOs {
                apple_silicon: true,
                major: 15,
            },
        ] {
            assert_eq!(
                ladder(older),
                [
                    SandboxBackendKind::Seatbelt,
                    SandboxBackendKind::Docker,
                    SandboxBackendKind::Podman,
                ]
            );
        }
    }

    #[test]
    fn linux_windows_and_wsl_ladders_match_the_design() {
        assert_eq!(
            ladder(HostPlatform::Linux),
            [
                SandboxBackendKind::Bwrap,
                SandboxBackendKind::Podman,
                SandboxBackendKind::Docker,
            ]
        );
        assert_eq!(
            ladder(HostPlatform::WslDistro),
            [
                SandboxBackendKind::Bwrap,
                SandboxBackendKind::WslDistro,
                SandboxBackendKind::Docker,
            ]
        );
        // A native Windows host has no rungs: an engine there would run its
        // container on a Linux kernel, which cannot mount a Windows path at
        // that same path — the invariant every place is built on.
        assert!(ladder(HostPlatform::Windows).is_empty());
        assert!(ladder(HostPlatform::Unknown).is_empty());
    }

    /// A native Windows host is refused whatever the profile asks for, and the
    /// refusal names WSL2 — where friring is a Linux binary and the whole Linux
    /// ladder applies.
    ///
    /// Both halves matter. `auto` must not read as a probe that came back empty
    /// (docker and podman are installable there, and the CLI answers), and a
    /// **pin** must not be probed at all: an installed engine would say
    /// "available" to a question friring no longer asks.
    #[test]
    fn a_native_windows_host_is_refused_and_told_where_a_boundary_lives() {
        for requested in [
            SandboxBackendKind::Auto,
            SandboxBackendKind::Docker,
            SandboxBackendKind::Podman,
            SandboxBackendKind::WslDistro,
        ] {
            let asked = RefCell::new(Vec::new());
            let selection = select_backend(
                requested,
                HostPlatform::Windows,
                probing(
                    &[
                        SandboxBackendKind::Docker,
                        SandboxBackendKind::Podman,
                        SandboxBackendKind::WslDistro,
                    ],
                    &asked,
                ),
            );
            assert_eq!(selection.chosen, None, "{requested}");
            assert!(asked.borrow().is_empty(), "{requested}: {asked:?}");
            let text = selection.backend().unwrap_err().to_string();
            assert!(text.contains("WSL2"), "{requested}: {text}");
            assert!(
                text.contains("exactly its host path"),
                "{requested}: {text}"
            );
        }
    }

    /// The rung the refusal points at is untouched: inside a distro friring is
    /// a Linux binary, and that is where a Windows user's boundary comes from.
    #[test]
    fn a_wsl_distro_host_still_gets_the_linux_ladder() {
        let asked = RefCell::new(Vec::new());
        let selection = select_backend(
            SandboxBackendKind::Auto,
            HostPlatform::WslDistro,
            probing(&[SandboxBackendKind::Bwrap], &asked),
        );
        assert_eq!(selection.chosen, Some(SandboxBackendKind::Bwrap));
    }

    #[test]
    fn auto_takes_the_first_available_rung_and_probes_no_further() {
        let asked = RefCell::new(Vec::new());
        let selection = select_backend(
            SandboxBackendKind::Auto,
            HostPlatform::Linux,
            probing(&[SandboxBackendKind::Bwrap], &asked),
        );
        assert_eq!(selection.chosen, Some(SandboxBackendKind::Bwrap));
        assert!(selection.rejected.is_empty());
        // The rungs below the winner are outranked, not rejected — probing them
        // would cost a `docker info` for nothing.
        assert_eq!(*asked.borrow(), [SandboxBackendKind::Bwrap]);
    }

    #[test]
    fn auto_records_every_rung_it_had_to_reject() {
        let asked = RefCell::new(Vec::new());
        let selection = select_backend(
            SandboxBackendKind::Auto,
            HostPlatform::Linux,
            probing(&[SandboxBackendKind::Docker], &asked),
        );
        assert_eq!(selection.chosen, Some(SandboxBackendKind::Docker));
        let names: Vec<SandboxBackendKind> = selection.rejected.iter().map(|r| r.backend).collect();
        assert_eq!(
            names,
            [SandboxBackendKind::Bwrap, SandboxBackendKind::Podman]
        );
        // Each rejection keeps its actionable text for the picker.
        assert_eq!(
            selection.rejected[0].availability.message(),
            "bwrap is not installed — install it"
        );
    }

    #[test]
    fn nothing_available_fails_with_every_reason() {
        let asked = RefCell::new(Vec::new());
        let selection = select_backend(
            SandboxBackendKind::Auto,
            HostPlatform::Linux,
            probing(&[], &asked),
        );
        assert_eq!(selection.chosen, None);
        let err = selection.backend().unwrap_err();
        let text = err.to_string();
        for backend in ["bwrap", "podman", "docker"] {
            assert!(text.contains(backend), "{text} should name {backend}");
        }
    }

    #[test]
    fn an_unknown_host_offers_no_rungs_and_says_so() {
        let asked = RefCell::new(Vec::new());
        let selection = select_backend(
            SandboxBackendKind::Auto,
            HostPlatform::Unknown,
            probing(&[SandboxBackendKind::Bwrap], &asked),
        );
        assert_eq!(selection.chosen, None);
        assert!(asked.borrow().is_empty());
        assert_eq!(
            selection.rejection_summary(),
            "no sandbox backend exists for unknown host"
        );
    }

    #[test]
    fn a_pinned_backend_is_probed_and_never_falls_back() {
        let asked = RefCell::new(Vec::new());
        // bwrap is missing but podman is there: a pinned bwrap must still fail.
        let selection = select_backend(
            SandboxBackendKind::Bwrap,
            HostPlatform::Linux,
            probing(&[SandboxBackendKind::Podman], &asked),
        );
        assert_eq!(selection.chosen, None);
        assert_eq!(*asked.borrow(), [SandboxBackendKind::Bwrap]);
        assert!(selection
            .backend()
            .unwrap_err()
            .to_string()
            .contains("bwrap is not installed"));

        // And a pin that *is* available wins without consulting the ladder.
        let asked = RefCell::new(Vec::new());
        let selection = select_backend(
            SandboxBackendKind::Podman,
            HostPlatform::Linux,
            probing(&[SandboxBackendKind::Podman], &asked),
        );
        assert_eq!(selection.chosen, Some(SandboxBackendKind::Podman));
        assert_eq!(*asked.borrow(), [SandboxBackendKind::Podman]);
    }
}
