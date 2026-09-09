//! A bridge child's **private** agent state, and how it is seeded (ADR-31).
//!
//! A bridge child never runs from its family's shared state directory. It gets
//! `<data>/sandbox/tmp/<child>/state`, exposed through the agent's own
//! `config_dir_env`, and the subtract set denies the family's — so a worker's
//! transcripts, history, logs and session list are its own, and no sibling's or
//! owner's are readable from inside it.
//!
//! **There is no shared-state mode and no fallback to one.** A seed that cannot
//! be made refuses the launch with `state_unrelocatable`; it never degrades to
//! "run from the family directory anyway". The whole point of the private
//! directory is that a leader and its workers cannot read each other's
//! conversations, and a fallback would make that property conditional on
//! nothing having gone wrong.
//!
//! # Who decides what
//!
//! The **agent** declares what it needs (`child_state_seed`): a path under its
//! state directory and the mode that path has to arrive in. The **profile**
//! authorizes (`child_seed_allow`): the exact path, in the exact mode. A seed
//! reaches a child only when both say so, so an operator decides which
//! credential and configuration surfaces a worker receives and an agent registry
//! cannot widen them.
//!
//! # The four modes
//!
//! - `symlink` — a link to the parent's file or directory, plus read on that
//!   exact target. Read-only skill or prompt material.
//! - `copy` — a copy. The child's diverges and nothing it writes reaches the
//!   parent.
//! - `copy-rewrite` — a copy of a UTF-8 text file with every occurrence of the
//!   parent's resolved state directory replaced by the private one. How a
//!   path-keyed hook configuration keeps working from a new location.
//! - `link-rw` — the **credential** mode: a link to the parent's file plus read
//!   *and write* on that exact target. A long-lived credential the agent
//!   refreshes is refreshed in the one file the owner also uses — the same
//!   sharing the vendor's own concurrent sessions already do on a host — so no
//!   second copy of a rotating token ever exists, which is ADR-28's condition.
//!   Legal only under the conditions in [`SeedPlan::build`].

use std::path::{Path, PathBuf};

use crate::session::{
    AgentSandboxDef, ChildSeedAllow, ChildStateSeed, SeedGrant, SeedMode, SubtractPath,
};

/// The largest file `copy-rewrite` will read.
///
/// A hook configuration is a few kilobytes. Anything approaching a megabyte is
/// not one, and rewriting it would mean holding it whole in memory to search it.
pub const MAX_REWRITE_BYTES: u64 = 1024 * 1024;

/// What friring will do to give one child its private state.
///
/// Built before anything is written, so a plan that cannot be carried out
/// refuses the launch rather than leaving a half-seeded directory behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedPlan {
    /// The child's private directory.
    pub state_dir: PathBuf,
    /// The family's directory the seeds come from.
    pub source_dir: PathBuf,
    /// One step per authorized, present seed.
    pub steps: Vec<SeedStep>,
}

/// One seed, resolved to absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedStep {
    /// Where it comes from, on the host.
    pub source: PathBuf,
    /// Where it lands, in the child's private directory.
    pub destination: PathBuf,
    pub mode: SeedMode,
}

impl SeedPlan {
    /// Decide what a child's private state directory needs, or refuse the
    /// launch.
    ///
    /// `agent` declares, `authorized` is the profile's `child_seed_allow`,
    /// `home` expands the agent's `~`-anchored paths, and `exists` answers
    /// whether a source is there — injected so the plan is a pure function of
    /// its inputs and a test never consults the developer's filesystem.
    ///
    /// # Errors
    ///
    /// Every one of these refuses the launch with `state_unrelocatable`, and
    /// **none** of them degrades:
    ///
    /// - the agent declares no `config_dir_env` or no `state_dir`, so friring
    ///   has no way to point it at a private directory at all;
    /// - a seed path is not a normalized relative path;
    /// - a **required** seed is missing on disk;
    /// - a seed is not authorized by the profile, or is authorized in a
    ///   *different* mode — `auth.json` as `copy` and as `link-rw` mean
    ///   different things about where a refreshed token lands, so the mismatch
    ///   is a refusal rather than a downgrade;
    /// - `link-rw` is asked for and any of its three conditions fails: the
    ///   source must be the agent's declared `credential_file`, the agent must
    ///   declare `writeback = true` (its own assertion that it refreshes this
    ///   credential), and the profile must authorize that exact path in that
    ///   exact mode.
    pub fn build(
        agent: &AgentSandboxDef,
        authorized: &[ChildSeedAllow],
        state_dir: &Path,
        home: &str,
        exists: &dyn Fn(&Path) -> bool,
    ) -> Result<Self, String> {
        if agent
            .config_dir_env
            .as_deref()
            .unwrap_or_default()
            .is_empty()
        {
            return Err(
                "this agent declares no `config_dir_env`, so friring cannot point it at a \
                 private state directory — and a bridge child never runs from its family's \
                 shared state"
                    .to_string(),
            );
        }
        let Some(source_dir) = agent.state_dir.as_deref().filter(|d| !d.is_empty()) else {
            return Err(
                "this agent declares no `state_dir`, so friring has nothing to seed a bridge \
                 child's private state from"
                    .to_string(),
            );
        };
        let source_dir = PathBuf::from(crate::session::expand_tilde(source_dir, home));
        let credential = agent
            .credential_file
            .as_deref()
            .map(|file| PathBuf::from(crate::session::expand_tilde(file, home)));

        let mut steps = Vec::new();
        for seed in &agent.child_state_seed {
            let step = Self::plan_one(seed, authorized, &source_dir, credential.as_deref(), agent)?;
            let Some(step) = step else { continue };
            if !exists(&step.source) {
                if seed.required {
                    return Err(format!(
                        "'{}' is a required child state seed and is not present at '{}'",
                        seed.src,
                        step.source.display()
                    ));
                }
                continue;
            }
            steps.push(step);
        }
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            source_dir,
            steps,
        })
    }

    /// Resolve one declared seed against the profile's authorization.
    ///
    /// `Ok(None)` is "the agent asked and the profile did not authorize, and the
    /// agent said it could live without it" — the only case that is skipped
    /// rather than refused.
    fn plan_one(
        seed: &ChildStateSeed,
        authorized: &[ChildSeedAllow],
        source_dir: &Path,
        credential: Option<&Path>,
        agent: &AgentSandboxDef,
    ) -> Result<Option<SeedStep>, String> {
        let entry = ChildSeedAllow {
            path: seed.src.clone(),
            mode: seed.mode,
        };
        entry.validate()?;

        let matched = authorized.iter().find(|allowed| allowed.path == seed.src);
        let Some(allowed) = matched else {
            if seed.required {
                return Err(format!(
                    "this profile does not authorize the child state seed '{}', which this \
                     agent requires. Add it to the profile's `child_seed_allow`",
                    seed.src
                ));
            }
            return Ok(None);
        };
        // The mode is part of the authorization, not a detail of it: `auth.json`
        // as `copy` and as `link-rw` say different things about where a
        // refreshed token lands, so a mismatch is refused rather than resolved
        // towards either.
        if allowed.mode != seed.mode {
            return Err(format!(
                "the child state seed '{}' is authorized as '{}' and this agent asks for '{}'. \
                 The modes mean different things about where a write lands, so friring will not \
                 pick one",
                seed.src, allowed.mode, seed.mode
            ));
        }
        let source = source_dir.join(&seed.src);
        if seed.mode == SeedMode::LinkRw {
            Self::check_link_rw(seed, &source, credential, agent)?;
        }
        Ok(Some(SeedStep {
            source,
            destination: PathBuf::from(&seed.src),
            mode: seed.mode,
        }))
    }

    /// The three conditions `link-rw` needs, all of them.
    ///
    /// It is the only mode that gives a child write access to a path outside its
    /// own tree, so it is the only one with conditions. Each closes a different
    /// hole: without the **credential_file** check any file under the state
    /// directory could be made writable by a registry edit; without
    /// **writeback** friring would be sharing a file the agent never claimed to
    /// refresh in place; without the **profile's** authorization the operator
    /// would not have decided.
    fn check_link_rw(
        seed: &ChildStateSeed,
        source: &Path,
        credential: Option<&Path>,
        agent: &AgentSandboxDef,
    ) -> Result<(), String> {
        let Some(credential) = credential else {
            return Err(format!(
                "'{}' asks for the 'link-rw' credential mode and this agent declares no \
                 `credential_file`, so friring cannot tell which file that is",
                seed.src
            ));
        };
        if credential != source {
            return Err(format!(
                "'{}' asks for the 'link-rw' credential mode, which is legal only for the \
                 agent's declared `credential_file` ('{}')",
                seed.src,
                credential.display()
            ));
        }
        if !agent.writeback {
            return Err(format!(
                "'{}' asks for the 'link-rw' credential mode, which shares one file between an \
                 owner and its children — legal only when the agent declares `writeback = true`, \
                 its own assertion that it refreshes this credential in place",
                seed.src
            ));
        }
        Ok(())
    }

    /// The read grants a launch must re-add after its subtract set.
    ///
    /// Only the two link modes: a `copy` or a `copy-rewrite` leaves a file of the
    /// child's own inside its private directory, which the subtract set never
    /// touched.
    pub fn grants(&self) -> Vec<SeedGrant> {
        self.steps
            .iter()
            .filter(|step| matches!(step.mode, SeedMode::Symlink | SeedMode::LinkRw))
            .map(|step| SeedGrant {
                target: step.source.display().to_string(),
                mode: step.mode,
            })
            .collect()
    }

    /// Carry the plan out.
    ///
    /// Idempotent: a relaunch of the same child re-seeds over what is there, so
    /// a `copy-rewrite`d hook configuration follows an edited source and a
    /// `symlink` that was replaced is put back. What it will not do is write
    /// through a symlink an agent planted at a destination.
    ///
    /// # Errors
    ///
    /// A source could not be read, a destination could not be written, or a
    /// `copy-rewrite` source is not UTF-8 text within
    /// [`MAX_REWRITE_BYTES`]. Every one refuses the launch: a child seeded
    /// halfway is a child whose agent fails for a reason nobody can attribute.
    pub fn apply(&self) -> Result<(), String> {
        for step in &self.steps {
            let destination = self.resolve_destination(&step.destination)?;
            // Whatever is there goes first, whichever kind of thing it is: a
            // relaunch re-seeds, and `symlink` and `rename` both refuse an
            // existing destination.
            remove_anything_at(&destination);
            match step.mode {
                SeedMode::Symlink => link(&step.source, &destination)?,
                SeedMode::LinkRw => {
                    // Checked here rather than in `build`, where a TOCTOU window
                    // would open between the test and the link. ADR-28's
                    // condition is that write reaches exactly one file, and
                    // `SandboxPolicy::narrow` grants read-write on the source of
                    // every writable seed: a directory there would hand the
                    // child that whole tree, and a symlink would aim the grant
                    // wherever it points.
                    let meta = std::fs::symlink_metadata(&step.source)
                        .map_err(|e| format!("{}: {e}", step.source.display()))?;
                    if !meta.file_type().is_file() {
                        return Err(format!(
                            "'{}' is not a single regular file, and 'link-rw' shares exactly one \
                             credential file (ADR-28)",
                            step.source.display()
                        ));
                    }
                    link(&step.source, &destination)?
                }
                SeedMode::Copy => copy_tree(&step.source, &destination)?,
                SeedMode::CopyRewrite => self.copy_rewrite(&step.source, &destination)?,
            }
        }
        Ok(())
    }

    /// Join one seed's relative destination onto the private state directory,
    /// creating the directories above it one component at a time.
    ///
    /// A single `create_dir_all` on the joined path traverses whatever is
    /// already there, and the child holds read-write on its own state directory.
    /// A nested seed (`hooks/config.json` — `ChildSeedAllow::validate` permits
    /// those) whose `hooks` component the child replaced with a symlink would
    /// then have this host-side reseed remove and rewrite a file outside the
    /// boundary. So every intermediate component is required to be a real
    /// directory before it is walked, and a missing one is created singly.
    fn resolve_destination(&self, relative: &Path) -> Result<PathBuf, String> {
        let mut path = self.state_dir.clone();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            path.push(component);
            if components.peek().is_none() {
                return Ok(path);
            }
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(format!(
                        "'{}' is not a directory, so friring will not seed a child's private \
                         state through it",
                        path.display()
                    ))
                }
                Err(_) => {
                    std::fs::create_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?
                }
            }
        }
        Ok(path)
    }

    /// Copy a text file, repointing every mention of the family's state
    /// directory at the private one.
    ///
    /// This is what keeps a **path-keyed** configuration working from a new
    /// location: a hook file naming `~/.codex/hooks/x.js` would, copied
    /// verbatim, point a worker's hooks back at its family's directory — the one
    /// place the subtract set denies. Both spellings of the source are replaced,
    /// because a configuration may name it as written or as resolved.
    ///
    /// A source that is not UTF-8, or is larger than [`MAX_REWRITE_BYTES`], is a
    /// refusal rather than a plain copy: a binary file carrying the old path
    /// would leave the child's hooks pointing at the family's directory, which
    /// is exactly the failure this mode exists to prevent.
    fn copy_rewrite(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let meta =
            std::fs::symlink_metadata(source).map_err(|e| format!("{}: {e}", source.display()))?;
        if !meta.is_file() {
            return Err(format!(
                "'{}' is not a regular file, and 'copy-rewrite' rewrites text",
                source.display()
            ));
        }
        if meta.len() > MAX_REWRITE_BYTES {
            return Err(format!(
                "'{}' is {} bytes, past the {MAX_REWRITE_BYTES} 'copy-rewrite' will read",
                source.display(),
                meta.len()
            ));
        }
        let text = std::fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
        let text = String::from_utf8(text).map_err(|_| {
            format!(
                "'{}' is not UTF-8 text, so 'copy-rewrite' cannot repoint the paths in it — and \
                 copying it verbatim would leave this child's configuration pointing at its \
                 family's state directory",
                source.display()
            )
        })?;
        let from = self.source_dir.display().to_string();
        let to = self.state_dir.display().to_string();
        let mut rewritten = text.replace(&from, &to);
        // The resolved spelling too: a configuration may name the directory
        // either way, and a rewrite that missed one would silently leave half
        // the paths pointing at the family's tree.
        if let Some(resolved) = std::fs::canonicalize(&self.source_dir)
            .ok()
            .map(|p| p.display().to_string())
            .filter(|resolved| *resolved != from)
        {
            rewritten = rewritten.replace(&resolved, &to);
        }
        std::fs::write(destination, rewritten)
            .map_err(|e| format!("{}: {e}", destination.display()))
    }
}

/// Link `destination` at `source`.
#[cfg(unix)]
fn link(source: &Path, destination: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(source, destination)
        .map_err(|e| format!("{} -> {}: {e}", destination.display(), source.display()))
}

#[cfg(not(unix))]
fn link(source: &Path, destination: &Path) -> Result<(), String> {
    // Windows has no sandbox backend (`docs/SANDBOX.md`), so no bridge child is
    // ever seeded there. Copying keeps the function total without claiming a
    // shared-file guarantee this platform would not give.
    copy_tree(source, destination)
}

/// Copy a file or a directory tree.
fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let meta =
        std::fs::symlink_metadata(source).map_err(|e| format!("{}: {e}", source.display()))?;
    if meta.is_file() {
        std::fs::copy(source, destination)
            .map(|_| ())
            .map_err(|e| format!("{}: {e}", destination.display()))
    } else if meta.is_dir() {
        std::fs::create_dir_all(destination)
            .map_err(|e| format!("{}: {e}", destination.display()))?;
        let entries =
            std::fs::read_dir(source).map_err(|e| format!("{}: {e}", source.display()))?;
        for entry in entries.flatten() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        // A symlink, a FIFO, a socket, a device node. None of them is
        // configuration, and copying one would either follow it out of the state
        // directory or create something an agent could block on.
        Err(format!(
            "'{}' is neither a file nor a directory, so friring will not seed it",
            source.display()
        ))
    }
}

/// Unlink whatever is at `path`, whichever kind of thing it is.
fn remove_anything_at(path: &Path) {
    if std::fs::remove_file(path).is_ok() {
        return;
    }
    let _ = std::fs::remove_dir_all(path);
}

/// The paths a bridge child is denied after every grant (ADR-31).
///
/// Computed by the **host**, from the sessions that exist at this launch — which
/// is why it is recomputed every time rather than stored: a sibling created after
/// this child was is covered by the wholesale entries below, and a sibling that
/// existed gets its own.
///
/// What is in it:
///
/// 1. The family's `state_dir` and every `state_rw` entry as the *parent* has
///    them — transcripts, history, logs, session state. The exact seed targets
///    are re-granted after this, which is what leaves write reaching the
///    credential file and nothing else.
/// 2. The owner's scratch, signal, bridge and gate directories.
/// 3. friring's own trees wholesale — `<data>/sandbox`, `<data>/signals`,
///    `<data>/gates`, `<data>/worktrees` — minus the child's own. That is what
///    covers every sibling, including one created after this child launched.
///
/// The child's own directories are excluded by being *re-added* by the caller
/// after this set is applied, in [`crate::session::SandboxPolicy::narrow`].
pub fn subtract_set(
    agent: Option<&AgentSandboxDef>,
    home: &str,
    owner_key: &str,
) -> Vec<SubtractPath> {
    let mut out: Vec<SubtractPath> = Vec::new();
    let mut push = |path: String, is_dir: bool| {
        if path.is_empty() {
            return;
        }
        if !out.iter().any(|entry| entry.path == path) {
            out.push(SubtractPath { path, is_dir });
        }
    };

    if let Some(agent) = agent {
        // The family's state, as the parent has it. A directory: the whole tree
        // goes, and the seed targets come back individually.
        for dir in agent.state_dir.iter().chain(agent.state_rw.iter()) {
            push(crate::session::expand_tilde(dir, home), true);
        }
    }
    // The owner's own control directories. A child that could read its owner's
    // bridge queue could answer its owner's requests.
    for dir in [
        crate::sandbox::dirs::session_scratch_dir(owner_key),
        crate::paths::session_signal_dir(owner_key),
        crate::sandbox::dirs::gate_dir(owner_key),
    ]
    .into_iter()
    .flatten()
    {
        push(dir.display().to_string(), true);
    }
    // friring's own trees wholesale. This is what covers every sibling — the
    // ones that exist now and the ones created after this child launched — and
    // it is why the set does not need to enumerate them.
    for dir in [
        crate::sandbox::dirs::sandbox_root(),
        crate::paths::signals_directory(),
        crate::sandbox::dirs::gate_root(),
        crate::paths::worktrees_directory(),
    ]
    .into_iter()
    .flatten()
    {
        push(dir.display().to_string(), true);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AgentSandboxDef {
        AgentSandboxDef {
            config_dir_env: Some("CODEX_HOME".to_string()),
            state_dir: Some("~/.codex".to_string()),
            credential_file: Some("~/.codex/auth.json".to_string()),
            writeback: true,
            state_rw: vec!["~/.codex/sessions".to_string()],
            child_state_seed: vec![
                ChildStateSeed {
                    src: "auth.json".to_string(),
                    mode: SeedMode::LinkRw,
                    required: true,
                },
                ChildStateSeed {
                    src: "config.toml".to_string(),
                    mode: SeedMode::Copy,
                    required: true,
                },
                ChildStateSeed {
                    src: "skills".to_string(),
                    mode: SeedMode::Symlink,
                    required: false,
                },
            ],
            ..AgentSandboxDef::default()
        }
    }

    fn authorized() -> Vec<ChildSeedAllow> {
        vec![
            ChildSeedAllow {
                path: "auth.json".to_string(),
                mode: SeedMode::LinkRw,
            },
            ChildSeedAllow {
                path: "config.toml".to_string(),
                mode: SeedMode::Copy,
            },
            ChildSeedAllow {
                path: "skills".to_string(),
                mode: SeedMode::Symlink,
            },
        ]
    }

    fn everything(_: &Path) -> bool {
        true
    }

    fn plan(agent: &AgentSandboxDef, authorized: &[ChildSeedAllow]) -> Result<SeedPlan, String> {
        SeedPlan::build(
            agent,
            authorized,
            Path::new("/data/sandbox/tmp/child/state"),
            "/home/u",
            &everything,
        )
    }

    #[test]
    fn a_fully_authorized_declaration_plans_every_seed() {
        let plan = plan(&agent(), &authorized()).expect("the plan builds");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.source_dir, Path::new("/home/u/.codex"));
        // Only the two link modes need a re-grant after the subtract set: a copy
        // leaves a file of the child's own.
        let grants = plan.grants();
        assert_eq!(grants.len(), 2);
        assert!(grants
            .iter()
            .any(|g| g.target == "/home/u/.codex/auth.json" && g.is_writable()));
        assert!(grants
            .iter()
            .any(|g| g.target == "/home/u/.codex/skills" && !g.is_writable()));
    }

    /// An agent friring cannot point at a private directory cannot have a bridge
    /// child at all — and never runs one from the family's shared state.
    #[test]
    fn an_agent_that_cannot_be_relocated_is_refused() {
        for (what, mut agent) in [("no config_dir_env", agent()), ("no state_dir", agent())]
            .into_iter()
            .enumerate()
            .map(|(n, (what, mut agent))| {
                if n == 0 {
                    agent.config_dir_env = None;
                } else {
                    agent.state_dir = None;
                }
                (what, agent)
            })
        {
            let error = plan(&agent, &authorized()).expect_err(what);
            assert!(
                error.contains("config_dir_env") || error.contains("state_dir"),
                "{what}: {error}"
            );
            let _ = &mut agent;
        }
    }

    /// A required seed the profile does not authorize refuses the launch; an
    /// optional one is simply skipped.
    #[test]
    fn an_unauthorized_seed_refuses_only_when_it_is_required() {
        let only_config = vec![ChildSeedAllow {
            path: "config.toml".to_string(),
            mode: SeedMode::Copy,
        }];
        let error = plan(&agent(), &only_config).expect_err("auth.json is required");
        assert!(error.contains("auth.json"), "{error}");
        assert!(error.contains("child_seed_allow"), "{error}");

        // With the required ones authorized, the optional one just does not
        // happen.
        let required_only = vec![
            ChildSeedAllow {
                path: "auth.json".to_string(),
                mode: SeedMode::LinkRw,
            },
            ChildSeedAllow {
                path: "config.toml".to_string(),
                mode: SeedMode::Copy,
            },
        ];
        let plan = plan(&agent(), &required_only).expect("the optional seed is skipped");
        assert_eq!(plan.steps.len(), 2);
    }

    /// The mode is part of the authorization: `copy` and `link-rw` say different
    /// things about where a refreshed token lands, so a mismatch is refused
    /// rather than resolved towards either.
    #[test]
    fn a_seed_authorized_in_another_mode_is_refused() {
        let wrong_mode = vec![
            ChildSeedAllow {
                path: "auth.json".to_string(),
                mode: SeedMode::Copy,
            },
            ChildSeedAllow {
                path: "config.toml".to_string(),
                mode: SeedMode::Copy,
            },
        ];
        let error = plan(&agent(), &wrong_mode).expect_err("the modes differ");
        assert!(error.contains("authorized as 'copy'"), "{error}");
        assert!(error.contains("will not pick one"), "{error}");
    }

    /// `link-rw` is the only mode that writes outside the child's own tree, so
    /// it is the only one with conditions — and all three have to hold.
    #[test]
    fn link_rw_needs_all_three_of_its_conditions() {
        // Not the declared credential file.
        let mut moved = agent();
        moved.credential_file = Some("~/.codex/elsewhere.json".to_string());
        let error = plan(&moved, &authorized()).expect_err("not the credential file");
        assert!(error.contains("credential_file"), "{error}");

        // No `credential_file` at all.
        let mut nameless = agent();
        nameless.credential_file = None;
        let error = plan(&nameless, &authorized()).expect_err("no credential file");
        assert!(error.contains("declares no `credential_file`"), "{error}");

        // The agent never claimed to refresh it in place.
        let mut no_writeback = agent();
        no_writeback.writeback = false;
        let error = plan(&no_writeback, &authorized()).expect_err("no writeback");
        assert!(error.contains("writeback = true"), "{error}");

        // And the profile has to authorize that exact path in that exact mode,
        // which the previous test covers.
    }

    /// A required seed that is not on disk refuses: a child started without the
    /// configuration its agent needs fails in a way nobody can attribute.
    #[test]
    fn a_missing_required_seed_refuses_the_launch() {
        let nothing = |_: &Path| false;
        let error = SeedPlan::build(
            &agent(),
            &authorized(),
            Path::new("/data/sandbox/tmp/child/state"),
            "/home/u",
            &nothing,
        )
        .expect_err("nothing is on disk");
        assert!(error.contains("required child state seed"), "{error}");
    }

    /// A seed path is joined onto a directory friring minted, so it must be a
    /// normalized relative path.
    #[test]
    fn a_seed_path_that_would_escape_is_refused() {
        for hostile in ["/etc/passwd", "../../secrets", "a/../b", ""] {
            let mut agent = agent();
            agent.child_state_seed = vec![ChildStateSeed {
                src: hostile.to_string(),
                mode: SeedMode::Copy,
                required: true,
            }];
            let allowed = vec![ChildSeedAllow {
                path: hostile.to_string(),
                mode: SeedMode::Copy,
            }];
            assert!(plan(&agent, &allowed).is_err(), "{hostile:?}");
        }
    }

    /// The subtract set covers the family's state, the owner's control
    /// directories, and friring's own trees wholesale — which is what covers
    /// every sibling, including one created later.
    #[test]
    fn the_subtract_set_covers_the_family_and_every_sibling() {
        let agent = agent();
        let set = subtract_set(Some(&agent), "/home/u", "owner-key");
        let paths: Vec<&str> = set.iter().map(|entry| entry.path.as_str()).collect();
        assert!(paths.contains(&"/home/u/.codex"), "{paths:?}");
        assert!(paths.contains(&"/home/u/.codex/sessions"), "{paths:?}");
        // friring's own trees, wholesale: a sibling created after this child
        // launched is inside them and needs no entry of its own.
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("/sandbox") || p.ends_with("/signals")),
            "{paths:?}"
        );
        assert!(set.iter().all(|entry| entry.is_dir));
    }

    // ── Carrying a plan out ──────────────────────────────────────────────
    //
    // Everything above is about what friring *decides*. These execute it, which
    // is where the four modes differ and where the two credential rules live.

    /// A real family state directory and a real private one, so `apply` writes
    /// to a filesystem rather than to a mock.
    fn seeded_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let family = tmp.path().join("family");
        let private = tmp.path().join("private");
        std::fs::create_dir_all(&family).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        (tmp, family, private)
    }

    fn plan_over(family: &Path, private: &Path, steps: Vec<SeedStep>) -> SeedPlan {
        SeedPlan {
            state_dir: private.to_path_buf(),
            source_dir: family.to_path_buf(),
            steps,
        }
    }

    fn step(family: &Path, name: &str, mode: SeedMode) -> SeedStep {
        SeedStep {
            source: family.join(name),
            destination: PathBuf::from(name),
            mode,
        }
    }

    /// The three read modes, carried out for real: a link that points at the
    /// family's file, a copy that diverges from it, and a directory copied whole.
    #[test]
    #[cfg(unix)]
    fn apply_carries_out_every_read_mode() {
        let (_tmp, family, private) = seeded_dirs();
        std::fs::write(family.join("skills.md"), "how to").unwrap();
        std::fs::write(family.join("config.toml"), "k = 1").unwrap();
        std::fs::create_dir_all(family.join("prompts")).unwrap();
        std::fs::write(family.join("prompts").join("a.md"), "prompt").unwrap();

        plan_over(
            &family,
            &private,
            vec![
                step(&family, "skills.md", SeedMode::Symlink),
                step(&family, "config.toml", SeedMode::Copy),
                step(&family, "prompts", SeedMode::Copy),
            ],
        )
        .apply()
        .unwrap();

        // A symlink, pointing at the family's file: read-only material is shared
        // rather than duplicated.
        let link = private.join("skills.md");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), family.join("skills.md"));

        // A copy that diverges: what the child writes never reaches the family.
        let copy = private.join("config.toml");
        assert!(!std::fs::symlink_metadata(&copy)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::write(&copy, "k = 2").unwrap();
        assert_eq!(
            std::fs::read_to_string(family.join("config.toml")).unwrap(),
            "k = 1"
        );

        // A directory, copied whole.
        assert_eq!(
            std::fs::read_to_string(private.join("prompts").join("a.md")).unwrap(),
            "prompt"
        );
    }

    /// `copy-rewrite` is how a path-keyed hook configuration keeps working from a
    /// new location: every occurrence of the family's directory is repointed at
    /// the private one, in **both** spellings.
    #[test]
    fn copy_rewrite_repoints_every_spelling_of_the_family_directory() {
        let (_tmp, family, private) = seeded_dirs();
        let hooks = format!(
            "{{\"command\": \"{}/bin/hook\", \"cwd\": \"{}\"}}",
            family.display(),
            family.display()
        );
        std::fs::write(family.join("hooks.json"), &hooks).unwrap();

        plan_over(
            &family,
            &private,
            vec![step(&family, "hooks.json", SeedMode::CopyRewrite)],
        )
        .apply()
        .unwrap();

        let written = std::fs::read_to_string(private.join("hooks.json")).unwrap();
        assert!(
            !written.contains(&family.display().to_string()),
            "the family's directory survived the rewrite: {written}"
        );
        assert_eq!(written.matches(&private.display().to_string()).count(), 2);
    }

    /// A source that is not text is a **refusal**, never a plain copy: a binary
    /// carrying the old path would leave the child's hooks pointing at its
    /// family's directory, which is exactly what the relocation exists to stop.
    #[test]
    fn copy_rewrite_refuses_a_source_it_cannot_rewrite() {
        let (_tmp, family, private) = seeded_dirs();
        std::fs::write(family.join("blob"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let err = plan_over(
            &family,
            &private,
            vec![step(&family, "blob", SeedMode::CopyRewrite)],
        )
        .apply()
        .unwrap_err();
        assert!(err.contains("not UTF-8"), "{err}");
        assert!(!private.join("blob").exists(), "a refused seed left a file");

        // …and one past the size it will read.
        std::fs::write(
            family.join("big"),
            vec![b'x'; (MAX_REWRITE_BYTES + 1) as usize],
        )
        .unwrap();
        let err = plan_over(
            &family,
            &private,
            vec![step(&family, "big", SeedMode::CopyRewrite)],
        )
        .apply()
        .unwrap_err();
        assert!(err.contains("past the"), "{err}");
    }

    /// `link-rw` is the credential mode and the only one that writes outside the
    /// child's own tree, so it shares **one regular file** — a directory would
    /// hand the child that whole tree, and a symlink would aim the grant
    /// wherever it points (ADR-28).
    #[test]
    #[cfg(unix)]
    fn link_rw_shares_one_regular_file_and_refuses_anything_else() {
        let (_tmp, family, private) = seeded_dirs();
        std::fs::write(family.join("auth.json"), "{\"token\":\"t\"}").unwrap();
        plan_over(
            &family,
            &private,
            vec![step(&family, "auth.json", SeedMode::LinkRw)],
        )
        .apply()
        .unwrap();

        // A refresh inside the child updates the one file the owner also uses,
        // which is ADR-28's condition: no second copy of a rotating token.
        std::fs::write(private.join("auth.json"), "{\"token\":\"refreshed\"}").unwrap();
        assert_eq!(
            std::fs::read_to_string(family.join("auth.json")).unwrap(),
            "{\"token\":\"refreshed\"}"
        );

        // A directory: refused rather than shared.
        std::fs::create_dir_all(family.join("tree")).unwrap();
        let err = plan_over(
            &family,
            &private,
            vec![step(&family, "tree", SeedMode::LinkRw)],
        )
        .apply()
        .unwrap_err();
        assert!(err.contains("single regular file"), "{err}");

        // A symlink: refused too, or the read-write grant would follow it.
        std::os::unix::fs::symlink(family.join("auth.json"), family.join("alias")).unwrap();
        let err = plan_over(
            &family,
            &private,
            vec![step(&family, "alias", SeedMode::LinkRw)],
        )
        .apply()
        .unwrap_err();
        assert!(err.contains("single regular file"), "{err}");
    }

    /// A relaunch re-seeds into a directory that is already populated, so
    /// applying twice must land where applying once did.
    #[test]
    #[cfg(unix)]
    fn applying_a_plan_twice_is_the_same_as_applying_it_once() {
        let (_tmp, family, private) = seeded_dirs();
        std::fs::write(family.join("config.toml"), "k = 1").unwrap();
        std::fs::write(family.join("skills.md"), "how to").unwrap();
        let steps = vec![
            step(&family, "config.toml", SeedMode::Copy),
            step(&family, "skills.md", SeedMode::Symlink),
        ];

        let plan = plan_over(&family, &private, steps);
        plan.apply().unwrap();
        // What the child changed is replaced, not merged: the seed is the
        // family's file as it stands now.
        std::fs::write(private.join("config.toml"), "k = 99").unwrap();
        plan.apply().unwrap();

        assert_eq!(
            std::fs::read_to_string(private.join("config.toml")).unwrap(),
            "k = 1"
        );
        assert!(std::fs::symlink_metadata(private.join("skills.md"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// A nested destination's parents are created one component at a time, and a
    /// symlink standing in for one is refused — the child holds read-write on
    /// its own state directory, so a single `create_dir_all` would traverse
    /// whatever it put there.
    #[test]
    #[cfg(unix)]
    fn a_nested_seed_never_traverses_a_symlink_the_child_planted() {
        let (tmp, family, private) = seeded_dirs();
        std::fs::create_dir_all(family.join("hooks")).unwrap();
        std::fs::write(family.join("hooks").join("c.json"), "{}").unwrap();

        // The ordinary case: the parent is created.
        plan_over(
            &family,
            &private,
            vec![SeedStep {
                source: family.join("hooks").join("c.json"),
                destination: PathBuf::from("hooks/c.json"),
                mode: SeedMode::Copy,
            }],
        )
        .apply()
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(private.join("hooks").join("c.json")).unwrap(),
            "{}"
        );

        // …and the hostile one: a symlink where the parent should be.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::remove_dir_all(private.join("hooks")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, private.join("hooks")).unwrap();
        let err = plan_over(
            &family,
            &private,
            vec![SeedStep {
                source: family.join("hooks").join("c.json"),
                destination: PathBuf::from("hooks/c.json"),
                mode: SeedMode::Copy,
            }],
        )
        .apply()
        .unwrap_err();
        assert!(err.contains("hooks"), "{err}");
        assert!(
            !elsewhere.join("c.json").exists(),
            "the seed was written through a symlink"
        );
    }
}
