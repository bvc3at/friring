//! The operations a **place** has and a policy backend does not.
//!
//! [`SandboxBackend`] is the seam every isolation technology plugs into, and it
//! is deliberately small: probe, declare capabilities, and then wrap an argv or
//! ensure an environment. A place needs
//! more than that — it has a *lifecycle*, and the launch path, the teardown
//! path, the reclaiming pass and the headless CLI all drive it. [`PlaceBackend`]
//! is that second seam.
//!
//! It exists so those callers name **one** interface rather than one accessor
//! per tool. Two place backends already share their mount plan, labels, spec
//! digest and collection decision as code (`crate::sandbox::container::plan` and
//! `::gc`); this shares the *callers*, which is the half where a third backend
//! that quietly skipped a step would be a new escape rather than a new feature.
//!
//! Not every place implements it. A [`wsl-distro`](crate::sandbox::wsl) place is
//! a registered distro reached by the `wsl:` transport friring already has, and
//! it hands back a distro name rather than an [`EnsuredPlace`]; it is driven
//! through [`SandboxHost::wsl_distro`](crate::sandbox::SandboxHost::wsl_distro)
//! instead, and the launch path refuses it with what is missing rather than
//! composing half of one.
//!
//! Being outside this trait is a reason to name it explicitly everywhere the
//! trait is what a caller walks, and **not** a reason to hold it to less: the
//! shared conformance table (`sandbox::tests::place_conformance`) drives a WSL
//! place through its own two seams — `ensure_distro`, then
//! [`SandboxBackend::wrap`] — and asserts the same refusals as the three that
//! ensure a container.

use crate::sandbox::backend::{SandboxBackend, SandboxError, SandboxResult};
use crate::sandbox::container::{EnsuredPlace, GcPlan, LiveContainer};
use crate::session::{SandboxBackendKind, SandboxPolicy, SandboxProfile};

/// The place backends friring creates, reaches and reclaims a container in.
///
/// One list, because every caller that walks the engines has to walk the same
/// ones: a pass that asked two of three would read the third's containers as
/// "no longer there" and forget the only ids anything has for them.
pub const PLACE_KINDS: &[SandboxBackendKind] = &[
    SandboxBackendKind::Docker,
    SandboxBackendKind::Podman,
    SandboxBackendKind::AppleContainer,
];

/// A backend whose sandbox is an environment that outlives one command.
///
/// Implemented by the container engines and by Apple's `container`, which differ
/// in their command lines and in nothing a caller here cares about. Every method
/// is fallible in the backend's own words, because "the engine is not installed"
/// and "the engine refused" are different answers and the callers act on the
/// difference (see [`live_places_here`]).
///
/// A supertrait rather than a second root: a place backend *is* a
/// [`SandboxBackend`], so one `&dyn PlaceBackend` answers `kind`, `probe`,
/// `capabilities` and `wrap` as well, and neither trait needs a `kind` of its
/// own to disagree about.
pub trait PlaceBackend: SandboxBackend {
    /// Make sure `profile`'s place exists and is running, and answer with
    /// everything a launch into it needs.
    ///
    /// Idempotent, so a relaunch adopts the place it already had.
    ///
    /// # Errors
    ///
    /// The backend is unavailable, the profile cannot be resolved or honoured,
    /// or the place will not start — each with an actionable sentence.
    fn ensure_place(&self, profile: &SandboxProfile) -> SandboxResult<EnsuredPlace>;

    /// Refuse this launch unless the place carries the agent it is about to run.
    ///
    /// Per launch rather than per place: a place is shared by every session of
    /// its profile and those sessions need not run the same agent.
    ///
    /// # Errors
    ///
    /// The place has no such program, or the backend would not answer — which
    /// are different fixes and are reported as such.
    fn ensure_agent_program(
        &self,
        policy: &SandboxPolicy,
        place: &EnsuredPlace,
        program: &str,
    ) -> SandboxResult<()>;

    /// The digest a place built from `profile` right now would carry, or `None`
    /// when this backend has no opinion — which the reclaiming pass reads as
    /// "leave its containers alone" rather than as "the profile is gone".
    fn current_spec(&self, profile: &SandboxProfile) -> Option<String>;

    /// The absolute engine binary the probe resolved and vetted, which the
    /// sandbox transport is built from.
    ///
    /// # Errors
    ///
    /// The backend is unavailable; the message is the probe's own.
    fn engine_program(&self) -> SandboxResult<&str>;

    /// Every place friring created under this backend, as garbage collection
    /// sees them.
    ///
    /// # Errors
    ///
    /// The backend is unavailable, or would not list what it holds.
    fn live_places(&self) -> SandboxResult<Vec<LiveContainer>>;

    /// Carry out a [`GcPlan`]'s removals, answering with the ones that failed.
    ///
    /// Best effort: a place that will not go is one the next pass tries again.
    /// Every implementation re-checks friring's own owner label immediately
    /// before it destroys anything.
    fn reap(&self, plan: &GcPlan) -> Vec<String>;

    /// The loopback port this session's egress relay listens on inside
    /// `container`.
    ///
    /// Defaulted to a refusal, which is right for a backend that cannot carry a
    /// filtered network mode at all: it refuses those profiles when the place is
    /// ensured, so nothing ever composes a relay for one, and a caller that
    /// somehow did would be told rather than handed a port to nowhere.
    ///
    /// # Errors
    ///
    /// This backend has no relay to give, or the place has run out of ports.
    fn relay_port(&self, container: &str, session_key: &str) -> SandboxResult<u16> {
        let _ = (container, session_key);
        Err(SandboxError::Unsupported {
            backend: self.kind(),
            detail: "this backend enforces no filtered network mode, so its places compose no \
                     egress relay"
                .to_string(),
        })
    }

    /// Forget every relay port a session held, whichever place it held them in.
    ///
    /// Teardown's half, and a no-op for a backend that hands none out.
    fn release_relay_ports(&self, session_key: &str) {
        let _ = session_key;
    }
}

/// What this backend holds here, or `None` when it is not installed at all.
///
/// The distinction every caller that walks [`PLACE_KINDS`] has to make, in one
/// place so they cannot make it differently. An engine friring **cannot drive**
/// is holding nothing, because it created nothing — so a machine with one engine
/// installed must not have its reclaiming pass permanently stalled by the two it
/// does not have. An engine that *is* installed and would not answer is the
/// opposite: it may be holding anything, and reading that as "nothing" would
/// forget the only ids that can find those containers again.
///
/// Asked of the **probe** rather than of the error a listing came back with:
/// "not installed" and "installed and would not answer" are both
/// [`SandboxError::Unavailable`] by the time a backend has tried to run
/// something, and only the probe knows which of the two it is.
///
/// # Errors
///
/// The backend is installed and would not say what it holds.
pub fn live_places_here(backend: &dyn PlaceBackend) -> SandboxResult<Option<Vec<LiveContainer>>> {
    if !backend.probe().is_available() {
        return Ok(None);
    }
    backend.live_places().map(Some)
}

/// Whether a string is a container name or id a place backend could have
/// minted: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, at most [`MAX_CONTAINER_REF`] bytes
/// (an engine id is 64 hex).
///
/// Everything friring learns about a place passes through here before it is
/// recorded or used, because it reaches a command line as the argument right
/// after `exec`'s own flags — a value that could pass for one (`-i`, `--rm`)
/// must never get that far. One rule with two ends: every backend refuses to
/// hand one over, and [`crate::agent::transport::Place`] refuses to build a
/// command line from one it is handed. Both call this, so neither can drift into
/// accepting what the other rejects.
#[must_use]
pub fn valid_container_ref(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_CONTAINER_REF
        && raw.starts_with(|c: char| c.is_ascii_alphanumeric())
        && raw.chars().all(is_container_ref_char)
}

/// Longest container reference friring will build a command from. Engine ids
/// are 64 hex characters and container names are short; anything longer is not
/// one.
pub const MAX_CONTAINER_REF: usize = 128;

/// Characters a container reference may carry after the first: the container
/// engines' own name grammar, which the ids they mint also satisfy.
fn is_container_ref_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_reference_is_a_name_or_an_id_and_never_a_flag() {
        assert!(valid_container_ref("friring-dev-1a2b3c"));
        assert!(valid_container_ref(&"a".repeat(MAX_CONTAINER_REF)));
        // The whole point: a value that could pass for an option to the `exec`
        // it lands beside.
        assert!(!valid_container_ref("-i"));
        assert!(!valid_container_ref("--rm"));
        assert!(!valid_container_ref(""));
        assert!(!valid_container_ref("has space"));
        assert!(!valid_container_ref("a;rm -rf /"));
        assert!(!valid_container_ref(&"a".repeat(MAX_CONTAINER_REF + 1)));
    }
}
