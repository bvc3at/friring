//! Which image a place runs, and how it comes to exist.
//!
//! A place needs an image carrying the agent, the toolchain and **tmux** — the
//! transport runs tmux *inside* (ADR-26). friring does not publish one: the
//! default image is defined declaratively by `packaging/sandbox/Containerfile`
//! in this repository and built under a documented tag, so nothing in the code
//! depends on a registry friring would have to keep, and nobody's agent ends up
//! running whatever a name resolved to today.
//!
//! The consequence is deliberate: a missing default image is a **refusal with
//! the build command**, not a silent pull. A tag the *user* named in their
//! profile is theirs, and is pulled if the engine does not have it.

use crate::sandbox::backend::{SandboxError, SandboxResult};
use crate::sandbox::dirs;
use crate::session::SandboxPolicy;

/// The image a profile gets when it names none.
///
/// Registry-less on purpose — a bare `name:tag` an engine can only satisfy
/// locally. Built from `packaging/sandbox/Containerfile`; the tag's number moves
/// when the file's contract changes (the agent runtimes it carries, the user it
/// declares), so an old image is never silently reused under a new contract.
pub const DEFAULT_IMAGE: &str = "friring/sandbox:1";

/// How to build the default image, quoted verbatim in the refusal that needs it.
pub const DEFAULT_IMAGE_BUILD: &str =
    "build it once: docker build -t friring/sandbox:1 - < packaging/sandbox/Containerfile \
     (podman build … for podman)";

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SandboxBackendKind, SandboxPath, SandboxProfile};

    fn resolved(mutate: impl FnOnce(&mut SandboxProfile)) -> SandboxPolicy {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        mutate(&mut profile);
        profile
            .resolve(SandboxBackendKind::Podman, "/home/u")
            .unwrap()
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
}
