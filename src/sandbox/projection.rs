//! Projecting a user's agent configuration into a place.
//!
//! A **place** gives the agent a synthetic per-profile home
//! ([`dirs::place_home_dir`]) and nothing
//! else — never a bind of the host's agent configuration, which is both useless
//! (on macOS the credentials are in the Keychain, not the file) and an escape
//! channel, because an agent that can write the host's agent settings can plant
//! hooks the *host* agent later runs outside the boundary (ADR-28). An empty
//! home is safe and unusable: the agent sees none of the user's instructions,
//! skills or commands.
//!
//! So the safe subset is **copied in**, and this module decides what "safe"
//! means. Every entry is home-relative on both sides — `~/.claude/skills` lands
//! at `$HOME/.claude/skills` inside — because a place already relocates `$HOME`,
//! so nothing about an agent's layout has to change and no path inside the
//! boundary has to be invented.
//!
//! # Why a lint pass rather than a copy
//!
//! Real agent configuration routinely names the host filesystem, and a place is
//! a different filesystem (usually a different operating system):
//!
//! - lifecycle hook commands, status-line commands and credential helpers are
//!   arbitrary shell, usually with absolute host paths;
//! - plugin, skill and rule directories may live outside the config directory;
//! - stdio MCP servers name host binaries.
//!
//! Copied verbatim, each of those is a *broken* agent rather than a projected
//! one — a hook pointing at `/Users/…/bin/x` inside a Linux container fails on
//! every event. So [`plan`] classifies every entry as
//! [`Projected`](Verdict::Projected), [`Rewritten`](Verdict::Rewritten),
//! [`NeedsMount`](Verdict::NeedsMount) or [`HostOnly`](Verdict::HostOnly), with
//! a reason, and [`ProjectionPlan::apply`] writes only what crosses.
//!
//! Nothing here is agent-specific. What an agent declares
//! ([`AgentSandboxDef::copy_in`], [`AgentSandboxDef::enforced`]) is data; what
//! this module knows is the shape of a *path*, not the shape of anybody's
//! settings schema.
//!
//! # A boundary in both directions
//!
//! Projection is copy-**in** only, and nothing it writes may open a route back
//! out:
//!
//! - no credential file crosses, ever — not the launching agent's own either.
//!   Rotating refresh tokens are single-use, so a copy and its original
//!   invalidate each other on the first refresh (ADR-28). A place's agent signs
//!   in inside its own pane.
//! - nothing that reaches friring's data directory crosses, so the database and
//!   the automation commands the *host* executes stay outside (ADR-29), and
//!   neither does anything reaching a tmux socket directory.
//! - projection adds no mount and mounts nothing. `NeedsMount` is a *sentence
//!   for the user*, not an action: widening the boundary stays a deliberate
//!   profile edit.
//! - every write refuses a symlink at every component. The synthetic home is
//!   writable by the sandbox, so the sandbox can plant one — and a link from its
//!   own home to the host's real agent directory would turn friring's next
//!   projection into a write into the very directory ADR-28 keeps out of reach.
//!
//! Projected files are `0600` (`0700` where the source was executable): content
//! crosses, and nothing else does.

pub mod document;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::sandbox::backend::{SandboxError, SandboxResult};
use crate::sandbox::dirs;
use crate::sandbox::secrets::{secrets_for, SecretPlatform};
use crate::session::{rewrite_status_signals_for_tmux, AgentSandboxDef, EnforcedSettings};

use document::Step;

/// Largest single file friring will project, in bytes.
///
/// Agent configuration is instructions, skills and settings — text a person
/// wrote. A megabyte is already generous for any of them, and the cap is what
/// keeps one enormous file (a model download parked in a skills directory, a
/// runaway log) out of a plan friring holds in memory and writes into a
/// container.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// Largest total friring will project for one launch, in bytes.
pub const MAX_TOTAL_BYTES: u64 = 8 << 20;

/// Most files friring will project for one launch.
pub const MAX_FILES: usize = 2048;

/// Deepest directory friring will walk under a declared entry.
///
/// A bound on the walk itself, not on anybody's taste in nesting: a directory
/// tree friring reads is a tree somebody else can grow.
pub const MAX_DEPTH: usize = 12;

/// Where friring's own managed configuration lands inside the boundary when it
/// does not live under the user's home — one fixed directory, so the mapping
/// stays a function of the file's name rather than of the host's layout.
pub const MANAGED_FALLBACK_DIR: &str = ".friring";

/// What friring decided about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Crossed as it stands.
    Projected,
    /// Crossed with a host reference rewritten to where it lands inside.
    Rewritten { inside: String },
    /// Did not cross, and a read-only grant for `host_path` in the profile would
    /// bring it in — the profile editor's "mount read-only, drop, or rewrite?".
    NeedsMount { host_path: String },
    /// Did not cross, and no grant would help.
    HostOnly,
}

impl Verdict {
    /// Whether this verdict is one the user has a decision to make about.
    pub fn is_actionable(&self) -> bool {
        matches!(self, Self::NeedsMount { .. } | Self::HostOnly)
    }
}

/// One classified entry, in the words the profile editor shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What was classified: a declared entry (`~/.claude/settings.json`), or a
    /// field inside one (`~/.claude/settings.json → mcpServers.docs`).
    pub entry: String,
    pub verdict: Verdict,
    /// Why, in a sentence that stands on its own.
    pub reason: String,
}

/// Everything [`plan`] needs. Injected rather than resolved, so a test never
/// consults the machine it runs on and a launch is composed against the home the
/// *agent* will see.
pub struct ProjectionInput<'a> {
    /// The profile whose place this is, for refusal messages.
    pub profile: &'a str,
    /// The agent's declaration. Without one, nothing of the *user's* crosses —
    /// friring's own [`managed`](Self::managed) payload still does, because it
    /// is friring's file and it is what makes the session report status.
    pub agent: Option<&'a AgentSandboxDef>,
    /// The absolute host paths the place mounts at identical paths — the
    /// profile's own, which is what decides whether a reference inside a
    /// projected document still resolves in there, and what pre-seeded
    /// workspace trust names.
    pub granted: &'a [String],
    /// The home the declaration's `~` entries are read from.
    pub home: &'a str,
    /// `$HOME` inside the place.
    pub inside_home: &'a str,
    /// The host platform, for the credential deny list.
    pub platform: SecretPlatform,
    /// friring's database, to keep out (ADR-29). A launch input rather than
    /// something resolved here, for the reason it is one everywhere else: the
    /// friring that owns the session is not necessarily the one on this host.
    pub friring_db: Option<&'a str>,
    /// friring's own configuration directory — the root every
    /// [`managed`](Self::managed) path must stay under.
    pub managed_root: &'a str,
    /// friring's own configuration files this launch's argv names (the bundled
    /// hooks payload). Carried inward with their status-signal commands
    /// rewritten to a tmux pane option, which is how a place reports
    /// working/blocked/done at all: `friring-cli session signal` needs a binary
    /// that need not be in the image and a database that may never be in the
    /// boundary.
    pub managed: &'a [String],
}

/// One file, as it will be written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedFile {
    /// Relative to the synthetic home, `/`-separated.
    rel: String,
    contents: Vec<u8>,
    executable: bool,
}

/// What one launch would project, and why.
///
/// Produced by [`plan`], which reads and writes nothing; [`apply`](Self::apply)
/// is the half with a side effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionPlan {
    /// Every classified entry, in path order.
    pub findings: Vec<Finding>,
    /// Where each friring-managed configuration path lands inside the boundary,
    /// so the launch can point the agent's own argument at it instead of at a
    /// host path that is not in there.
    pub arg_map: BTreeMap<String, String>,
    files: Vec<PlannedFile>,
    bytes: u64,
    /// Whether a projected file carries friring's status hooks.
    status: bool,
    /// Whether this launch had a hook payload to carry at all. Told apart from
    /// [`status`](Self::status) so a session whose agent has no hooks wired
    /// anywhere is not reported as one whose hooks friring dropped.
    status_expected: bool,
    /// Whether there was anything at all to project — what the agent declares,
    /// plus friring's own managed payload, which crosses regardless.
    declared: bool,
}

impl ProjectionPlan {
    /// How many files would be written.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// How many bytes would be written.
    pub fn byte_count(&self) -> u64 {
        self.bytes
    }

    /// Whether a place launched from this plan reports its status.
    pub fn reports_status(&self) -> bool {
        self.status
    }

    /// Where `host_path` lands inside the boundary, for a launch rewriting the
    /// agent's own arguments. `None` for a path that did not cross, which the
    /// caller drops together with the flag introducing it.
    pub fn inside_path(&self, host_path: &str) -> Option<&str> {
        self.arg_map.get(host_path).map(String::as_str)
    }

    /// The entries a user still has a decision to make about.
    pub fn actionable(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.verdict.is_actionable())
    }

    /// The composition line, for the session's `Sandbox:` row and the launch's
    /// log line.
    ///
    /// Says what crossed *and* what did not: a place whose agent silently has no
    /// skills, or one that silently never leaves `idle`, looks like a broken
    /// session, and there is nothing on screen connecting the two.
    pub fn summary(&self) -> String {
        if !self.declared {
            return "no config projected — this agent declares none".to_string();
        }
        let mut parts = vec![format!("config projected: {} files", self.files.len())];
        let mounts = self
            .findings
            .iter()
            .filter(|f| matches!(f.verdict, Verdict::NeedsMount { .. }))
            .count();
        if mounts > 0 {
            parts.push(format!("{mounts} need a read-only mount"));
        }
        let host_only = self
            .findings
            .iter()
            .filter(|f| f.verdict == Verdict::HostOnly)
            .count();
        if host_only > 0 {
            parts.push(format!("{host_only} host-only"));
        }
        // Only where friring had hooks to carry and they did not make it: an
        // agent with no hooks wired anywhere reports no state sandboxed or not,
        // and blaming the boundary for that would send the user looking in the
        // wrong place.
        if self.status_expected && !self.status {
            parts.push("friring's hooks did not cross — it reports no state".to_string());
        }
        parts.join(" · ")
    }

    /// Write the projected files into the synthetic home.
    ///
    /// Answers how many files were written. Idempotent: every file is
    /// overwritten, and nothing friring did not project is removed — the agent's
    /// own state lives in the same tree, and a launch that wiped it would sign
    /// the agent out on every restart.
    ///
    /// # Errors
    ///
    /// The synthetic home is missing or is not a directory, a component of a
    /// projected path is a symlink or is not a directory, or a write failed.
    /// Every one of those is refused rather than repaired: the home is writable
    /// by the sandbox, so anything unexpected in it was put there by the thing
    /// the boundary exists to constrain.
    pub fn apply(&self, place_home: &Path) -> SandboxResult<usize> {
        verify_root(place_home)?;
        for file in &self.files {
            write_into(place_home, &file.rel, &file.contents, file.executable)?;
        }
        Ok(self.files.len())
    }
}

/// Classify everything the agent declares, and build the files that would cross.
///
/// Reads the host filesystem — that is the whole job — and writes nothing, so
/// the profile editor can show the same verdicts a launch would apply. An entry
/// that is not on the host is not a finding: there is nothing to classify.
pub fn plan(input: &ProjectionInput<'_>) -> ProjectionPlan {
    let mut plan = ProjectionPlan::default();
    // friring's own hook payload crosses whether or not the agent declares
    // anything: it is friring's file, and it is what makes the session report
    // status at all. Only what the *user's* configuration contributes is gated
    // on a declaration.
    let declared = input
        .agent
        .map(|a| a.copy_in.as_slice())
        .unwrap_or_default();
    let enforced = input
        .agent
        .map(|a| a.enforced.as_slice())
        .unwrap_or_default();
    plan.declared = !declared.is_empty() || !enforced.is_empty() || !input.managed.is_empty();
    plan.status_expected = !input.managed.is_empty();

    let mut collector = Collector::new(input);
    for entry in declared {
        collector.declared_entry(entry);
    }
    for path in input.managed {
        collector.managed_entry(path);
    }
    let Collector {
        mut candidates,
        mut findings,
        mut arg_map,
        status,
        ..
    } = collector;

    // Every enforced document lands whether or not the user's own configuration
    // put a file there, so its path counts as reachable before anything is
    // linted — a projected hook that names it must not be dropped for pointing
    // at a file friring is about to write.
    let enforced_rels: Vec<String> = enforced
        .iter()
        .filter_map(EnforcedSettings::home_relative)
        .map(str::to_string)
        .collect();
    let reach = Reach::new(
        input,
        candidates
            .iter()
            .map(|c| c.rel.clone())
            .chain(enforced_rels.iter().cloned()),
    );

    // Second pass: a document is only linted once the whole projected set is
    // known, because "does this reference still resolve?" is a question about
    // that set.
    candidates.retain_mut(|candidate| lint_candidate(candidate, &reach, &mut findings));
    let mut files: BTreeMap<String, PlannedFile> = candidates
        .into_iter()
        .map(|candidate| {
            (
                candidate.rel.clone(),
                PlannedFile {
                    rel: candidate.rel,
                    contents: candidate.contents,
                    executable: candidate.executable,
                },
            )
        })
        .collect();
    // A managed file the lint withheld is one the launch must stop naming: an
    // argument pointing at a path nothing wrote is the "settings file not found"
    // that kills a pane on startup.
    let inside_root = input.inside_home.trim_end_matches('/');
    arg_map.retain(|_, inside| {
        inside
            .strip_prefix(inside_root)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rel| files.contains_key(rel))
    });

    apply_enforced(enforced, input, &reach, &mut files, &mut findings);

    // Only if the file that carried them is one that actually landed: a hook
    // payload the lint withheld reports nothing, and saying otherwise would hide
    // a session that never leaves `idle`.
    plan.status = status.is_some_and(|rel| files.contains_key(&rel));
    plan.bytes = files.values().map(|f| f.contents.len() as u64).sum();
    plan.files = files.into_values().collect();
    findings.sort_by(|a, b| a.entry.cmp(&b.entry));
    plan.findings = findings;
    plan.arg_map = arg_map;
    plan
}

/// A file that survived the entry-level rules and is waiting to be linted.
struct Candidate {
    /// Where it came from, `~`-anchored, for a finding that names it.
    entry: String,
    /// Where it lands, relative to the synthetic home.
    rel: String,
    contents: Vec<u8>,
    executable: bool,
}

/// One absolute host root nothing may cross, and how far the refusal reaches.
struct Forbidden {
    root: String,
    /// Whether naming an **ancestor** is refused too.
    ///
    /// True for a tree friring must not carry at all: an ancestor of the data
    /// directory carries the database as surely as the directory itself does
    /// (ADR-29), so both a `copy_in` entry above it and a mount suggestion for
    /// one are refused outright — the same "refuse, never trim" rule the profile
    /// validator applies.
    ///
    /// False for a credential, where the directory *containing* one is
    /// ordinarily the very directory worth projecting: `~/.claude` is projected
    /// and `~/.claude/.credentials.json` inside it is not.
    tree: bool,
    why: String,
}

/// Every root nothing may cross.
///
/// Shared by the entry walk and the document lint on purpose: a credential is no
/// less a credential for being named in a hook rather than declared in
/// `copy_in`, and a `copy_in` entry naming friring's data directory is the same
/// ADR-29 violation as a settings file that points at the database.
fn forbidden_roots(input: &ProjectionInput<'_>) -> Vec<Forbidden> {
    let mut roots: Vec<Forbidden> = dirs::protected_data_dirs(input.friring_db)
        .into_iter()
        .map(|dir| Forbidden {
            root: dir,
            tree: true,
            why: "friring's data directory holds the automation commands the host executes, so \
                  nothing crosses a boundary naming it (ADR-29)"
                .to_string(),
        })
        .collect();
    roots.push(Forbidden {
        root: dirs::tmux_socket_root().display().to_string(),
        tree: true,
        why: "a sandbox that can reach friring's own tmux socket can run commands in any pane, \
              outside the boundary"
            .to_string(),
    });
    // Deliberately `secrets_for(_, None)` rather than the launching agent's own
    // family: under host passthrough an agent keeps its own credential file, but
    // nothing is *copied* there. Here a copy is exactly what would happen, and a
    // rotating refresh token with two consumers invalidates itself on the first
    // refresh — so an agent's own credential is as host-only as a sibling's
    // (ADR-28).
    for secret in secrets_for(input.platform, None) {
        roots.push(Forbidden {
            root: secret.resolved(input.home),
            tree: false,
            why: secret.why.to_string(),
        });
    }
    if let Some(declared) = input
        .agent
        .and_then(|agent| agent.credential_file.as_deref())
        .and_then(|path| path.trim().strip_prefix("~/"))
        .filter(|rel| unsafe_relative(rel).is_none())
    {
        roots.push(Forbidden {
            root: format!("{}/{declared}", input.home.trim_end_matches('/')),
            tree: false,
            why: "the credential file this agent declares; no credential is ever copied into a \
                  sandbox, because the copy and the original invalidate each other on the first \
                  refresh (ADR-28)"
                .to_string(),
        });
    }
    roots
}

/// The root covering `path`: the one it is inside, or — for a whole tree — the
/// one it sits above.
fn covering<'r>(roots: &'r [Forbidden], path: &str) -> Option<&'r Forbidden> {
    roots.iter().find(|forbidden| {
        dirs::encloses(&forbidden.root, path)
            || (forbidden.tree && dirs::encloses(path, &forbidden.root))
    })
}

/// The entry-level walk: what may be read at all.
struct Collector<'a> {
    input: &'a ProjectionInput<'a>,
    forbidden: Vec<Forbidden>,
    candidates: Vec<Candidate>,
    findings: Vec<Finding>,
    arg_map: BTreeMap<String, String>,
    bytes: u64,
    /// Where friring's status hooks landed, when a managed file actually carried
    /// them — read from the rewrite having something to do, rather than from the
    /// projected text, so a user's own file that happens to mention the pane
    /// option cannot make a silent session claim it reports state.
    status: Option<String>,
}

impl<'a> Collector<'a> {
    fn new(input: &'a ProjectionInput<'a>) -> Self {
        Self {
            input,
            forbidden: forbidden_roots(input),
            candidates: Vec::new(),
            findings: Vec::new(),
            arg_map: BTreeMap::new(),
            bytes: 0,
            status: None,
        }
    }

    fn refuse(&mut self, entry: &str, reason: String) {
        self.findings.push(Finding {
            entry: entry.to_string(),
            verdict: Verdict::HostOnly,
            reason,
        });
    }

    /// One `copy_in` entry: a file or a directory tree, home-relative on both
    /// sides.
    fn declared_entry(&mut self, entry: &str) {
        let entry = entry.trim();
        let Some(rel) = entry.strip_prefix("~/").filter(|rel| !rel.is_empty()) else {
            self.refuse(
                entry,
                format!(
                    "'{entry}' is not written '~/…'. A projected entry keeps its home-relative \
                     path inside the boundary, so it has to be written relative to a home"
                ),
            );
            return;
        };
        if let Some(reason) = unsafe_relative(rel) {
            self.refuse(entry, reason);
            return;
        }
        let before = self.candidates.len();
        let source = PathBuf::from(format!("{}/{rel}", self.input.home.trim_end_matches('/')));
        self.take(entry, rel, &source, 0);
        let taken = self.candidates.len() - before;
        if taken > 0 {
            self.findings.push(Finding {
                entry: entry.to_string(),
                verdict: Verdict::Projected,
                reason: format!("{taken} file(s) copied into the sandbox's own home"),
            });
        }
    }

    /// One of friring's own configuration files, carried in with its status
    /// hooks rewritten for a boundary that cannot write friring's database.
    fn managed_entry(&mut self, path: &str) {
        let entry = path.to_string();
        // The caller only offers paths under friring's own config directory, and
        // the local claude launch points that argument at a per-session symlink
        // to the shared payload — so a link is followed here, and only here, and
        // only as far as that same root.
        let inside_root = std::fs::canonicalize(self.input.managed_root)
            .ok()
            .and_then(|root| {
                let target = std::fs::canonicalize(path).ok()?;
                dirs::encloses(&root.display().to_string(), &target.display().to_string())
                    .then_some(target)
            });
        let Some(target) = inside_root else {
            self.refuse(
                &entry,
                format!(
                    "'{path}' does not resolve inside friring's own configuration directory \
                     ('{}'), so friring will not carry it into the sandbox",
                    self.input.managed_root
                ),
            );
            return;
        };
        let Some(rel) = self.managed_rel(path) else {
            self.refuse(
                &entry,
                format!("'{path}' has no file name friring could land inside the boundary"),
            );
            return;
        };
        let Some(text) = self.read_text(&entry, &target) else {
            return;
        };
        let inside = format!("{}/{rel}", self.input.inside_home.trim_end_matches('/'));
        let rewritten = rewrite_status_signals_for_tmux(&text);
        if rewritten != text {
            self.status = Some(rel.clone());
        }
        self.push(Candidate {
            entry: entry.clone(),
            rel,
            contents: rewritten.into_bytes(),
            executable: false,
        });
        self.arg_map.insert(entry.clone(), inside);
        self.findings.push(Finding {
            entry,
            verdict: Verdict::Projected,
            reason: "friring's own hook configuration, reporting status through a tmux pane \
                     option because a boundary has neither friring-cli nor the database \
                     (ADR-29)"
                .to_string(),
        });
    }

    /// Where a managed path lands: its home-relative path when it is under the
    /// home, and a fixed directory otherwise — an `XDG_CONFIG_HOME` outside the
    /// home has no home-relative spelling to keep.
    fn managed_rel(&self, path: &str) -> Option<String> {
        let home = self.input.home.trim_end_matches('/');
        if let Some(rel) = path
            .strip_prefix(home)
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|rel| unsafe_relative(rel).is_none())
        {
            return Some(rel.to_string());
        }
        let name = Path::new(path).file_name()?.to_str()?;
        Some(format!("{MANAGED_FALLBACK_DIR}/{name}"))
    }

    /// Take one filesystem entry, recursing into a directory.
    fn take(&mut self, entry: &str, rel: &str, source: &Path, depth: usize) {
        // Before anything is opened: a declared entry that reaches a credential
        // store, friring's data directory or a tmux socket directory is refused
        // whether the user meant it or not.
        if let Some(forbidden) = covering(&self.forbidden, &source.display().to_string()) {
            let (root, why) = (forbidden.root.clone(), forbidden.why.clone());
            // A credential's own reason says what it is; the rule about copying
            // one is friring's, so the refusal says that too rather than leaving
            // the reader to infer why a readable file did not cross.
            let rule = if forbidden.tree {
                ""
            } else {
                ". No credential is ever copied into a sandbox — the agent signs in inside its \
                 own pane (ADR-28)"
            };
            self.refuse(
                entry,
                format!("'{entry}' reaches '{root}', which never crosses a boundary: {why}{rule}"),
            );
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(source) else {
            // Not on this host. Nothing to classify, and an agent declaring an
            // entry the user has never created is the ordinary case.
            return;
        };
        if meta.file_type().is_symlink() {
            self.refuse(
                entry,
                format!(
                    "'{entry}' is a symlink. friring copies content and never follows a link out \
                     of the tree it was handed — one pointing at a credential store or at \
                     friring's own data directory would carry it across (ADR-29)"
                ),
            );
            return;
        }
        if meta.is_dir() {
            if depth >= MAX_DEPTH {
                self.refuse(
                    entry,
                    format!("'{entry}' nests deeper than {MAX_DEPTH} directories"),
                );
                return;
            }
            let Ok(children) = std::fs::read_dir(source) else {
                self.refuse(entry, format!("'{entry}' could not be listed"));
                return;
            };
            let mut names: Vec<String> = children
                .flatten()
                .filter_map(|child| child.file_name().to_str().map(str::to_string))
                .collect();
            names.sort();
            for name in names {
                let child_entry = format!("{entry}/{name}");
                let child_rel = format!("{rel}/{name}");
                if unsafe_relative(&child_rel).is_some() {
                    continue;
                }
                self.take(&child_entry, &child_rel, &source.join(&name), depth + 1);
            }
            return;
        }
        if !meta.is_file() {
            self.refuse(
                entry,
                format!(
                    "'{entry}' is not a regular file, and a socket or a device is not \
                     configuration"
                ),
            );
            return;
        }
        if let Some(name) = Path::new(rel).file_name().and_then(|n| n.to_str()) {
            if looks_like_a_credential(name) {
                self.refuse(
                    entry,
                    format!(
                        "'{entry}' is named like a credential, and no credential is ever copied \
                         into a sandbox (ADR-28)"
                    ),
                );
                return;
            }
        }
        if meta.len() > MAX_FILE_BYTES {
            self.refuse(
                entry,
                format!(
                    "'{entry}' is {} bytes; friring projects configuration, and nothing larger \
                     than {MAX_FILE_BYTES} bytes is configuration",
                    meta.len()
                ),
            );
            return;
        }
        let Ok(contents) = std::fs::read(source) else {
            self.refuse(entry, format!("'{entry}' could not be read"));
            return;
        };
        // Re-checked after the read: the file friring stat'ed and the file it
        // read are only the same file if nothing rewrote it in between.
        if contents.len() as u64 > MAX_FILE_BYTES {
            self.refuse(
                entry,
                format!("'{entry}' grew past {MAX_FILE_BYTES} bytes while it was being read"),
            );
            return;
        }
        let executable = is_executable(&meta);
        self.push(Candidate {
            entry: entry.to_string(),
            rel: rel.to_string(),
            contents,
            executable,
        });
    }

    fn push(&mut self, candidate: Candidate) {
        if self.candidates.len() >= MAX_FILES {
            let reason = format!(
                "projecting '{}' would take this launch past {MAX_FILES} files",
                candidate.entry
            );
            self.refuse(&candidate.entry, reason);
            return;
        }
        let size = candidate.contents.len() as u64;
        if self.bytes + size > MAX_TOTAL_BYTES {
            let reason = format!(
                "projecting '{}' would take this launch past {MAX_TOTAL_BYTES} bytes",
                candidate.entry
            );
            self.refuse(&candidate.entry, reason);
            return;
        }
        self.bytes += size;
        self.candidates.push(candidate);
    }

    fn read_text(&mut self, entry: &str, source: &Path) -> Option<String> {
        match std::fs::metadata(source) {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                self.refuse(entry, format!("'{entry}' is larger than {MAX_FILE_BYTES}"));
                return None;
            }
            Ok(meta) if meta.is_file() => {}
            _ => {
                self.refuse(entry, format!("'{entry}' is not a readable regular file"));
                return None;
            }
        }
        match std::fs::read_to_string(source) {
            Ok(text) => Some(text),
            Err(_) => {
                self.refuse(entry, format!("'{entry}' could not be read as UTF-8"));
                None
            }
        }
    }
}

/// Why a relative path may not be joined onto anything, or `None`.
///
/// The one rule that has to hold before a declared string becomes a path on
/// either side of the boundary.
fn unsafe_relative(rel: &str) -> Option<String> {
    if rel.starts_with('/') || rel.contains('\\') {
        return Some(format!(
            "'{rel}' is not a relative POSIX path, and a projected entry keeps its home-relative \
             path inside the boundary"
        ));
    }
    if rel
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Some(format!(
            "'{rel}' carries an empty, '.' or '..' component, which would name something outside \
             the sandbox's own home"
        ));
    }
    if rel.contains('\0') {
        return Some(format!("'{rel}' contains a NUL byte"));
    }
    None
}

/// Whether a file name says "credential" on its own, for anything the deny list
/// does not already name.
///
/// Matched on the **name**, never on the directory around it, so a skill about
/// authentication is content while `auth.json` beside it is not. Erring towards
/// refusing is deliberate: a missing skill is reported and fixable, and a copied
/// refresh token invalidates the user's real login (ADR-28).
fn looks_like_a_credential(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "credential",
        "secret",
        "password",
        "token",
        "auth.json",
        "auth.toml",
        "id_rsa",
        "id_ecdsa",
        "id_ed25519",
        "keychain",
    ];
    const SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".env"];
    lower == ".env"
        || NEEDLES.iter().any(|needle| lower.contains(needle))
        || SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// Whether the source file's owner may execute it.
fn is_executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o100 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

// ---- The lint pass ------------------------------------------------------

/// What a path named inside a projected document resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reference {
    /// Not a path, or one that already resolves inside the boundary.
    Fine,
    /// Resolves only once the text becomes this.
    Rewrite(String),
    /// Never crosses, whatever the profile says.
    Forbidden(String),
    /// Outside the boundary; a read-only grant would bring it in.
    Mountable(String),
    /// Outside the boundary and no grant would help.
    Unreachable(String),
}

/// What the boundary makes reachable, and where a host path lands inside it.
struct Reach {
    home: String,
    inside_home: String,
    granted: Vec<String>,
    /// Home-relative paths that are projected, and every directory above them.
    projected: BTreeSet<String>,
    /// Roots nothing may name, with the reason each carries.
    forbidden: Vec<Forbidden>,
}

impl Reach {
    fn new(input: &ProjectionInput<'_>, projected: impl Iterator<Item = String>) -> Self {
        let mut reachable = BTreeSet::new();
        for rel in projected {
            let mut prefix = String::new();
            for part in rel.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                reachable.insert(prefix.clone());
            }
        }
        Self {
            home: input.home.trim_end_matches('/').to_string(),
            inside_home: input.inside_home.trim_end_matches('/').to_string(),
            granted: input.granted.to_vec(),
            projected: reachable,
            forbidden: forbidden_roots(input),
        }
    }

    /// Classify one token out of a document's string.
    fn resolve(&self, token: &str) -> Reference {
        // A drive letter or a UNC name cannot be a path in a POSIX place, and
        // guessing at a translation would invent one.
        if token.starts_with("\\\\")
            || token
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
                && token.get(1..3).is_some_and(|s| s == ":\\" || s == ":/")
        {
            return Reference::Forbidden(format!(
                "'{token}' is a Windows path, and a place's filesystem is POSIX"
            ));
        }
        // `~/x`, `$HOME/x` and `${HOME}/x` already mean "the home the agent
        // has", which inside a place is the synthetic one — so the text is
        // right if and only if the file was projected, and never needs editing.
        for prefix in ["~/", "$HOME/", "${HOME}/"] {
            if let Some(rel) = token.strip_prefix(prefix) {
                return self.home_relative(token, rel, None);
            }
        }
        if !token.starts_with('/') {
            return Reference::Fine;
        }
        if let Some((root, why)) = self.forbidden_root(token) {
            return Reference::Forbidden(format!("'{token}' reaches '{root}': {why}"));
        }
        if self.granted.iter().any(|path| dirs::encloses(path, token)) {
            // Mounted at exactly its host path, so the text is already right.
            return Reference::Fine;
        }
        if let Some(rel) = token
            .strip_prefix(&self.home)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            let inside = format!("{}/{rel}", self.inside_home);
            return self.home_relative(token, rel, Some(inside));
        }
        self.outside(token)
    }

    /// A home-relative reference: reachable exactly when it was projected.
    fn home_relative(&self, token: &str, rel: &str, rewrite: Option<String>) -> Reference {
        if unsafe_relative(rel).is_some() {
            return Reference::Forbidden(format!(
                "'{token}' names something outside the home it is written against"
            ));
        }
        let host = format!("{}/{rel}", self.home);
        if let Some((root, why)) = self.forbidden_root(&host) {
            return Reference::Forbidden(format!("'{token}' reaches '{root}': {why}"));
        }
        if self.projected.contains(rel) {
            return match rewrite {
                Some(inside) => Reference::Rewrite(inside),
                None => Reference::Fine,
            };
        }
        self.outside(&host)
    }

    /// Outside the boundary: a directory a profile could grant, or a host binary
    /// no grant would make runnable.
    fn outside(&self, host: &str) -> Reference {
        if Path::new(host).is_dir() {
            Reference::Mountable(host.to_string())
        } else {
            Reference::Unreachable(format!(
                "'{host}' is a file on this host, not a path inside the boundary. A place runs its \
                 image's binaries, so a host script or executable is not one it could run even if \
                 it were mounted"
            ))
        }
    }

    fn forbidden_root(&self, path: &str) -> Option<(&str, &str)> {
        covering(&self.forbidden, path)
            .map(|forbidden| (forbidden.root.as_str(), forbidden.why.as_str()))
    }
}

/// Everything that ends a path token inside a shell command or a bare value.
///
/// Deliberately not `:` — a colon is legal in a path, and splitting on one would
/// turn a single reference into two fragments and classify neither correctly.
const TOKEN_BREAKS: &[char] = &[
    ' ', '\t', '\n', '\r', '"', '\'', '=', ',', ';', '|', '&', '(', ')', '<', '>', '`', '*', '?',
];

/// The path-looking tokens inside one string, with the byte range each occupies.
///
/// The range is what a rewrite splices over. Replacing by *text* would be wrong
/// wherever one token is a prefix of another (`~/.claude` beside
/// `~/.claude-backup`): a substring replace would edit both, and the second one
/// would come out as a path that never existed on either side.
fn path_tokens(text: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut take = |start: usize, end: usize| {
        let token = &text[start..end];
        let windows_drive = token
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            && token.get(1..3).is_some_and(|s| s == ":\\" || s == ":/");
        if token.starts_with('/')
            || token.starts_with("~/")
            || token.starts_with("$HOME/")
            || token.starts_with("${HOME}/")
            || token.starts_with("\\\\")
            || windows_drive
        {
            out.push((start..end, token));
        }
    };
    for (index, ch) in text.char_indices() {
        if TOKEN_BREAKS.contains(&ch) {
            take(start, index);
            start = index + ch.len_utf8();
        }
    }
    take(start, text.len());
    out
}

/// Lint one candidate document in place. `false` means it does not cross at all.
///
/// A document friring cannot read is **withheld**, not emptied: an empty file at
/// a settings path is not "no settings", it is a settings file the agent fails
/// to parse — which is the pane dying on startup rather than an agent running
/// with its defaults.
fn lint_candidate(candidate: &mut Candidate, reach: &Reach, findings: &mut Vec<Finding>) -> bool {
    let Some(format) = Path::new(&candidate.rel)
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(document::format_of)
    else {
        // Content, not configuration. Instructions, skills and commands are
        // prose: a path mentioned in them names nothing and executes nothing, so
        // editing them would be friring rewriting the user's words.
        return true;
    };
    let withhold = |findings: &mut Vec<Finding>, reason: String| {
        findings.push(Finding {
            entry: candidate.entry.clone(),
            verdict: Verdict::HostOnly,
            reason,
        });
        false
    };
    let text = match std::str::from_utf8(&candidate.contents) {
        Ok(text) => text.to_string(),
        Err(_) => {
            return withhold(
                findings,
                format!(
                    "'{}' is named as a {format} document but is not UTF-8, so friring cannot \
                     check what it points at",
                    candidate.entry
                ),
            )
        }
    };
    let mut tree = match document::parse(&text, format) {
        Ok(tree) => tree,
        Err(detail) => {
            return withhold(
                findings,
                format!(
                    "'{}' does not parse as {format} ({detail}), so friring cannot tell what it \
                     points at on this host",
                    candidate.entry
                ),
            )
        }
    };

    let mut drops: Vec<Vec<Step>> = Vec::new();
    let mut rewrites: Vec<(Vec<Step>, String)> = Vec::new();
    for (path, value) in document::strings(&tree) {
        let mut splices: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        let mut verdict: Option<(Verdict, String)> = None;
        for (range, token) in path_tokens(&value) {
            match reach.resolve(token) {
                Reference::Fine => {}
                Reference::Rewrite(inside) => {
                    if verdict.is_none() {
                        verdict = Some((
                            Verdict::Rewritten {
                                inside: inside.clone(),
                            },
                            format!(
                                "'{token}' is projected, and inside the boundary it is at \
                                 '{inside}'"
                            ),
                        ));
                    }
                    splices.push((range, inside));
                }
                Reference::Forbidden(reason) | Reference::Unreachable(reason) => {
                    verdict = Some((Verdict::HostOnly, reason));
                    break;
                }
                Reference::Mountable(host_path) => {
                    let reason = format!(
                        "'{host_path}' is outside this profile's paths. Grant it read-only and \
                         the reference works unchanged — a place mounts every path at exactly its \
                         host path"
                    );
                    verdict = Some((Verdict::NeedsMount { host_path }, reason));
                    break;
                }
            }
        }
        let Some((verdict, reason)) = verdict else {
            continue;
        };
        let dropped = verdict.is_actionable();
        findings.push(Finding {
            entry: format!(
                "{} → {}",
                candidate.entry,
                document::render_path(document::entry_of(&path))
            ),
            verdict,
            reason,
        });
        if dropped {
            drops.push(path);
            continue;
        }
        // Spliced from the end, so an earlier replacement cannot move a later
        // range out from under itself.
        let mut rewritten = value.clone();
        for (range, inside) in splices.into_iter().rev() {
            rewritten.replace_range(range, &inside);
        }
        rewrites.push((path, rewritten));
    }

    for (path, text) in rewrites {
        document::set_string(&mut tree, &path, text);
    }
    // Deepest and highest-indexed first: removing an array element renumbers its
    // siblings, and a later removal by an index taken before it would take the
    // wrong one.
    drops.sort();
    drops.reverse();
    for path in drops {
        document::drop_entry(&mut tree, &path);
    }

    match document::render(&tree, format) {
        Ok(text) => {
            candidate.contents = text.into_bytes();
            true
        }
        Err(detail) => withhold(
            findings,
            format!(
                "'{}' could not be written back as {format} ({detail})",
                candidate.entry
            ),
        ),
    }
}

/// Overlay each declared enforced-settings document on whatever projected to the
/// same path.
///
/// The overlay is what makes it the highest-precedence layer: the user's own
/// file is the base, so nothing under it can take back a key friring names.
fn apply_enforced(
    enforced: &[EnforcedSettings],
    input: &ProjectionInput<'_>,
    reach: &Reach,
    files: &mut BTreeMap<String, PlannedFile>,
    findings: &mut Vec<Finding>,
) {
    let mut workspaces = input.granted.to_vec();
    workspaces.sort();
    workspaces.dedup();

    for declared in enforced {
        let entry = format!("{} (enforced)", declared.path);
        let refuse = |findings: &mut Vec<Finding>, reason: String| {
            findings.push(Finding {
                entry: entry.clone(),
                verdict: Verdict::HostOnly,
                reason,
            });
        };
        let (Some(rel), Ok(rendered)) = (declared.home_relative(), declared.render(&workspaces))
        else {
            refuse(
                findings,
                format!(
                    "sandbox profile '{}' cannot apply this agent's enforced settings: {}",
                    input.profile,
                    declared.invalid().unwrap_or_default()
                ),
            );
            continue;
        };
        let overlay = match document::parse(&rendered, declared.format) {
            Ok(overlay) => overlay,
            Err(detail) => {
                refuse(
                    findings,
                    format!(
                        "the declared template does not render a {} document ({detail}), so \
                         friring wrote nothing rather than a file the agent fails to parse",
                        declared.format
                    ),
                );
                continue;
            }
        };
        // friring's own document is held to the rule everything else is: a path
        // in it that the boundary does not reach would be friring writing the
        // very reference the lint pass strips out of the user's file.
        if let Some(reason) = document::strings(&overlay)
            .iter()
            .flat_map(|(_, value)| path_tokens(value))
            .find_map(|(_, token)| match reach.resolve(token) {
                Reference::Fine => None,
                Reference::Rewrite(inside) => Some(format!(
                    "'{token}' is not where it lands inside the boundary ('{inside}')"
                )),
                Reference::Forbidden(reason)
                | Reference::Unreachable(reason)
                | Reference::Mountable(reason) => Some(reason),
            })
        {
            refuse(
                findings,
                format!("the declared template names a path the boundary does not reach: {reason}"),
            );
            continue;
        }

        let base = files
            .get(rel)
            .and_then(|file| std::str::from_utf8(&file.contents).ok())
            .and_then(|text| document::parse(text, declared.format).ok())
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        match document::render(&document::merge(base, overlay), declared.format) {
            Ok(text) => {
                files.insert(
                    rel.to_string(),
                    PlannedFile {
                        rel: rel.to_string(),
                        contents: text.into_bytes(),
                        executable: false,
                    },
                );
                findings.push(Finding {
                    entry,
                    verdict: Verdict::Projected,
                    reason: format!(
                        "friring's own layer, over anything projected to the same path — {} \
                         granted path(s) pre-trusted so a fresh home does not prompt",
                        workspaces.len()
                    ),
                });
            }
            Err(detail) => refuse(
                findings,
                format!(
                    "the merged document could not be written as {} ({detail})",
                    declared.format
                ),
            ),
        }
    }
}

// ---- The writer ---------------------------------------------------------

fn io_error(path: &Path, detail: impl Into<String>) -> SandboxError {
    SandboxError::Io {
        path: path.display().to_string(),
        detail: detail.into(),
    }
}

/// The synthetic home has to be there, and has to be a directory friring can
/// trust as the root of everything below.
fn verify_root(root: &Path) -> SandboxResult<()> {
    match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io_error(
            root,
            "the sandbox's home is a symlink; friring will not project configuration through one",
        )),
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(io_error(root, "the sandbox's home is not a directory")),
        Err(e) => Err(io_error(root, e.to_string())),
    }
}

/// Write one projected file under `root`, refusing a symlink at every component.
///
/// The synthetic home is writable by the sandbox by design — that is where the
/// agent's own state lives — so every directory on the way down is one the
/// sandbox could have replaced. `create_dir` is used rather than a recursive
/// create because `mkdir(2)` never creates or follows a symlink: it either makes
/// the directory (and friring knows what it is) or fails, and the failure is
/// checked rather than assumed.
fn write_into(root: &Path, rel: &str, contents: &[u8], executable: bool) -> SandboxResult<()> {
    use std::io::Write as _;

    if let Some(reason) = unsafe_relative(rel) {
        return Err(io_error(root, reason));
    }
    let mut components: Vec<&str> = rel.split('/').collect();
    let Some(name) = components.pop() else {
        return Err(io_error(root, format!("'{rel}' names no file")));
    };

    let mut at = root.to_path_buf();
    for component in components {
        at.push(component);
        match std::fs::create_dir(&at) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match std::fs::symlink_metadata(&at) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(io_error(
                            &at,
                            "is a symlink inside the sandbox's own home; friring will not project \
                             configuration through one, because the sandbox chose where it points",
                        ))
                    }
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => return Err(io_error(&at, "exists and is not a directory")),
                    Err(e) => return Err(io_error(&at, e.to_string())),
                }
            }
            Err(e) => return Err(io_error(&at, e.to_string())),
        }
        set_private(&at, false)?;
    }

    let path = at.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io_error(
                &path,
                "is a symlink inside the sandbox's own home; friring will not project \
                 configuration through one",
            ))
        }
        Ok(meta) if !meta.is_file() => {
            return Err(io_error(&path, "exists and is not a regular file"))
        }
        _ => {}
    }

    // Staged through an `O_EXCL` sibling and renamed into place, so a link
    // planted in the race window is replaced rather than written through, and a
    // hard link to something inside the boundary keeps its own inode.
    //
    // The name carries a counter as well as the pid: sessions of one profile
    // share a place, so two of this process's launches can be projecting into
    // one home at once, and a shared staging name would have each unlinking the
    // other's file mid-write.
    static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ticket = STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = at.join(format!(
        ".friring-projection-{}-{ticket}.tmp",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&staging);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut file = options
        .open(&staging)
        .map_err(|e| io_error(&staging, e.to_string()))?;
    file.write_all(contents)
        .map_err(|e| io_error(&staging, e.to_string()))?;
    drop(file);
    set_private(&staging, executable)?;
    std::fs::rename(&staging, &path).map_err(|e| io_error(&path, e.to_string()))
}

/// Re-assert the mode, which `create_dir`/`OpenOptions` only apply to what they
/// actually created.
fn set_private(path: &Path, executable: bool) -> SandboxResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if executable { 0o700 } else { 0o600 };
        let mode = if path.is_dir() { 0o700 } else { mode };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| io_error(path, e.to_string()))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
