//! The `host-minus-secrets` deny list.
//!
//! [`ReadScope::HostMinusSecrets`](crate::session::ReadScope::HostMinusSecrets)
//! is the default read policy for policy backends because host passthrough is
//! the default credential story: the agent reads its own configuration from the
//! real home, the macOS Keychain keeps working, and nothing is copied
//! (ADR-28). The price is that everything *else* in the home is readable too,
//! so the scope is only honest if the credentials that are not the agent's own
//! are taken back — which is what this list does.
//!
//! Two consequences worth stating up front:
//!
//! - Denying `~/.ssh` means **git over SSH does not work inside a sandbox**.
//!   That is the point: a key the agent can read is a key it can use anywhere.
//!   Use an HTTPS remote with a scoped token, or a profile that lists the key
//!   explicitly and accepts what that means.
//! - The list is per *credential family*, not per agent binary. Launching
//!   claude keeps claude's own credential file readable and denies codex's, so
//!   one sandboxed agent cannot read another's token.

use crate::sandbox::probe::HostPlatform;

/// Whether an entry names a directory (hidden wholesale) or a single file.
///
/// A bind-mount backend needs the distinction: a directory is covered by an
/// empty tmpfs, a file by a bind of `/dev/null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Dir,
    File,
}

/// Which hosts an entry applies to. Most credentials live at the same path
/// everywhere; the keychain database does not exist off macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretPlatform {
    Any,
    MacOs,
    Linux,
}

impl SecretPlatform {
    /// The entry set a host of this platform gets: its own, plus the universal
    /// ones.
    pub fn of(host: HostPlatform) -> Self {
        match host {
            HostPlatform::MacOs { .. } => Self::MacOs,
            HostPlatform::Linux | HostPlatform::WslDistro => Self::Linux,
            HostPlatform::Windows | HostPlatform::Unknown => Self::Any,
        }
    }

    fn covers(self, entry: Self) -> bool {
        entry == Self::Any || entry == self
    }
}

/// One denied path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretPath {
    /// Relative to the home directory. Never absolute: the home a sandbox runs
    /// under is not necessarily this machine's.
    pub path: &'static str,
    pub kind: SecretKind,
    pub platform: SecretPlatform,
    /// The agent whose *own* credential this is, matched against the launching
    /// agent's family so passthrough keeps working for it and only for it.
    pub owner: Option<&'static str>,
    /// What is lost if the agent reads this. Rendered into the generated
    /// profile, so the reason travels with the rule.
    pub why: &'static str,
}

const fn dir(path: &'static str, why: &'static str) -> SecretPath {
    SecretPath {
        path,
        kind: SecretKind::Dir,
        platform: SecretPlatform::Any,
        owner: None,
        why,
    }
}

const fn file(path: &'static str, why: &'static str) -> SecretPath {
    SecretPath {
        path,
        kind: SecretKind::File,
        platform: SecretPlatform::Any,
        owner: None,
        why,
    }
}

/// Everything a `host-minus-secrets` sandbox is denied, in the order it is
/// written into a generated profile.
///
/// Adding an entry is cheap and denying a path that does not exist costs
/// nothing, so the bar for inclusion is "reading it grants credentials or
/// executes code elsewhere", not "the author happens to use it".
pub const SECRET_PATHS: &[SecretPath] = &[
    dir(
        ".ssh",
        "private keys and known_hosts: an agent that reads them authenticates as the user \
         on every machine those keys reach",
    ),
    dir(
        ".gnupg",
        "the GPG secret keyring: commit signatures, and anything the user ever encrypted",
    ),
    dir(
        ".aws",
        "long-lived AWS access keys and cached SSO session tokens",
    ),
    dir(
        ".config/gcloud",
        "Google Cloud refresh tokens and application-default credentials",
    ),
    dir(".azure", "Azure CLI access and refresh tokens"),
    dir(
        ".kube",
        "kubeconfig embeds cluster tokens and client certificates",
    ),
    dir(
        ".docker",
        "registry credentials, or the credential helper that hands them out",
    ),
    dir(
        ".config/gh",
        "the GitHub CLI's OAuth token, which carries the user's full repository scope",
    ),
    file(
        ".netrc",
        "plaintext logins that curl and git use without being asked",
    ),
    file(".npmrc", "the npm publish token"),
    file(".pypirc", "the PyPI upload token"),
    file(".cargo/credentials.toml", "the crates.io publish token"),
    file(
        ".cargo/credentials",
        "the crates.io publish token, in cargo's pre-1.68 spelling",
    ),
    file(
        ".git-credentials",
        "plaintext credentials for every git remote the user has pushed to",
    ),
    SecretPath {
        path: "Library/Keychains",
        kind: SecretKind::Dir,
        platform: SecretPlatform::MacOs,
        owner: None,
        why: "the login keychain database. Reading the file bypasses securityd's per-item \
              prompts; the agent keeps Keychain *access* through the mach lookups this \
              profile allows, and loses the ability to copy the whole store",
    },
    SecretPath {
        path: ".claude/.credentials.json",
        kind: SecretKind::File,
        platform: SecretPlatform::Any,
        owner: Some("claude"),
        why: "another agent's OAuth credentials",
    },
    SecretPath {
        path: ".codex/auth.json",
        kind: SecretKind::File,
        platform: SecretPlatform::Any,
        owner: Some("codex"),
        why: "another agent's OAuth credentials",
    },
];

impl SecretPath {
    /// This entry's absolute path under `home`.
    pub fn resolved(&self, home: &str) -> String {
        format!("{}/{}", home.trim_end_matches(['/', '\\']), self.path)
    }
}

/// The entries that apply to `platform`, with the launching agent's own
/// credential file left out.
///
/// `agent` is a credential *family* — an agent's registry name, or its
/// `hook_schema` when it is a rebrand of a built-in — so a custom agent that
/// declares itself a claude keeps claude's credentials.
pub fn secrets_for(platform: SecretPlatform, agent: Option<&str>) -> Vec<&'static SecretPath> {
    SECRET_PATHS
        .iter()
        .filter(|s| platform.covers(s.platform))
        .filter(|s| match (s.owner, agent) {
            (Some(owner), Some(agent)) => owner != agent,
            _ => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_is_home_relative_and_explains_itself() {
        for secret in SECRET_PATHS {
            assert!(
                !secret.path.starts_with('/') && !secret.path.starts_with('~'),
                "{} must be relative to the home directory",
                secret.path
            );
            assert!(
                secret.why.len() > 20,
                "{} needs a reason worth reading",
                secret.path
            );
        }
    }

    #[test]
    fn the_launching_agent_keeps_its_own_credentials() {
        let claude = secrets_for(SecretPlatform::Linux, Some("claude"));
        assert!(claude.iter().all(|s| s.path != ".claude/.credentials.json"));
        // …and still cannot read the other agent's.
        assert!(claude.iter().any(|s| s.path == ".codex/auth.json"));

        // With no agent named, nothing is exempted.
        let anonymous = secrets_for(SecretPlatform::Linux, None);
        assert!(anonymous
            .iter()
            .any(|s| s.path == ".claude/.credentials.json"));
    }

    #[test]
    fn the_keychain_database_is_denied_on_macos_only() {
        let mac = secrets_for(SecretPlatform::MacOs, None);
        assert!(mac.iter().any(|s| s.path == "Library/Keychains"));
        let linux = secrets_for(SecretPlatform::Linux, None);
        assert!(linux.iter().all(|s| s.path != "Library/Keychains"));
        // The universal entries are on both.
        for set in [&mac, &linux] {
            assert!(set.iter().any(|s| s.path == ".ssh"));
        }
    }

    #[test]
    fn platform_mapping_follows_the_host() {
        assert_eq!(
            SecretPlatform::of(HostPlatform::MacOs {
                apple_silicon: true,
                major: 26
            }),
            SecretPlatform::MacOs
        );
        // A WSL distro is Linux for this purpose — same paths, same tools.
        assert_eq!(
            SecretPlatform::of(HostPlatform::WslDistro),
            SecretPlatform::Linux
        );
        assert_eq!(
            SecretPlatform::of(HostPlatform::Windows),
            SecretPlatform::Any
        );
    }

    #[test]
    fn resolution_joins_onto_the_target_home() {
        let ssh = SECRET_PATHS.iter().find(|s| s.path == ".ssh").unwrap();
        assert_eq!(ssh.resolved("/home/u"), "/home/u/.ssh");
        // A trailing separator on the home must not double up.
        assert_eq!(ssh.resolved("/home/u/"), "/home/u/.ssh");
        assert_eq!(ssh.kind, SecretKind::Dir);
    }
}
