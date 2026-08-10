//! Where friring keeps the tokens it injects into a place — and the seam that
//! keeps every test away from a real one.
//!
//! `env-token` (`docs/SANDBOX.md` §Credentials) is a long-lived token the user
//! supplies once. It lives in **friring's own OS keychain entry** and nowhere
//! else: not in the database, not in `agents.toml`, not in a profile, not on
//! disk. The registry declares the variable *names* an agent accepts
//! ([`AgentSandboxDef::secret_env`](crate::session::AgentSandboxDef::secret_env));
//! the values only ever exist here and in the environment of the window the
//! agent runs in.
//!
//! ## Why this is its own seam
//!
//! Two constraints decide the shape:
//!
//! - **A secret must never reach argv.** Anything on a command line is on the
//!   host's process table (`/proc/<pid>/cmdline` is world-readable on Linux by
//!   default), which is the same rule that keeps the egress proxy's token out
//!   of the tmux command line. So a value is handed to a tool on **stdin**, and
//!   a platform whose tool takes it no other way is refused with the command
//!   the *user* can run instead.
//! - **No test may touch a real keychain.** [`SecretStore`] is a trait, so a
//!   test answers with `StubStore` and nothing on the machine is consulted —
//!   not even to "check the format".
//!
//! [`crate::sandbox::probe::ProbeHost`] is the seam every other host question
//! goes through, and it is deliberately *not* this one: it has no stdin, so a
//! write through it would have to put the value in argv.
//!
//! ## What each platform's tool does
//!
//! | Host | Read | Write |
//! |---|---|---|
//! | macOS | `security find-generic-password -w` prints the value on stdout | `-w <value>` is the only documented form, so friring will not run it |
//! | Linux | `secret-tool lookup` prints the value on stdout | `secret-tool store` reads the value on stdin, by design |
//!
//! macOS's asymmetry is stated rather than papered over: `security` has no
//! documented stdin mode for a new item, so friring hands back the exact
//! command to run — `-w` with no value makes `security` prompt, which keeps the
//! token out of argv *and* out of shell history.

use std::fmt;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::sandbox::dirs;
use crate::sandbox::probe::{LocalProbeHost, ProbeHost};

/// A credential value, in a type that will not print itself.
///
/// `Debug` is hand-written and there is deliberately **no `Display`**: a type
/// that can be `{}`-formatted ends up in a log line, a toast or an `expect`
/// message eventually, and the compiler refusing `format!("{secret}")` is a
/// better guarantee than a convention. The one way out is
/// [`expose`](Self::expose), which is greppable.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself.
    ///
    /// Every caller of this is a place where a credential leaves friring's
    /// custody, so there are meant to be very few: the launch path routes it
    /// into the tmux window over the control connection, and nothing else may.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the store answered with nothing but whitespace, which is a
    /// cleared entry rather than a token.
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    /// Withholds the value. A derived `Debug` would put a vendor token in every
    /// `{:?}`, every panic message and every `expect` that mentions one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(withheld)")
    }
}

/// Which entry one secret lives under.
///
/// Keyed on the agent's **credential family** and the variable name, not on the
/// profile: the token is a property of the account the user holds, so one entry
/// serves every profile that agent runs in — which is what "supplies it once"
/// means. A profile-scoped override would be a second lookup here, not a
/// different shape.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretKey {
    family: String,
    variable: String,
}

/// The keychain service every friring entry is filed under, so a user can see
/// (and revoke) exactly what friring holds.
pub const SERVICE: &str = "dev.friring.sandbox";

impl SecretKey {
    /// A key for `family`'s `variable`, or `None` when either could not be
    /// spelled as one.
    ///
    /// Both halves reach a keychain tool's command line as the value of a flag,
    /// and both come from `agents.toml` — so they are held to a charset that
    /// cannot be read as a flag, a path or a shell fragment. A variable name is
    /// held to what a POSIX environment variable may be called, which is also
    /// what makes it safe to hand to `tmux setenv` later.
    pub fn new(family: &str, variable: &str) -> Option<Self> {
        let family_ok = !family.is_empty()
            && family
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !family.starts_with('-')
            && !family.starts_with('.');
        if !family_ok || !is_env_name(variable) {
            return None;
        }
        Some(Self {
            family: family.to_string(),
            variable: variable.to_string(),
        })
    }

    /// The variable this secret is injected as.
    pub fn variable(&self) -> &str {
        &self.variable
    }

    /// The credential family the entry belongs to.
    pub fn family(&self) -> &str {
        &self.family
    }

    /// The account name inside [`SERVICE`]: `<family>/<variable>`.
    pub fn account(&self) -> String {
        format!("{}/{}", self.family, self.variable)
    }
}

/// Whether `name` is a POSIX environment-variable name.
///
/// Shared with [`super`], which holds every variable it takes from a
/// declaration to the same rule before it reaches a `tmux setenv`.
pub(super) fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// friring's own credential store.
///
/// Small on purpose: read one entry, write one entry, forget one entry, and say
/// how a human would do the same. Everything that decides *whether* to consult
/// it lives in [`super`].
pub trait SecretStore: Send + Sync {
    /// What this store is, in the words a message uses ("the macOS keychain").
    fn label(&self) -> &str;

    /// The token under `key`, `None` when the entry does not exist.
    ///
    /// A store that is *there* but could not answer — a locked keychain, no
    /// secret service running — is an `Err`, not a `None`: the difference
    /// decides whether the user is told to sign in or told to unlock something.
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, String>;

    /// Store `secret` under `key`, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// The store is unavailable, the tool failed, or the platform has no way to
    /// take the value without putting it on a command line — in which case the
    /// message is [`how_to_store`](Self::how_to_store)'s command.
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), String>;

    /// Remove `key`'s entry. Removing one that is not there is not an error.
    fn remove(&self, key: &SecretKey) -> Result<(), String>;

    /// The command a user runs to put a token in this store themselves.
    ///
    /// Carries a *placeholder*, never a value: this string is shown in a modal
    /// and written to a log.
    fn how_to_store(&self, key: &SecretKey) -> String;
}

/// Which platform tool a [`SystemKeychain`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeychainKind {
    /// macOS `security`, against the login keychain.
    MacSecurity,
    /// `secret-tool`, against whatever Secret Service is running (GNOME
    /// Keyring, KWallet).
    Libsecret,
}

impl KeychainKind {
    fn label(self) -> &'static str {
        match self {
            Self::MacSecurity => "the macOS keychain",
            Self::Libsecret => "the system keyring",
        }
    }
}

/// The real thing: one of the platform credential tools, resolved to an
/// absolute path.
///
/// Absolute for the reason every other program this module runs is (see
/// [`crate::sandbox::dirs::rewritable_root`]): a copy of `security` or
/// `secret-tool` planted somewhere a sandboxed agent can write would be handed
/// the user's token on the next launch. A tool that resolves into such a place
/// is refused, and the store degrades to [`Unavailable`].
pub struct SystemKeychain {
    kind: KeychainKind,
    program: String,
}

impl SystemKeychain {
    /// A store driving `program` (an absolute path) as `kind`.
    pub fn new(kind: KeychainKind, program: impl Into<String>) -> Self {
        Self {
            kind,
            program: program.into(),
        }
    }

    /// Run the tool with no stdin, capturing both streams so nothing reaches
    /// the terminal the TUI is drawing on.
    fn run(&self, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("{} could not be run: {e}", self.program))
    }

    /// Run the tool with `secret` on **stdin** — the only channel that keeps a
    /// value off the process table.
    fn run_with_secret(&self, args: &[&str], secret: &Secret) -> Result<(), String> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{} could not be run: {e}", self.program))?;
        if let Some(mut stdin) = child.stdin.take() {
            // The value, and nothing after it: `secret-tool` stores stdin
            // verbatim up to EOF, so a trailing newline would become part of
            // the token.
            stdin
                .write_all(secret.expose().as_bytes())
                .map_err(|e| format!("{} would not take the value: {e}", self.program))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("{} did not finish: {e}", self.program))?;
        if output.status.success() {
            return Ok(());
        }
        Err(first_line(
            &String::from_utf8_lossy(&output.stderr),
            "the credential store gave no reason",
        ))
    }
}

impl SecretStore for SystemKeychain {
    fn label(&self) -> &str {
        self.kind.label()
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, String> {
        let account = key.account();
        let output = match self.kind {
            KeychainKind::MacSecurity => {
                self.run(&["find-generic-password", "-s", SERVICE, "-a", &account, "-w"])?
            }
            KeychainKind::Libsecret => {
                self.run(&["lookup", "service", SERVICE, "account", &account])?
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            // A tool that says nothing is saying "no such item" — both of these
            // exit non-zero for a missing entry. Anything it *did* say is a
            // condition the user can act on (a locked keychain, no secret
            // service), and reporting it is the difference between "sign in"
            // and "unlock your keychain".
            return match missing_entry(&stderr) {
                true => Ok(None),
                false => Err(first_line(&stderr, "the credential store would not answer")),
            };
        }
        // Both tools terminate the value with a newline on a tty, and
        // `security` always does; a token never ends in whitespace, so trimming
        // the line ending is safe and not trimming it breaks every request.
        let value = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        Ok(Some(Secret::new(value)).filter(|s| !s.is_empty()))
    }

    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), String> {
        let account = key.account();
        match self.kind {
            // `security` takes a new item's value only as `-w <value>`, which
            // would put a vendor token on the host's process table. friring
            // will not do that; the user's own run of the same command with
            // `-w` and no value prompts for it instead.
            KeychainKind::MacSecurity => Err(format!(
                "friring will not write to {} itself, because `security` takes a new item's value \
                 only on its command line, where every process on this machine can read it. Run \
                 this instead — it prompts for the value: {}",
                self.label(),
                self.how_to_store(key)
            )),
            KeychainKind::Libsecret => self.run_with_secret(
                &[
                    "store",
                    "--label",
                    "friring sandbox token",
                    "service",
                    SERVICE,
                    "account",
                    &account,
                ],
                secret,
            ),
        }
    }

    fn remove(&self, key: &SecretKey) -> Result<(), String> {
        let account = key.account();
        let output = match self.kind {
            KeychainKind::MacSecurity => {
                self.run(&["delete-generic-password", "-s", SERVICE, "-a", &account])?
            }
            KeychainKind::Libsecret => {
                self.run(&["clear", "service", SERVICE, "account", &account])?
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() || missing_entry(&stderr) {
            return Ok(());
        }
        Err(first_line(
            &stderr,
            "the credential store would not remove the entry",
        ))
    }

    fn how_to_store(&self, key: &SecretKey) -> String {
        let account = key.account();
        match self.kind {
            // `-w` with no value: `security` prompts, so the token reaches
            // neither the process table nor the shell history.
            KeychainKind::MacSecurity => format!(
                "security add-generic-password -U -s {SERVICE} -a {account} -w   (it will prompt \
                 for the value)"
            ),
            KeychainKind::Libsecret => format!(
                "secret-tool store --label 'friring sandbox token' service {SERVICE} account \
                 {account}   (it will prompt for the value)"
            ),
        }
    }
}

/// No credential store on this host, with the reason.
///
/// Never a silent absence: a launch that finds this reports "there is nowhere
/// to keep a token here, so sign in inside the pane" rather than behaving as if
/// the entry were merely empty.
pub struct Unavailable {
    reason: String,
    fix: String,
}

impl Unavailable {
    pub fn new(reason: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            fix: fix.into(),
        }
    }
}

impl SecretStore for Unavailable {
    fn label(&self) -> &str {
        &self.reason
    }

    fn get(&self, _key: &SecretKey) -> Result<Option<Secret>, String> {
        Ok(None)
    }

    fn set(&self, _key: &SecretKey, _secret: &Secret) -> Result<(), String> {
        Err(format!("{}: {}", self.reason, self.fix))
    }

    fn remove(&self, _key: &SecretKey) -> Result<(), String> {
        Ok(())
    }

    fn how_to_store(&self, _key: &SecretKey) -> String {
        self.fix.clone()
    }
}

/// The store this host has, probed once.
///
/// Cached because the launch path asks on every spawn and the answer is a
/// `PATH` scan. Nothing here opens a keychain — finding the tool is not reading
/// from it.
pub fn system_store() -> &'static dyn SecretStore {
    #[cfg(test)]
    if let Some(installed) = TEST_STORE.with(std::cell::Cell::get) {
        return installed;
    }
    static STORE: OnceLock<Box<dyn SecretStore>> = OnceLock::new();
    STORE.get_or_init(|| detect(&LocalProbeHost)).as_ref()
}

#[cfg(test)]
thread_local! {
    /// See [`TestSecretStore`].
    static TEST_STORE: std::cell::Cell<Option<&'static dyn SecretStore>> =
        const { std::cell::Cell::new(None) };
}

/// Answer [`system_store`] with a fabricated store for as long as this guard
/// lives — the credential twin of `crate::agent::sandboxing::TestSandboxHost`,
/// and thread-local for the same reason.
///
/// Exists because the launch path resolves the store itself, and **no test may
/// consult a real keychain**: without this, a test of a launch that injects a
/// token would run the machine owner's `security`/`secret-tool` against their
/// own entries. The installed store is leaked, which is what makes it `'static`
/// and is bounded by the test binary's lifetime.
#[cfg(test)]
pub struct TestSecretStore;

#[cfg(test)]
impl TestSecretStore {
    pub fn install(store: impl SecretStore + 'static) -> Self {
        let leaked: &'static dyn SecretStore = Box::leak(Box::new(store));
        TEST_STORE.with(|installed| installed.set(Some(leaked)));
        Self
    }
}

#[cfg(test)]
impl Drop for TestSecretStore {
    fn drop(&mut self) {
        TEST_STORE.with(|installed| installed.set(None));
    }
}

/// Which store `host` offers.
///
/// Injected rather than resolved from `cfg!`, so the choice is testable — and
/// so a tool sitting somewhere a sandboxed agent could rewrite is refused here
/// rather than handed the user's token.
fn detect(host: &dyn ProbeHost) -> Box<dyn SecretStore> {
    let home = host.home();
    let candidates: &[(KeychainKind, &str, &str)] = &[
        (
            KeychainKind::MacSecurity,
            "security",
            "macOS ships `security`; a host that has lost it has bigger problems",
        ),
        (
            KeychainKind::Libsecret,
            "secret-tool",
            "install libsecret-tools (Debian/Ubuntu) or libsecret (Fedora/Arch), and run a Secret \
             Service such as gnome-keyring",
        ),
    ];
    let mut fixes: Vec<&str> = Vec::new();
    for (kind, program, fix) in candidates {
        let Some(path) = host.which(program) else {
            fixes.push(fix);
            continue;
        };
        if let Some(root) = dirs::rewritable_root(&path, home.as_deref()) {
            // A `security` under the home is a `security` a sandboxed agent can
            // replace, and the next launch would hand it the token.
            return Box::new(Unavailable::new(
                format!("'{path}' sits under '{root}', where a sandboxed agent could replace it"),
                format!("remove '{path}' so the system copy is found instead"),
            ));
        }
        return Box::new(SystemKeychain::new(*kind, path));
    }
    Box::new(Unavailable::new(
        "this host has no credential store friring can use",
        fixes.join("; "),
    ))
}

/// Whether a tool's stderr is its way of saying the entry is not there.
///
/// A missing entry is the ordinary case — it is what "the user has not supplied
/// a token yet" looks like — so it must not read as a failure. `secret-tool`
/// says nothing at all; `security` says so in words.
fn missing_entry(stderr: &str) -> bool {
    let stderr = stderr.trim();
    stderr.is_empty()
        || stderr.contains("could not be found")
        || stderr.contains("No such")
        || stderr.contains("no such")
}

/// The first non-empty line of a tool's stderr, which is where its own
/// actionable sentence is.
fn first_line(stderr: &str, fallback: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

/// A store whose every answer is scripted, for tests.
///
/// The whole reason [`SecretStore`] is a trait: no test may read a real
/// keychain, and this one cannot — it holds fabricated tokens in memory and
/// counts what was asked of it, so a test can also assert that a strategy which
/// must not consult the store did not.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct StubStore {
    entries: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    /// How many times [`SecretStore::get`] was called — the assertion behind
    /// "a policy backend never looks for a token".
    reads: std::sync::atomic::AtomicUsize,
    /// A store that is present but will not answer (a locked keychain).
    failure: Option<String>,
}

#[cfg(test)]
impl StubStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that holds `value` for `family`'s `variable`.
    pub fn with_token(self, family: &str, variable: &str, value: &str) -> Self {
        let key = SecretKey::new(family, variable).expect("a spellable key");
        self.entries
            .lock()
            .expect("uncontended")
            .insert(key.account(), value.to_string());
        self
    }

    /// A store that exists and refuses to answer.
    pub fn failing(reason: &str) -> Self {
        Self {
            failure: Some(reason.to_string()),
            ..Self::default()
        }
    }

    pub fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
impl SecretStore for StubStore {
    fn label(&self) -> &str {
        "a fabricated keychain"
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, String> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(reason) = &self.failure {
            return Err(reason.clone());
        }
        Ok(self
            .entries
            .lock()
            .expect("uncontended")
            .get(&key.account())
            .map(|value| Secret::new(value.clone())))
    }

    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), String> {
        if let Some(reason) = &self.failure {
            return Err(reason.clone());
        }
        self.entries
            .lock()
            .expect("uncontended")
            .insert(key.account(), secret.expose().to_string());
        Ok(())
    }

    fn remove(&self, key: &SecretKey) -> Result<(), String> {
        self.entries
            .lock()
            .expect("uncontended")
            .remove(&key.account());
        Ok(())
    }

    fn how_to_store(&self, key: &SecretKey) -> String {
        format!("put a token under {SERVICE}/{}", key.account())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::probe::StubHost;

    const FAKE: &str = "fabricated-token-4f2b9c";

    #[test]
    fn a_secret_will_not_print_itself() {
        let secret = Secret::new(FAKE);
        // The type that holds the value, formatted every way a log line, an
        // `expect` or a `{:?}` on a containing struct would format it.
        assert_eq!(format!("{secret:?}"), "Secret(withheld)");
        assert!(!format!("{secret:?}").contains(FAKE));
        assert!(!format!("{:?}", Some(secret.clone())).contains(FAKE));
        assert!(!format!("{:?}", vec![secret.clone()]).contains(FAKE));
        // …and the one deliberate way out still works.
        assert_eq!(secret.expose(), FAKE);
    }

    #[test]
    fn a_key_is_only_built_from_names_that_cannot_be_read_as_a_flag() {
        let key = SecretKey::new("claude", "ANTHROPIC_API_KEY").unwrap();
        assert_eq!(key.account(), "claude/ANTHROPIC_API_KEY");
        assert_eq!(key.variable(), "ANTHROPIC_API_KEY");
        assert_eq!(key.family(), "claude");

        for (family, variable) in [
            ("claude", "-w"),
            ("claude", "A B"),
            ("claude", "1KEY"),
            ("claude", ""),
            ("-s", "KEY"),
            ("../etc", "KEY"),
            ("", "KEY"),
            (".hidden", "KEY"),
            ("cla ude", "KEY"),
        ] {
            assert!(
                SecretKey::new(family, variable).is_none(),
                "{family}/{variable} must not be spellable as a key"
            );
        }
    }

    /// The store is stubbed everywhere, so this asserts the *selection* rather
    /// than any tool's behaviour: nothing here runs `security` or
    /// `secret-tool`, and a host with neither says so with the fix.
    #[test]
    fn the_store_is_chosen_from_what_the_host_has() {
        let mac = detect(&StubHost::new().with_binary("security"));
        assert_eq!(mac.label(), "the macOS keychain");
        let linux = detect(&StubHost::new().with_binary("secret-tool"));
        assert_eq!(linux.label(), "the system keyring");

        let bare = detect(&StubHost::new());
        let key = SecretKey::new("claude", "ANTHROPIC_API_KEY").unwrap();
        assert!(bare.label().contains("no credential store"));
        assert_eq!(bare.get(&key).unwrap(), None);
        assert!(bare.how_to_store(&key).contains("libsecret"));
    }

    /// A tool a sandboxed agent could replace is refused rather than handed the
    /// user's token — the rule `bwrap` and the container engines already follow.
    #[test]
    fn a_rewritable_credential_tool_is_refused() {
        let planted = StubHost::new()
            .with_home("/home/u")
            .with_binary_at("secret-tool", "/home/u/.local/bin/secret-tool");
        let store = detect(&planted);
        assert!(
            store.label().contains("could replace it"),
            "{}",
            store.label()
        );
        // …and it is inert, so a launch degrades to signing in rather than
        // reading a token through a binary the agent chose.
        let key = SecretKey::new("claude", "ANTHROPIC_API_KEY").unwrap();
        assert_eq!(store.get(&key).unwrap(), None);
    }

    /// macOS is the platform whose tool cannot take a value off the command
    /// line, and friring says so with the command that can.
    #[test]
    fn macos_refuses_to_write_and_hands_back_the_command_that_prompts() {
        let store = SystemKeychain::new(KeychainKind::MacSecurity, "/usr/bin/security");
        let key = SecretKey::new("claude", "ANTHROPIC_API_KEY").unwrap();
        let err = store.set(&key, &Secret::new(FAKE)).unwrap_err();
        assert!(err.contains("add-generic-password"), "{err}");
        assert!(err.contains("prompts for the value"), "{err}");
        // The refusal reports the token nowhere — it is the whole reason the
        // write is refused.
        assert!(!err.contains(FAKE), "{err}");
        assert!(!store.how_to_store(&key).contains(FAKE));
        assert!(store
            .how_to_store(&key)
            .contains("claude/ANTHROPIC_API_KEY"));
    }

    #[test]
    fn a_missing_entry_is_not_a_failure_but_a_locked_store_is() {
        assert!(missing_entry(""));
        assert!(missing_entry(
            "security: SecKeychainSearchCopyNext: The specified item could not be found in the \
             keychain."
        ));
        assert!(!missing_entry(
            "The user name or passphrase you entered is not correct."
        ));
        assert_eq!(first_line("\n  boom  \nmore", "fallback"), "boom");
        assert_eq!(first_line("   ", "fallback"), "fallback");
    }

    #[test]
    fn the_stub_answers_only_what_it_was_told_and_counts_the_asking() {
        let store = StubStore::new().with_token("claude", "ANTHROPIC_API_KEY", FAKE);
        let key = SecretKey::new("claude", "ANTHROPIC_API_KEY").unwrap();
        let other = SecretKey::new("codex", "OPENAI_API_KEY").unwrap();
        assert_eq!(store.get(&key).unwrap().unwrap().expose(), FAKE);
        assert_eq!(store.get(&other).unwrap(), None);
        assert_eq!(store.reads(), 2);

        store.set(&other, &Secret::new("second")).unwrap();
        assert_eq!(store.get(&other).unwrap().unwrap().expose(), "second");
        store.remove(&other).unwrap();
        assert_eq!(store.get(&other).unwrap(), None);

        let locked = StubStore::failing("the keychain is locked");
        assert_eq!(locked.get(&key).unwrap_err(), "the keychain is locked");
    }
}
