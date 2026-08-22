//! The `wsl-distro` place backend: one cloned WSL distro per profile.
//!
//! A **place** is an environment that outlives an individual command (ADR-26),
//! created once per profile and reached through a transport with tmux running
//! inside it. Here the transport is one friring already has — `wsl.exe -d
//! <distro>`, the same launcher a `wsl:` host is reached by — so this backend
//! adds a lifecycle and no plumbing:
//!
//! ```text
//! friring
//!   └─ tmux (host, control mode)
//!        └─ wsl.exe -d friring-sbx-dev tmux …      ← the transport
//!             └─ tmux (in the distro)
//!                  └─ bwrap … agent                ← [`SandboxBackend::wrap`]
//! ```
//!
//! That last line is the decisive one, and it is why this backend composes an
//! argv at all. **All WSL distros share one utility VM, one kernel and one
//! network namespace**: Hyper-V firewall rules scope to the whole VM and take
//! addresses rather than names, and `iptables` set in one distro applies to
//! every distro on the machine. A distro is therefore a filesystem boundary and
//! an identity, and *not* a network boundary — so the per-sandbox network
//! namespace comes from running the [`bwrap`] backend **inside** the distro,
//! whose `--unshare-net` is real. The same bubblewrap applies
//! the profile's per-path read-only/read-write intent, at identical absolute
//! paths, which a distro alone cannot express either.
//!
//! Three consequences the design records and this module refuses to paper over:
//!
//! - **Memory and CPU caps are global to the utility VM** (`.wslconfig`), so a
//!   profile carrying one is refused rather than launched with a limit friring
//!   cannot apply.
//! - **WSL1 is not a boundary at all** — no utility VM, no namespaces — and is
//!   rejected explicitly, at the probe and again at the template.
//! - **Repositories belong on the distro's ext4.** A Windows-side path costs
//!   10–100× on metadata and sits outside the distro's filesystem, and the
//!   hardened template mounts no Windows drive at all, so friring warns.

pub mod plan;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::sandbox::backend::{
    Argv, Availability, Caps, InnerSandboxVerdict, ProxyTransport, SandboxBackend, SandboxError,
    SandboxLaunch, SandboxResult,
};
use crate::sandbox::egress::proxy_required;
use crate::sandbox::probe::{detect_platform, HostPlatform, ProbeHost, ProbeOutput};
use crate::sandbox::{bwrap, dirs};
use crate::session::{
    NetworkMode, ReadScope, SandboxBackendKind, SandboxInstance, SandboxProfile, SandboxShape,
};

pub use plan::{distro_name, DistroInfo, DISTRO_PREFIX, MARKER_FILE, WSL_CONF};

/// The name looked up on `PATH`.
///
/// A lookup key and never what is executed: the probe resolves it to an absolute
/// path once and refuses one a sandboxed agent could rewrite, exactly as
/// [`bwrap::BWRAP`] and the container engines do. `wsl.exe` registers and
/// destroys distros, so choosing which copy of it runs is choosing what the
/// boundary is.
pub const WSL: &str = "wsl.exe";

/// The WSL release that added `--export --format vhd` and `--import --vhd`.
///
/// The tar path predates it and still works, but restoring a distro from a tar
/// unpacks it file by file into a fresh image — minutes where a VHD copy is
/// seconds — so friring asks for the version that has the fast one rather than
/// shipping two lifecycles.
const VHD_SINCE: (u32, u32) = (2, 0);

/// The network modes a WSL place can actually enforce.
///
/// `none` is real: `bwrap --unshare-net` inside the distro gives the sandbox its
/// own empty network namespace, which is the one thing the shared utility VM
/// cannot take away. `allowlist` — and a `full` carrying denies — are enforced
/// by friring's egress proxy over a unix socket bind-mounted across the boundary
/// (ADR-27), and a distro's filesystem is inside the VM while friring is on the
/// Windows side of it, with no shared socket between the two. So the mode is
/// declared unavailable and a launch that asks for one is refused, rather than
/// started believing it is filtered.
const ENFORCEABLE_MODES: &[NetworkMode] = &[NetworkMode::None, NetworkMode::Full];

/// The `state` a `sandbox_instances` row carries for a distro this backend
/// handed back. The only one it writes: [`WslDistroBackend::ensure_distro`]
/// returns when the distro is registered and answering, or not at all.
pub const INSTANCE_STATE_RUNNING: &str = "running";

/// What the probe learned about WSL on one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslDetails {
    pub availability: Availability,
    /// The absolute `wsl.exe` the probe resolved and vetted, or `None` when the
    /// backend is unusable. This — never [`WSL`] — is what runs.
    pub program: Option<String>,
    /// `(major, minor)` of `wsl --version`, or `None` when it could not be read.
    pub version: Option<(u32, u32)>,
}

/// A registered distro, with everything a launch into it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsuredDistro {
    /// What the caller records in `sandbox_instances`.
    pub instance: SandboxInstance,
    /// The distro name, which is also what the `wsl:` transport addresses.
    pub distro: String,
    /// Absolute path of bubblewrap **inside** the distro. The boundary the
    /// profile's paths and its network mode are actually applied by.
    pub bwrap_program: String,
    /// `$HOME` inside the distro. The profile's `~` expands against this and
    /// nothing else: a place's paths are the *place's*, and expanding them
    /// against the Windows home would name a directory that does not exist in
    /// there.
    pub home: String,
    /// What friring will honour but would rather the user changed — a
    /// Windows-side path, today. Never a reason to refuse; see
    /// [`plan::windows_side_paths`].
    pub warnings: Vec<String>,
}

/// One cloned WSL distro per profile.
pub struct WslDistroBackend {
    host: Arc<dyn ProbeHost>,
    details: OnceLock<WslDetails>,
    /// Bubblewrap inside each distro this friring has ensured.
    ///
    /// Pinned when the distro is ensured rather than resolved again per launch,
    /// for the reason the container backend pins its relay: it is an answer only
    /// the inside of the place can give, and asking again on every launch would
    /// let an agent with a writable `PATH` choose which program applies its own
    /// boundary. A launch that finds no entry is refused, because composing
    /// without one would be a distro the agent sees all of.
    bwrap_programs: Mutex<HashMap<String, String>>,
}

impl WslDistroBackend {
    pub fn new(host: Arc<dyn ProbeHost>) -> Self {
        Self {
            host,
            details: OnceLock::new(),
            bwrap_programs: Mutex::new(HashMap::new()),
        }
    }

    /// The probe's full answer, cached with the availability.
    pub fn details(&self) -> &WslDetails {
        self.details.get_or_init(|| self.run_probe())
    }

    /// The vetted absolute `wsl.exe`, or the probe's own reason.
    ///
    /// # Errors
    ///
    /// WSL is unusable on this host; the message is the probe's.
    fn program(&self) -> SandboxResult<&str> {
        let details = self.details();
        details
            .program
            .as_deref()
            .ok_or_else(|| SandboxError::Unavailable {
                backend: SandboxBackendKind::WslDistro,
                reason: details.availability.message(),
            })
    }

    fn run_probe(&self) -> WslDetails {
        let unavailable = |availability| WslDetails {
            availability,
            program: None,
            version: None,
        };
        match detect_platform(self.host.as_ref()) {
            HostPlatform::Windows => {}
            // The rung is reachable only from the Windows side. Registering a
            // distro is a Windows-side operation, and the distro friring would
            // be running in is the wrong side of the boundary to perform it from
            // — while the rung *above* this one on the same ladder is bwrap,
            // which is both available in here and the only per-sandbox network
            // namespace WSL has.
            HostPlatform::WslDistro => {
                return unavailable(Availability::needs_fix(
                    "this host is a WSL distro, and registering another distro is a Windows-side \
                     operation",
                    "run friring on the Windows side for wsl-distro places, or use the bwrap \
                     backend in here — which is the ladder's first rung and the only per-sandbox \
                     network namespace WSL has",
                ))
            }
            other => {
                return unavailable(Availability::unavailable(format!(
                    "the wsl-distro backend needs Windows; this host is {}",
                    other.label()
                )))
            }
        }

        let Some(program) = self.host.which(WSL) else {
            return unavailable(Availability::needs_fix(
                "WSL is not installed",
                "install it: wsl --install",
            ));
        };
        // The program that registers and destroys distros must not be one the
        // sandbox can replace — the rule bubblewrap and the container engines
        // apply to themselves, for the same reason.
        if let Some(root) = dirs::rewritable_root(&program, self.host.home().as_deref()) {
            return unavailable(Availability::needs_fix(
                format!(
                    "wsl.exe resolves to '{program}', inside '{root}' — a sandboxed agent could \
                     replace it and the next launch would run whatever it planted"
                ),
                "take the writable copy off PATH; wsl.exe belongs in the system directory",
            ));
        }

        let version = self
            .run_wsl(&program, &["--version"])
            .ok()
            .filter(ProbeOutput::ok)
            .and_then(|output| {
                let decoded = plan::decode(&output.stdout);
                // The first line is the WSL version and the ones under it are
                // the kernel's, WSLg's and Windows' own, so the parse is given
                // that line alone. `bwrap`'s parser is reused deliberately: the
                // question is the same one ("the first `major.minor` in this
                // line"), and a second copy of it is a second thing to keep
                // right.
                bwrap::parse_version(decoded.lines().next().unwrap_or_default())
            });
        let Some(version) = version.filter(|found| *found >= VHD_SINCE) else {
            let (major, minor) = VHD_SINCE;
            return unavailable(Availability::needs_fix(
                format!(
                    "this host's WSL is older than {major}.{minor}, which is where exporting a \
                     distro as a VHD and importing one back arrived"
                ),
                "update WSL from the Microsoft Store: wsl --update",
            ));
        };

        let distros = match self.list_with(&program) {
            Ok(distros) => distros,
            Err(reason) => return unavailable(Availability::unavailable(reason)),
        };
        if distros.is_empty() {
            return unavailable(Availability::needs_fix(
                "this host has no WSL distro for friring to clone a sandbox from",
                "install one: wsl --install -d Ubuntu",
            ));
        }
        if distros.iter().all(|distro| distro.version == 1) {
            return unavailable(Availability::needs_fix(
                "WSL1 is not supported: every distro on this host runs as WSL1, which has no \
                 utility VM, no namespaces to isolate anything with, and cannot be exported as a \
                 VHD",
                "convert one: wsl --set-version <distro> 2",
            ));
        }

        let (major, minor) = version;
        WslDetails {
            availability: Availability::available(format!("WSL {major}.{minor}")),
            program: Some(program),
            version: Some(version),
        }
    }

    /// Make sure `profile`'s distro is registered, hardened and answering, and
    /// hand back everything a launch into it needs.
    ///
    /// Idempotent, and cheap when the distro is already there: one
    /// `--list --verbose` and a handful of commands inside it. A distro friring
    /// did not register is neither adopted nor destroyed — the launch is refused
    /// instead, exactly as the container backend refuses a same-named container
    /// without its owner label.
    ///
    /// # Errors
    ///
    /// WSL is unavailable; the profile asks for something a WSL place cannot
    /// enforce (a memory or CPU cap, a filtered network mode, a containerfile);
    /// the template is missing or is WSL1; the distro exists and is not
    /// friring's; the export or import failed; the distro carries no
    /// bubblewrap, which is what would apply the profile's paths; or one of the
    /// profile's read-write paths encloses that bubblewrap, which would let the
    /// sandbox replace the program applying its own boundary.
    pub fn ensure_distro(&self, profile: &SandboxProfile) -> SandboxResult<EnsuredDistro> {
        let refuse = |detail: String| SandboxError::Refused {
            profile: profile.name.clone(),
            detail,
        };
        // Before anything is registered: an unavailable WSL must fail with the
        // probe's own actionable sentence rather than with whatever the first
        // command it could not run said.
        self.program()?;
        check_enforceable(profile, &refuse)?;

        // Trimmed, because `SandboxPolicy::profile` is — and a launch composes
        // the distro name from *that*, so an untrimmed name here would leave
        // `wrap` looking for a distro nothing ensured.
        let distro = plan::distro_name(profile.name.trim());
        let listed = self.list_distros()?;
        match listed.iter().find(|found| found.name == distro) {
            // The template is resolved only where one is needed. A place that
            // already exists does not need the distro it was cloned from, and
            // refusing to reuse it because that distro has since been deleted
            // would take a running profile away over an irrelevance.
            Some(found) => self.adopt(found, profile, &refuse)?,
            None => {
                let template = self.template(&listed, profile, &refuse)?;
                self.register(&distro, &template, profile, &refuse)?;
            }
        }

        // `$HOME` comes from the distro rather than from Windows: a place's
        // paths are the place's, and the profile's `~` means nothing else in
        // here.
        let home = self.inside_home(&distro, &refuse)?;
        let policy = profile
            .resolve(SandboxBackendKind::WslDistro, &home)
            .map_err(|detail| refuse(detail.to_string()))?;
        let bwrap = self.inside_bwrap(&distro, &home, &refuse)?;
        check_bwrap_containment(&policy.rw_paths, &distro, &bwrap, &refuse)?;

        self.bwrap_programs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(distro.clone(), bwrap.clone());

        Ok(EnsuredDistro {
            instance: SandboxInstance {
                profile: policy.profile.clone(),
                engine: SandboxBackendKind::WslDistro,
                external_id: distro.clone(),
                state: INSTANCE_STATE_RUNNING.to_string(),
            },
            distro,
            bwrap_program: bwrap,
            home,
            warnings: plan::windows_side_paths(&policy),
        })
    }

    /// The template distro `profile` is cloned from: its `image`, or this host's
    /// default distro.
    ///
    /// Two refusals, and the second is the one that matters. A **WSL1** template
    /// has no VHD to export and no VM to isolate anything with. And a template
    /// that is itself one of friring's places would carry that profile's whole
    /// filesystem — its agent's login among it — into a second boundary, which
    /// is exactly the copy ADR-28 exists to prevent.
    fn template(
        &self,
        listed: &[DistroInfo],
        profile: &SandboxProfile,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        let requested = profile
            .image
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let found = match requested {
            Some(name) => listed
                .iter()
                .find(|distro| distro.name == name)
                .ok_or_else(|| {
                    refuse(format!(
                        "this profile clones the WSL distro '{name}', which is not registered on \
                         this host. Install it, or name one of: {}",
                        available_names(listed)
                    ))
                })?,
            None => listed
                .iter()
                .find(|distro| distro.default && !plan::looks_like_ours(&distro.name))
                .ok_or_else(|| {
                    refuse(format!(
                        "this host has no default WSL distro for friring to clone, so the profile \
                         has to name one of: {}",
                        available_names(listed)
                    ))
                })?,
        };
        if found.version != 2 {
            return Err(refuse(format!(
                "WSL1 is not supported: the template distro '{}' runs as WSL1, which has no \
                 utility VM and cannot be exported as a VHD. Convert it: wsl --set-version '{}' 2",
                found.name, found.name
            )));
        }
        if plan::looks_like_ours(&found.name) {
            return Err(refuse(format!(
                "'{}' is one of friring's own sandbox distros, and cloning it would copy that \
                 profile's whole filesystem — the agent's login in it included — into a second \
                 boundary (ADR-28). Name a distro of your own as the template",
                found.name
            )));
        }
        Ok(found.name.clone())
    }

    /// Reuse a distro friring already registered for this profile.
    ///
    /// Ownership is checked before anything is reused, because the alternative
    /// is friring adopting — and later destroying — a distro somebody else
    /// registered under a name that happens to match.
    fn adopt(
        &self,
        found: &DistroInfo,
        profile: &SandboxProfile,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        if found.version != 2 {
            return Err(refuse(format!(
                "WSL1 is not supported: the distro '{}' runs as WSL1. Remove it and let friring \
                 register it again: wsl --unregister '{}'",
                found.name, found.name
            )));
        }
        // Asked before the marker, so a distro that will not run at all is
        // reported as that rather than as one carrying no marker — the two need
        // completely different things from the user.
        if let Err(detail) = self.started(&found.name) {
            return Err(refuse(format!(
                "the distro '{}' would not start: {detail}",
                found.name
            )));
        }
        match self.read_marker(&found.name) {
            Some(owner) if owner == profile.name.trim() => {}
            Some(owner) => {
                return Err(refuse(format!(
                    "the distro '{}' was registered by friring for the profile '{owner}', not for \
                     this one. Rename one of the two profiles",
                    found.name
                )))
            }
            None => {
                return Err(refuse(format!(
                    "a distro named '{}' already exists and was not registered by friring, so it \
                     is neither reused nor destroyed. Rename that distro, or rename the profile",
                    found.name
                )))
            }
        }
        self.verify_hardening(&found.name, refuse)
    }

    /// Refuse a distro whose `/etc/wsl.conf` is no longer the one friring wrote.
    ///
    /// That file is what removes the *default* exposure: with `automount` back
    /// on, every Windows drive — and with it friring's data directory and the
    /// database ADR-29 keeps out of every sandbox — is mounted inside the place;
    /// with `interop` back on, a process in there can `execve` a Windows binary
    /// that runs outside the VM altogether.
    ///
    /// **What this proves, exactly.** WSL reads `/etc/wsl.conf` when a distro
    /// *starts*, so a file check is a statement about the next start rather than
    /// about a running instance. On the registering path that is the whole
    /// story: [`finish`](Self::finish) stops the distro and refuses when the
    /// stop failed, so the instance a launch would meet is one that started from
    /// exactly these bytes. On the adopting path the distro may already be
    /// running, and friring does not restart it to find out — it cannot ask
    /// `wsl --list` either, whose states are localised strings nothing here
    /// compares (see [`plan::DistroInfo::state`]). So this is a **tamper
    /// detector on the file**, not a measurement of a live kernel.
    ///
    /// It is also not a containment boundary. A process that is already root
    /// inside the distro — which the distro's own user is — can mount a Windows
    /// drive by hand whatever `wsl.conf` says. Per-path containment inside a
    /// distro is bubblewrap's, which is why it is required rather than optional;
    /// this keeps the *default* filesystem and the interop bridge out of a
    /// freshly registered place, and says when somebody changed friring's answer
    /// to that.
    ///
    /// Classified as interference rather than as a profile a user should edit,
    /// so it never routes through `allow_unsandboxed_fallback`: answering "the
    /// boundary friring builds the next launch out of has been changed" with
    /// "so run on the host instead" would make breaking the sandbox the way out
    /// of it.
    fn verify_hardening(
        &self,
        distro: &str,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        let found = self.inside_read(distro, plan::WSL_CONF).unwrap_or_default();
        if found.trim_end() == plan::WSL_CONF_CONTENTS.trim_end() {
            return Ok(());
        }
        Err(refuse(format!(
            "the distro '{distro}' no longer carries the '{}' friring wrote, which is what keeps \
             the Windows filesystem out of the place and stops a process in it running a Windows \
             binary outside the VM. Remove the distro and let friring register it again: wsl \
             --unregister '{distro}'",
            plan::WSL_CONF
        ))
        .tampered())
    }

    /// Clone the template into a new distro, harden it, and mark it as
    /// friring's.
    ///
    /// The order is load-bearing. The hardening is written *before* the distro
    /// is ever used, and `--terminate` follows it because WSL reads
    /// `/etc/wsl.conf` when a distro starts — so the first process friring runs
    /// in there already has no Windows drives and no interop. The marker goes in
    /// with it, in one script, so a distro that exists is a distro that was
    /// finished.
    fn register(
        &self,
        distro: &str,
        template: &str,
        profile: &SandboxProfile,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        let place = dirs::place_dir(profile.name.trim()).ok_or_else(|| {
            refuse(
                "friring could not resolve its data directory, so it has nowhere to keep the \
                 distro it would register"
                    .to_string(),
            )
        })?;
        dirs::create_private_dir(&place)?;
        let install = dirs::create_private_dir_under(&place, "distro")?;
        // The staging VHD needs none of the symlink care friring takes over a
        // file it writes itself: this tree is under the data directory, which no
        // profile may name in either mode, so nothing inside any boundary can
        // reach the path to plant anything at it.
        let vhd = place.join("template.vhdx");
        let (install, vhd_path) = (representable(&install)?, representable(&vhd)?);

        let exported = self.wsl_run(&plan::export_argv(template, &vhd_path));
        if let Err(error) = outcome(exported, refuse, || {
            format!("WSL could not export the distro '{template}' as a VHD")
        }) {
            // A failed export leaves a partial image, and a template's worth of
            // one is gigabytes.
            let _ = std::fs::remove_file(&vhd);
            return Err(error);
        }
        let imported = self.wsl_run(&plan::import_argv(distro, &install, &vhd_path));
        // `--import --vhd` copies the image into the install directory, so the
        // staging copy is dead weight either way.
        let _ = std::fs::remove_file(&vhd);
        outcome(imported, refuse, || {
            format!(
                "WSL could not import the sandbox distro '{distro}' from '{install}' — clear that \
                 directory if a previous attempt left an image in it"
            )
        })?;

        if let Err(error) = self.finish(distro, profile, refuse) {
            // Registered moments ago, in this call, and never handed to a
            // launch: a distro that exists has to be one that was finished, or
            // the next ensure finds an unhardened distro with no marker and can
            // neither adopt it nor remove it.
            let _ = self.wsl_run(&["--unregister", distro]);
            return Err(error);
        }
        Ok(())
    }

    /// Harden a freshly imported distro, mark it as friring's, and check that
    /// both stuck.
    ///
    /// The stop between the two is **checked**, and that is the whole reason
    /// [`verify_hardening`](Self::verify_hardening) means anything here. WSL
    /// applies `/etc/wsl.conf` when a distro starts, and writing the file
    /// started this one — so without a stop that friring knows succeeded, the
    /// bytes on disk say "hardened" while the instance every later command
    /// reaches is the unhardened one the import left running, with every
    /// Windows drive in it. Reading the file back would confirm the file and
    /// nothing else: a verification against a distro that may still be running
    /// is not a verification.
    fn finish(
        &self,
        distro: &str,
        profile: &SandboxProfile,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        let hardened = self.inside_root(distro, &plan::harden_script(profile.name.trim()));
        outcome(hardened, refuse, || {
            format!("friring could not harden the sandbox distro '{distro}'")
        })?;
        let stopped = self.wsl_run(&["--terminate", distro]);
        outcome(stopped, refuse, || {
            format!(
                "friring hardened the sandbox distro '{distro}' but could not stop it, and WSL \
                 reads '{}' only when a distro starts — so the running distro still has every \
                 Windows drive mounted and the interop bridge open. Stop it and let friring \
                 register it again: wsl --terminate '{distro}'",
                plan::WSL_CONF
            )
        })?;
        self.verify_hardening(distro, refuse)
    }

    /// `$HOME` inside the distro, which every `~` in the profile expands
    /// against.
    fn inside_home(
        &self,
        distro: &str,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        self.inside_run(distro, &["sh", "-c", "printf %s \"$HOME\""])
            .ok()
            .filter(ProbeOutput::ok)
            .map(|output| plan::decode(output.trimmed()))
            .filter(|home| home.starts_with('/'))
            .ok_or_else(|| {
                refuse(format!(
                    "the distro '{distro}' did not answer with a home directory, and a profile's \
                     paths are written relative to one"
                ))
            })
    }

    /// Bubblewrap inside the distro — the program that actually applies this
    /// profile.
    ///
    /// Required unconditionally, not only for a filtered network mode: a distro
    /// is one filesystem and one identity, so without bwrap the agent would see
    /// all of it read-write whatever the profile's paths say, which is a
    /// boundary that grants more than the words it was written with. Resolved
    /// once here and pinned, and refused where it sits under one of the fixed
    /// prefixes anything can write ([`dirs::rewritable_root`]).
    ///
    /// That is half the rule every backend applies to the binary that *is* its
    /// boundary; the other half asks whether this **profile's own** read-write
    /// paths enclose it, and is [`check_bwrap_containment`].
    fn inside_bwrap(
        &self,
        distro: &str,
        home: &str,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<String> {
        let resolved = self
            .inside_run(distro, &["sh", "-c", "command -v bwrap"])
            .ok()
            .filter(ProbeOutput::ok)
            .map(|output| plan::decode(output.trimmed()))
            .filter(|path| path.starts_with('/'));
        let program = resolved.ok_or_else(|| {
            refuse(format!(
                "the distro '{distro}' carries no bubblewrap, which is what applies this \
                 profile's paths and its network mode inside a WSL place — all WSL distros share \
                 one network namespace, so a distro on its own is not a boundary. Install it in \
                 there: wsl -d '{distro}' -u root -e apt install bubblewrap"
            ))
        })?;
        if let Some(root) = dirs::rewritable_root(&program, Some(home)) {
            return Err(refuse(format!(
                "bubblewrap in the distro '{distro}' resolves to '{program}', inside '{root}' — a \
                 sandboxed agent could replace it and the next launch would run unwrapped. \
                 Install it system-wide in there and take the writable copy off PATH"
            )));
        }
        Ok(program)
    }

    /// Every distro on this host, as `wsl --list --verbose` describes them.
    ///
    /// # Errors
    ///
    /// WSL is unavailable, or would not list its distros.
    pub fn list_distros(&self) -> SandboxResult<Vec<DistroInfo>> {
        let program = self.program()?;
        self.list_with(program)
            .map_err(|reason| SandboxError::Unavailable {
                backend: SandboxBackendKind::WslDistro,
                reason,
            })
    }

    fn list_with(&self, program: &str) -> Result<Vec<DistroInfo>, String> {
        let output = self
            .run_wsl(program, &["--list", "--verbose"])
            .map_err(|detail| format!("wsl.exe could not be run: {detail}"))?;
        if !output.ok() {
            return Err(first_line(
                &output.stderr,
                "WSL would not list this host's distros",
            ));
        }
        Ok(plan::parse_list(&output.stdout))
    }

    /// The distros friring could have registered, for the pass that reclaims
    /// them.
    ///
    /// Filtered on the name prefix alone, and deliberately: the other half of
    /// ownership is a file *inside* the distro, and reading it starts the distro
    /// — seconds and a slice of the utility VM's memory, for every candidate, on
    /// a background pass that mostly reclaims nothing. Which profile a distro
    /// belongs to is a question a caller can answer without starting anything,
    /// by asking [`plan::distro_name`] for each profile it knows; the marker is
    /// read where it is worth the cost, immediately before [`reap`](Self::reap)
    /// destroys something.
    ///
    /// # Errors
    ///
    /// WSL is unavailable, or would not list its distros.
    pub fn live_places(&self) -> SandboxResult<Vec<String>> {
        Ok(self
            .list_distros()?
            .into_iter()
            .map(|distro| distro.name)
            .filter(|name| plan::looks_like_ours(name))
            .collect())
    }

    /// The profile a distro's marker names, or `None` for one that does not
    /// carry friring's — which includes a distro that will not start.
    ///
    /// Starts the distro, so it is a question to ask about one distro rather
    /// than about every distro on a machine (see [`live_places`](Self::live_places)).
    pub fn owner_of(&self, distro: &str) -> Option<String> {
        self.read_marker(distro)
    }

    /// Destroy the distro `name`, answering with the reason when it stays.
    ///
    /// `wsl --unregister` deletes a distro's entire filesystem, so this is the
    /// one command in this feature that can destroy something a user cares
    /// about. Both halves of ownership are therefore re-checked immediately
    /// before it runs, and a distro that will not say who it belongs to is left
    /// alone and reported rather than removed: "friring could not ask" has to
    /// read as "it may not be friring's".
    pub fn reap(&self, name: &str) -> Result<(), String> {
        if !plan::looks_like_ours(name) {
            return Err(format!(
                "{name}: not a distro friring registers, so it was left alone"
            ));
        }
        if self.read_marker(name).is_none() {
            return Err(format!(
                "{name}: carries no friring marker, so it was left alone"
            ));
        }
        match self.wsl_run(&["--unregister", name]) {
            Ok(output) if output.ok() => Ok(()),
            Ok(output) => Err(format!(
                "{name}: {}",
                first_line(&output.stderr, "WSL gave no reason")
            )),
            Err(detail) => Err(format!("{name}: {detail}")),
        }
    }

    /// Refuse a launch naming a path that is not in the distro's filesystem.
    ///
    /// Bubblewrap binds a profile's paths without `-try`, so a missing one is a
    /// pane that dies the instant it opens with a mount error in it. Inside a
    /// place that is not a rare mistake but the default case: every path a
    /// launch carries is judged against the **distro's** filesystem, and a
    /// host-side directory friring minted for a *policy* sandbox — a scratch
    /// directory, a signal directory, anything on a Windows drive — is simply
    /// not in there. Naming it, with the way in, is what turns that into
    /// something a user can act on.
    fn check_present(
        &self,
        distro: &str,
        launch: &SandboxLaunch<'_>,
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        for path in launch
            .writable_paths()
            .into_iter()
            .chain(launch.readable_paths())
        {
            if self.inside_exists(distro, &path) {
                continue;
            }
            return Err(refuse(format!(
                "'{path}' does not exist inside the WSL distro '{distro}', and a place's paths \
                 are paths in the place's own filesystem — a Windows-side one is not there at \
                 all, because a distro friring registers mounts no Windows drive. Create it in \
                 there (wsl -d '{distro}') or take it out of the profile: a place that silently \
                 dropped a path would be a boundary nobody can reason about"
            )));
        }
        Ok(())
    }

    /// Whether the distro runs a command at all, with WSL's own reason when it
    /// does not.
    fn started(&self, distro: &str) -> Result<(), String> {
        match self.inside_run(distro, &["true"]) {
            Ok(output) if output.ok() => Ok(()),
            Ok(output) => Err(first_line(&output.stderr, "WSL gave no reason")),
            Err(detail) => Err(detail),
        }
    }

    /// The profile a distro says it was registered for, or `None` for one that
    /// carries no marker — which includes one that will not start.
    fn read_marker(&self, distro: &str) -> Option<String> {
        self.inside_read(distro, plan::MARKER_FILE)
            .map(|text| text.trim().to_string())
            .filter(|owner| !owner.is_empty())
    }

    /// Read a file inside a distro, or `None` when it is not there.
    fn inside_read(&self, distro: &str, path: &str) -> Option<String> {
        self.inside_run(distro, &["cat", path])
            .ok()
            .filter(ProbeOutput::ok)
            .map(|output| plan::decode(&output.stdout))
    }

    /// Whether `path` exists **inside** the distro, which is the only
    /// filesystem a WSL place's mount plan is about.
    fn inside_exists(&self, distro: &str, path: &str) -> bool {
        self.inside_run(distro, &["test", "-e", path])
            .is_ok_and(|output| output.ok())
    }

    /// Run a command inside `distro` as its default user.
    ///
    /// `--exec` rather than a bare `--`, so the argv is handed to `execve`
    /// instead of to a login shell that would re-split it.
    fn inside_run(&self, distro: &str, argv: &[&str]) -> Result<ProbeOutput, String> {
        let mut args = vec!["-d", distro, "--exec"];
        args.extend_from_slice(argv);
        self.wsl_run(&args)
    }

    /// Run one `sh -c` script inside `distro` as root — what writing
    /// `/etc/wsl.conf` and the marker needs, and nothing else does.
    fn inside_root(&self, distro: &str, script: &str) -> Result<ProbeOutput, String> {
        self.wsl_run(&["-d", distro, "-u", "root", "--exec", "sh", "-c", script])
    }

    fn wsl_run(&self, args: &[&str]) -> Result<ProbeOutput, String> {
        let program = self.program().map_err(|error| error.to_string())?;
        self.run_wsl(program, args)
    }

    fn run_wsl(&self, program: &str, args: &[&str]) -> Result<ProbeOutput, String> {
        self.host.run(program, args)
    }
}

impl SandboxBackend for WslDistroBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::WslDistro
    }

    fn probe(&self) -> Availability {
        self.details().availability.clone()
    }

    fn capabilities(&self) -> Caps {
        Caps {
            shape: SandboxShape::Place,
            // Every distro shares one utility VM, and a cap on it is set in the
            // machine-wide `.wslconfig` rather than per distro. Reported as
            // unavailable rather than accepted and ignored, which is what
            // `ensure` then refuses a profile carrying one for.
            limits: false,
            network_modes: ENFORCEABLE_MODES,
            // A place has no host filesystem to read: `host-minus-secrets` is a
            // statement about the machine friring runs on, and a distro is a
            // filesystem of its own.
            read_scopes: &[ReadScope::Workspace],
            persistent: true,
            // The Windows credential store is on the other side of the utility
            // VM, so a place signs in inside itself (ADR-28).
            host_credentials: false,
            inner_agent_sandbox: InnerSandboxVerdict::Redundant,
            // The bwrap that runs inside the distro is what has a network
            // namespace, and a namespaced sandbox can only be handed a socket
            // (ADR-27). Which is also why a filtered mode is refused for now:
            // the socket would have to be inside the distro, and friring is on
            // the Windows side of the VM.
            proxy_transport: ProxyTransport::UnixSocket,
        }
    }

    /// Compose the command that runs **inside** the distro: bubblewrap around
    /// the agent.
    ///
    /// The place backend's half of the argv seam (ADR-26). Nothing here names
    /// `wsl.exe` — reaching the distro is the transport's job — and everything
    /// that makes this a boundary at all is in the argv it returns: the
    /// profile's paths bound at identical absolute paths, `.git/hooks` kept
    /// read-only inside every writable root, and `--unshare-net` for a mode that
    /// says no network. A distro alone has none of that.
    ///
    /// # Errors
    ///
    /// The policy was resolved for another backend, the launch carries no place,
    /// the distro was never ensured, the profile's network mode is one a WSL
    /// place cannot enforce, or a writable path encloses the bubblewrap that
    /// applies the boundary — including one this launch minted, which the
    /// profile never named and `ensure_distro` therefore never saw.
    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv> {
        if launch.policy.backend != SandboxBackendKind::WslDistro {
            return Err(SandboxError::Unsupported {
                backend: SandboxBackendKind::WslDistro,
                detail: format!(
                    "policy was resolved for '{}'; resolve it for wsl-distro first",
                    launch.policy.backend
                ),
            });
        }
        let refuse = |detail: String| SandboxError::Refused {
            profile: launch.policy.profile.clone(),
            detail,
        };
        // Asked of the policy rather than of the launch, and before the launch's
        // own validation: the answer is the same whether or not a proxy was
        // bound, and this is the sentence that says why — the launch's is about
        // a proxy being missing, which here it always will be.
        if proxy_required(launch.policy) {
            return Err(refuse(format!(
                "network mode '{}' is enforced by friring's egress proxy, and {UNFILTERED}",
                launch.policy.network
            )));
        }
        launch.validate()?;
        // A launch with no place never called `ensure`, and returning the argv
        // unchanged would run the agent on the *host* under a profile that says
        // it is in a distro.
        let place = launch.place.ok_or_else(|| {
            refuse(
                "this profile runs in a WSL distro, whose command is composed for the inside of \
                 the distro; this launch was built without one"
                    .to_string(),
            )
        })?;
        if place.relay.is_some() {
            return Err(refuse(format!(
                "this launch carries an egress relay, and a WSL place has nowhere to reach one \
                 from: {UNFILTERED}"
            )));
        }

        let distro = plan::distro_name(&launch.policy.profile);
        let bwrap_program = self
            .bwrap_programs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&distro)
            .cloned()
            .ok_or_else(|| {
                refuse(format!(
                    "friring has not ensured the distro '{distro}' in this run, so it does not \
                     know which bubblewrap applies this profile inside it — and a WSL place \
                     without one is a distro the agent sees all of"
                ))
            })?;
        // Against the *launch's* writable set rather than the profile's, and
        // before anything is asked of the distro: the minted directories a
        // launch adds are writable too, and this is the last gate before the
        // argv is composed — `bwrap::build_argv` is reached directly here, so
        // `BwrapBackend::wrap`'s own copy of this check never runs.
        check_bwrap_containment(&launch.writable_paths(), &distro, &bwrap_program, &refuse)?;
        self.check_present(&distro, launch, &refuse)?;

        // Every path in the plan is a path in the *distro's* filesystem, so the
        // question "is this there?" is asked in there rather than of Windows.
        let mut out = bwrap::build_argv(&bwrap_program, launch, None, &|path| {
            self.inside_exists(&distro, path)
        })?;
        out.extend(argv);
        Ok(out)
    }

    fn ensure(&self, profile: &SandboxProfile) -> SandboxResult<SandboxInstance> {
        Ok(self.ensure_distro(profile)?.instance)
    }
}

/// Why a filtered network mode is refused, said once so the two refusals that
/// need it cannot drift.
const UNFILTERED: &str = "a WSL place's egress relay is not wired: the proxy enforces the rules \
                          from outside the boundary over a unix socket bind-mounted across it, \
                          and friring is on the Windows side of the utility VM the distro lives \
                          in. Use network 'none' or 'full', or run friring inside a distro and \
                          pick the bwrap backend, whose sandbox is on the same filesystem as its \
                          proxy";

/// Refuse a set of read-write paths that encloses the bubblewrap applying this
/// place's boundary.
///
/// The rule the other three place backends apply to their engine's CLI
/// ([`dirs::program_in_writable_root`]) and bubblewrap applies to itself, said
/// once here for both of the seams a WSL place has:
/// [`ensure_distro`](WslDistroBackend::ensure_distro), where the program is
/// resolved and pinned, and [`wrap`](SandboxBackend::wrap), which composes the
/// argv from the pinned one and reaches [`bwrap::build_argv`] without passing
/// [`bwrap::BwrapBackend::wrap`]'s own copy of the check.
///
/// Both, rather than either: the profile is what the first sees, and the launch
/// adds writable directories of its own that the profile never named — so a
/// check in one place would answer about the wrong set.
///
/// The program is compared **as the distro spelled it**, with no resolved
/// second spelling. `command -v` inside the distro answers with a `PATH`
/// lookup, and asking the distro to resolve it further is a command friring
/// cannot add without starting the place again; the container backends leave
/// the same literal comparison standing for a remote engine, for the same
/// reason. What is left is a symlink *inside* the distro pointing from a system
/// prefix into a writable one, which the distro's own owner planted.
fn check_bwrap_containment(
    rw_paths: &[String],
    distro: &str,
    program: &str,
    refuse: &dyn Fn(String) -> SandboxError,
) -> SandboxResult<()> {
    let Some((root, _)) = dirs::program_in_writable_root(rw_paths, program, None) else {
        return Ok(());
    };
    Err(refuse(format!(
        "the read-write path '{root}' contains bubblewrap itself ('{program}' in the distro \
         '{distro}'), so the sandbox could replace the program that applies its own boundary. A \
         WSL distro is one filesystem and one identity — bubblewrap is the whole of the boundary \
         in there, so it is refused rather than granted"
    )))
}

/// Refuse a profile asking for something a WSL place cannot enforce.
///
/// Every one of these has a silent alternative that grants more than the profile
/// says: a limit friring accepts and never applies, a domain allowlist nothing
/// enforces, an image nothing builds. `docs/SANDBOX.md` §Failure modes requires
/// an unavailable capability to be shown as unavailable rather than accepted and
/// ignored, and a stored profile can carry one whatever the editor greys out —
/// imported, hand-edited, or written before the capability was known.
fn check_enforceable(
    profile: &SandboxProfile,
    refuse: &dyn Fn(String) -> SandboxError,
) -> SandboxResult<()> {
    if profile.memory_mb.is_some() || profile.cpus.is_some() {
        return Err(refuse(
            "memory and CPU caps in WSL are global to the utility VM every distro shares, and are \
             set once in %USERPROFILE%\\.wslconfig — friring will not accept a per-sandbox limit \
             it cannot apply. Clear the limits, or use a container backend"
                .to_string(),
        ));
    }
    if profile.containerfile.is_some() {
        return Err(refuse(
            "a WSL place is cloned from a distro registered on this host, not built from a \
             containerfile. Name the template distro as the profile's image, or use a container \
             backend"
                .to_string(),
        ));
    }
    let filtered = profile.network_mode == NetworkMode::Allowlist
        || (profile.network_mode == NetworkMode::Full && !profile.network_deny.is_empty());
    if filtered {
        return Err(refuse(format!(
            "network mode '{}' is enforced by friring's egress proxy, and {UNFILTERED}",
            profile.network_mode
        )));
    }
    Ok(())
}

/// Turn a WSL command's outcome into friring's own sentence, with WSL's first
/// line of stderr as the detail.
///
/// `wsl.exe` writes its errors as UTF-16, so the decode is what makes them
/// legible at all rather than a diagnostic nicety.
fn outcome(
    result: Result<ProbeOutput, String>,
    refuse: &dyn Fn(String) -> SandboxError,
    what: impl Fn() -> String,
) -> SandboxResult<()> {
    match result {
        Ok(output) if output.ok() => Ok(()),
        Ok(output) => Err(refuse(format!(
            "{}: {}",
            what(),
            first_line(&output.stderr, "WSL gave no reason")
        ))),
        Err(detail) => Err(refuse(format!("{}: {detail}", what()))),
    }
}

/// The first non-empty line of what `wsl.exe` wrote, decoded.
fn first_line(stderr: &str, fallback: &str) -> String {
    plan::decode(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

/// The distros a profile could name as its template, for a refusal that says
/// what to do next.
fn available_names(listed: &[DistroInfo]) -> String {
    let names: Vec<&str> = listed
        .iter()
        .filter(|distro| !plan::looks_like_ours(&distro.name))
        .map(|distro| distro.name.as_str())
        .collect();
    if names.is_empty() {
        return "none — install one with 'wsl --install -d Ubuntu'".to_string();
    }
    names.join(", ")
}

/// A path the backend can name exactly, or a refusal — a lossy conversion would
/// export to, or import from, a different file.
fn representable(path: &Path) -> SandboxResult<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| SandboxError::Io {
            path: path.display().to_string(),
            detail: "is not valid UTF-8, and a distro registered from an approximation of it \
                     would name a different file"
                .to_string(),
        })
}

#[cfg(test)]
mod tests;
