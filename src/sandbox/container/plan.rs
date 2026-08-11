//! What one place is made of, as a pure function of its profile.
//!
//! [`plan_instance`] takes a resolved policy and answers with everything the
//! engine needs — the mounts, the labels, the environment, the limits, the
//! network — and [`create_argv`] renders that into a command line. Neither runs
//! anything, so the whole shape of a container is assertable in a unit test and
//! no test has to start one.
//!
//! Three rules drive the mounts:
//!
//! - **Identical absolute paths.** Every profile path is mounted at exactly its
//!   host path. A git linked worktree names its main repository by absolute path
//!   and vice versa, and agents key transcripts and per-project trust the same
//!   way, so a path that differs inside silently breaks `git status`, breaks
//!   resume, and re-triggers trust prompts (`docs/SANDBOX.md` §Identical
//!   absolute paths).
//! - **A mount that cannot be honoured is refused, never dropped.** `--mount`
//!   (rather than `-v`) is used precisely because it refuses a missing source
//!   instead of inventing a root-owned directory — and each refusal is checked
//!   here first, so the message names the profile's path and the reason rather
//!   than quoting an engine error.
//! - **A source is judged as the kernel resolves it.** The literal string is
//!   what reaches `--mount`, and the kernel resolves it *again* when it sets the
//!   bind up, so a source travelling through a symlink means one thing to the
//!   check and another to the mount. [`MountCheck`] therefore refuses any source
//!   that does not already spell its own canonical path, and compares the
//!   canonical form against the canonical spelling of every directory a place
//!   may not be handed.

use std::collections::BTreeMap;

use crate::sandbox::backend::{SandboxError, SandboxResult, PROTECTED_IN_WRITABLE_ROOT};
use crate::sandbox::dirs;
use crate::session::{NetworkMode, SandboxPolicy};

/// Where a place puts the agent's home.
///
/// The one deliberate exception to identical absolute paths, and it is not a
/// path anything keys state by: `$HOME` inside a place is friring's own
/// per-profile directory (ADR-28 — never a bind of the host's agent
/// configuration), and the host's home path may not even be creatable inside an
/// image built for a different user.
pub const CONTAINER_HOME: &str = "/home/agent";

/// Marks a container as friring's. Every lookup filters on it and every removal
/// re-checks it, so friring can find its own places and touches nothing else on
/// a machine that runs containers for other reasons.
pub const LABEL_OWNER: &str = "dev.friring.sandbox";

/// Which profile a place was built for.
pub const LABEL_PROFILE: &str = "dev.friring.sandbox.profile";

/// A digest of everything about the place that a profile edit could change.
///
/// Mounts, limits, the image and the network are fixed when a container is
/// created and cannot be altered afterwards, so an edited profile has to mean a
/// *new* container. Comparing this label is how the difference is noticed
/// without re-deriving an engine's own view of a running container.
pub const LABEL_SPEC: &str = "dev.friring.sandbox.spec";

/// Prefix of every container friring creates.
pub const NAME_PREFIX: &str = "friring-sbx-";

/// What a place runs while it waits for sessions.
///
/// A place outlives every command in it (ADR-26), so something has to hold it
/// open; the agents arrive later through `exec`. Paired with `--init`, so pid 1
/// is an init that reaps the agent's orphans and forwards the signal a stop
/// sends — without it a shell as pid 1 ignores `SIGTERM` and every stop waits
/// for the kill.
pub const KEEPALIVE: &[&str] = &["/bin/sh", "-c", "while :; do sleep 86400; done"];

/// One bind mount, at identical paths unless it is the synthetic home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub writable: bool,
}

impl Mount {
    fn new(source: impl Into<String>, target: impl Into<String>, writable: bool) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            writable,
        }
    }

    /// Same path on both sides — the rule for everything but the home.
    fn identical(path: &str, writable: bool) -> Self {
        Self::new(path, path, writable)
    }

    /// The `--mount` value. Comma-separated `key=value`, which is why a path
    /// carrying a comma or a quote is refused rather than escaped: the engines
    /// parse this field as CSV and disagree about quoting.
    fn spec(&self) -> String {
        let mut spec = format!("type=bind,src={},dst={}", self.source, self.target);
        if !self.writable {
            spec.push_str(",readonly");
        }
        spec
    }
}

/// Whether a place has a way out at all, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSetting {
    /// `--network none`: no interface but loopback. Everything except an
    /// unrestricted `full` (ADR-27) — the way out, when there is one, is the
    /// bind-mounted proxy socket and the relay that fronts it.
    None,
    /// The engine's default network: `full` carrying no denies, the one mode
    /// nothing outside the kernel has to enforce.
    Default,
}

/// Everything one place is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePlan {
    pub name: String,
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub mounts: Vec<Mount>,
    pub env: BTreeMap<String, String>,
    pub network: NetworkSetting,
    pub memory_mb: Option<u32>,
    pub cpus: Option<u32>,
    /// `--user uid:gid`, or `None` when the engine's own mapping already gives
    /// the container the host user's identity.
    pub user: Option<String>,
    /// `--userns=keep-id`, for rootless Podman.
    pub userns_keep_id: bool,
    /// The host directory mounted at [`CONTAINER_HOME`] — the synthetic
    /// per-profile home, carried on the plan so a launch can write into the
    /// exact directory this place mounts rather than re-deriving it.
    ///
    /// Not hashed into [`spec`](Self::spec) separately: it is already there as
    /// the source of the `CONTAINER_HOME` mount.
    pub home_dir: String,
    /// The digest behind [`LABEL_SPEC`].
    pub spec: String,
}

/// Everything [`plan_instance`] needs that the policy does not carry.
pub struct PlanInput<'a> {
    /// The frozen profile, already resolved for this engine.
    pub policy: &'a SandboxPolicy,
    /// The image to run — see [`super::image`].
    pub image: &'a str,
    /// The per-profile directory friring keeps this place's egress sockets in,
    /// mounted read-write **at its own path** so one string names a socket on
    /// both sides of the boundary.
    pub place_dir: &'a str,
    /// The per-profile synthetic home, mounted at [`CONTAINER_HOME`].
    pub home_dir: &'a str,
    pub user: Option<&'a str>,
    pub userns_keep_id: bool,
    /// What every mount source is judged against.
    pub check: MountCheck<'a>,
}

/// Everything a mount source is judged against, injected so the whole check is
/// a pure function of its inputs and no test consults the developer's own
/// filesystem.
///
/// Carried on [`PlanInput`] and re-usable on its own, because the check has to
/// happen **twice**: once while the plan is built, and once immediately before
/// the engine is asked to create the container (see [`MountCheck::check`]).
pub struct MountCheck<'a> {
    /// friring's database, to keep *out* (ADR-29). A launch input rather than
    /// something resolved here: the friring that owns the session is not
    /// necessarily the one on the host where the agent runs.
    pub friring_db: Option<&'a str>,
    /// The home directory on the host the engine runs on, for the per-user
    /// engine sockets a desktop or rootless install keeps under it.
    pub home: Option<&'a str>,
    /// Whether a path exists on the host the engine runs on.
    pub exists: &'a dyn Fn(&str) -> bool,
    /// A mount source as the kernel will resolve it, or the reason friring will
    /// not name it — [`dirs::canonical_source`] in production.
    pub resolve: &'a dyn Fn(&str) -> Result<String, String>,
}

/// Build the plan for one profile's place, or refuse with the reason.
///
/// # Errors
///
/// Everything [`MountCheck::check`] refuses: a path that cannot be mounted at
/// its own path (relative, unspellable in a `--mount` value, absent on the host,
/// or reached through a symlink), a mount that would reach friring's data
/// directory, a tmux socket directory or a container engine's control socket, or
/// two mounts landing on one target.
pub fn plan_instance(input: PlanInput<'_>) -> SandboxResult<InstancePlan> {
    let policy = input.policy;
    let refuse = |detail: String| SandboxError::Refused {
        profile: policy.profile.clone(),
        detail,
    };

    let mut mounts: Vec<Mount> = Vec::new();
    for path in &policy.rw_paths {
        mounts.push(Mount::identical(path, true));
    }
    for path in &policy.ro_paths {
        mounts.push(Mount::identical(path, false));
    }
    // Ancestor before descendant, so a read-only path nested in a writable one
    // is applied last and wins — the same ordering rule both policy backends
    // follow, and the order the engines apply mounts in.
    mounts.sort_by(|a, b| a.target.cmp(&b.target));

    // `.git/hooks` stays read-only inside every writable root: hook scripts are
    // run by whichever git touches the repository next, *including the host's*,
    // outside the boundary. Only where it already exists — an engine asked to
    // bind a missing source either fails the create or invents a root-owned
    // directory inside somebody's repository.
    for root in &policy.rw_paths {
        let hooks = format!("{root}/{PROTECTED_IN_WRITABLE_ROOT}");
        if (input.check.exists)(&hooks) {
            mounts.push(Mount::identical(&hooks, false));
        }
    }

    // friring's own two: the egress directory at its own path (the socket inside
    // it is named identically on both sides), and the synthetic home. Checked
    // like every other source rather than trusted: the place directory is
    // mounted read-write, so an agent inside can replace the `home` directory
    // under it with a symlink and have the next ensure mount whatever it names.
    mounts.push(Mount::identical(input.place_dir, true));
    mounts.push(Mount::new(input.home_dir, CONTAINER_HOME, true));

    input.check.check(&mounts, &refuse)?;

    let env = place_env(policy);
    let network = network_setting(policy);
    let mut plan = InstancePlan {
        name: String::new(),
        image: input.image.to_string(),
        labels: BTreeMap::new(),
        mounts,
        env,
        network,
        memory_mb: policy.memory_mb,
        cpus: policy.cpus,
        user: input.user.map(str::to_string),
        userns_keep_id: input.userns_keep_id,
        home_dir: input.home_dir.to_string(),
        spec: String::new(),
    };
    plan.spec = spec_digest(&plan);
    plan.name = container_name(&policy.profile, &plan.spec);
    plan.labels = BTreeMap::from([
        (LABEL_OWNER.to_string(), "1".to_string()),
        (LABEL_PROFILE.to_string(), policy.profile.clone()),
        (LABEL_SPEC.to_string(), plan.spec.clone()),
    ]);
    Ok(plan)
}

impl MountCheck<'_> {
    /// Refuse a mount set that cannot be honoured, or that would carry something
    /// across the boundary that never may.
    ///
    /// Called twice per place: once from [`plan_instance`], and once by the
    /// backend immediately before it runs the engine's `create`. The second call
    /// is what narrows the check-to-create race — the mounts of a plan are
    /// decided long before the container is made (an image may be pulled or
    /// built in between, which takes minutes), and every source in it is a path
    /// a sandboxed agent of some *other* place may be writing the whole time.
    ///
    /// What is left after that is one window nothing outside the engine can
    /// close: between friring's last `lstat`/`realpath` of a source and the
    /// engine's own resolution of the same string as it sets the bind up. The
    /// engine — or, for a rootful Docker, its daemon — is what performs that
    /// resolution, so no check on this side can be the last word; the window is
    /// the `fork`/`exec` of the CLI plus the daemon's own handling of the
    /// request. Two things make it hard to land in: the mount sources are
    /// re-resolved with nothing between them and the spawn, and any source that
    /// is a symlink at *either* check is refused outright rather than followed.
    ///
    /// # Errors
    ///
    /// A source that is relative, unspellable in a `--mount` value, absent, or
    /// reached through a symlink; one that reaches friring's data directory
    /// (ADR-29), a tmux socket directory or a container engine's control socket;
    /// or two mounts landing on one target.
    pub fn check(
        &self,
        mounts: &[Mount],
        refuse: &dyn Fn(String) -> SandboxError,
    ) -> SandboxResult<()> {
        let protected = dirs::protected_data_dirs(self.friring_db);
        let mut targets: Vec<&str> = Vec::new();

        for mount in mounts {
            let source = mount.source.as_str();
            if !source.starts_with('/') {
                return Err(refuse(format!(
                    "'{source}' is not an absolute path, and a place mounts every path at \
                     exactly its host path"
                )));
            }
            if let Some(bad) = source
                .chars()
                .chain(mount.target.chars())
                .find(|c| *c == ',' || *c == '"' || c.is_control())
            {
                // The engines parse a `--mount` value as comma-separated
                // key=value and disagree about quoting inside it, so a path
                // carrying one of these characters cannot be named exactly.
                // Refusing beats mounting whatever the parser makes of the
                // fragments.
                return Err(refuse(format!(
                    "'{source}' contains {} , which cannot be spelled in a container mount",
                    bad.escape_debug()
                )));
            }
            if !(self.exists)(source) {
                return Err(refuse(format!(
                    "'{source}' does not exist on this host, so it cannot be mounted into the \
                     sandbox. Create it, or take it out of the profile — a place that silently \
                     drops a path is a boundary nobody can reason about"
                )));
            }
            // Everything below compares the *canonical* source, because that is
            // what the kernel will bind. A source that is not already its own
            // canonical path is refused rather than rewritten: rewriting it
            // would mount a directory the profile does not name, and following
            // it would mount whatever the link points at by the time the engine
            // gets there.
            let canonical = (self.resolve)(source).map_err(|reason| {
                // Tampering rather than an ordinary refusal: the commonest way
                // to land here is an agent inside a place planting a link at a
                // path the plan adds by itself, and a profile's
                // `allow_unsandboxed_fallback` must not turn that into a launch
                // on the host. A user's own symlinked path is refused the same
                // way — friring cannot tell them apart, and the message says
                // what to change.
                refuse(format!(
                    "{reason}, and a place mounts every path at exactly its host path — friring \
                     will not bind a source whose meaning a symlink can change between the check \
                     and the mount. Name the resolved path in the profile, or take the link out"
                ))
                .tampered()
            })?;
            // ADR-29, and it is *not* only about the writable set: the database
            // is never mounted into a sandbox, read-only or otherwise. A
            // read-only bind of the data directory still exposes the automation
            // commands the host executes, and a `-wal` written through any
            // writable route is replayed by the host on next open.
            for dir in &protected {
                if dirs::reaches(&canonical, dir) {
                    return Err(refuse(format!(
                        "mounting '{source}' would carry friring's data directory '{dir}' into \
                         the sandbox. The database there holds automation commands the host \
                         executes, so it never enters a boundary, read-only or otherwise \
                         (ADR-29) — mount the directories the agent needs instead of an ancestor \
                         of the data directory"
                    )));
                }
            }
            if let Some(socket_root) = dirs::grants_tmux_socket_tree(&canonical) {
                // Read-only is no defence: a read-only superblock does not take
                // write permission away from a socket inode, so `connect(2)`
                // still succeeds and the agent drives the host's tmux server.
                return Err(refuse(format!(
                    "mounting '{source}' would carry the tmux socket directory '{socket_root}' \
                     into the sandbox, and a sandbox that can reach friring's own tmux socket \
                     can run commands in any pane, outside the boundary"
                )));
            }
            if let Some(socket) = dirs::grants_engine_socket(&canonical, self.home) {
                return Err(refuse(format!(
                    "mounting '{source}' would carry the container engine's control socket \
                     '{socket}' into the sandbox. Anything that can speak to it can start a \
                     privileged container with the host's filesystem in it, which is the whole \
                     host — no profile may grant that, so it is refused rather than offered as \
                     an exception"
                )));
            }
            if targets.contains(&mount.target.as_str()) {
                return Err(refuse(format!(
                    "two paths would be mounted at '{}' inside the sandbox, and only one of them \
                     could win",
                    mount.target
                )));
            }
            targets.push(mount.target.as_str());
        }
        Ok(())
    }
}

/// The environment every session in a place inherits.
///
/// Per *profile*, so nothing session-specific belongs here: an `exec` carries
/// what one session needs (its proxy URLs, its identity), and this carries what
/// the place is.
///
/// **Never a credential.** Everything here reaches the engine through
/// [`create_argv`], which is a command line on the host's process table; a
/// token, a proxy bearer or anything else secret travels over the control-mode
/// `new-window` that opens the session's own window inside the place, and
/// nowhere else (`docs/SANDBOX.md` §Failure modes).
///
/// `safe.directory` and a committer identity are both required rather than
/// polite. A bind mount surfaces the host's ownership, which git refuses to act
/// on inside a container ("dubious ownership"), and a commit made with no
/// identity either fails or is attributed to whatever the image happens to
/// declare. Both go in as environment rather than a written config file, so
/// nothing has to be materialised inside the image and every `exec` inherits
/// them: `GIT_CONFIG_COUNT` is git's own way to state configuration without a
/// file.
fn place_env(policy: &SandboxPolicy) -> BTreeMap<String, String> {
    let profile = &policy.profile;
    BTreeMap::from([
        ("HOME".to_string(), CONTAINER_HOME.to_string()),
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), "safe.directory".to_string()),
        ("GIT_CONFIG_VALUE_0".to_string(), "*".to_string()),
        (
            "GIT_AUTHOR_NAME".to_string(),
            format!("friring sandbox ({profile})"),
        ),
        (
            "GIT_COMMITTER_NAME".to_string(),
            format!("friring sandbox ({profile})"),
        ),
        // `.invalid` is reserved by RFC 2606 and can never be a real address, so
        // a commit made in here is attributable to the profile and to nobody's
        // mailbox.
        (
            "GIT_AUTHOR_EMAIL".to_string(),
            format!("{profile}@sandbox.friring.invalid"),
        ),
        (
            "GIT_COMMITTER_EMAIL".to_string(),
            format!("{profile}@sandbox.friring.invalid"),
        ),
        ("FRIRING_SANDBOX".to_string(), profile.clone()),
    ])
}

/// `--network none` for everything but an unrestricted `full`.
///
/// The same rule [`crate::sandbox::backend::SandboxLaunch::egress`] applies, one
/// step earlier: a place is created long before a session's proxy is bound, so
/// the decision is made from the policy alone. `allowlist` and a `full` carrying
/// denies both mean "no direct egress" — what makes them different from `none`
/// is the socket a session gets, not the network the place has.
fn network_setting(policy: &SandboxPolicy) -> NetworkSetting {
    match policy.network {
        NetworkMode::Full if policy.deny.is_empty() => NetworkSetting::Default,
        _ => NetworkSetting::None,
    }
}

/// Everything a rebuild would have to change, in one string.
///
/// Deliberately excludes the name (which is derived *from* this) and the labels
/// (which carry it). Rendered rather than hashed field by field so a new field
/// that is not added here fails loudly in review rather than silently reusing a
/// container built without it.
fn spec_digest(plan: &InstancePlan) -> String {
    let mut rendered = format!("image={}\n", plan.image);
    for mount in &plan.mounts {
        rendered.push_str(&format!(
            "mount={}:{}:{}\n",
            mount.source,
            mount.target,
            if mount.writable { "rw" } else { "ro" }
        ));
    }
    for (key, value) in &plan.env {
        rendered.push_str(&format!("env={key}={value}\n"));
    }
    rendered.push_str(&format!("network={:?}\n", plan.network));
    rendered.push_str(&format!("memory={:?}\n", plan.memory_mb));
    rendered.push_str(&format!("cpus={:?}\n", plan.cpus));
    rendered.push_str(&format!("user={:?}\n", plan.user));
    rendered.push_str(&format!("keepid={}\n", plan.userns_keep_id));
    dirs::digest(&rendered)
}

/// `friring-sbx-<profile>-<spec>`, inside what an engine accepts as a name.
///
/// The spec digest is part of the name, not only of a label: a rebuild after a
/// profile edit must not collide with the container it replaces, which stays on
/// disk until garbage collection reclaims it.
fn container_name(profile: &str, spec: &str) -> String {
    let cleaned: String = dirs::sanitize_component(profile).chars().take(24).collect();
    format!("{NAME_PREFIX}{}-{}", cleaned.trim_matches('-'), &spec[..12])
}

/// The `run` command line for a plan.
///
/// `--detach`, because the place outlives the command that made it;
/// `--cap-drop ALL` and `--security-opt no-new-privileges`, because an agent
/// needs no capability the image cannot already give it and nothing inside
/// should be able to gain one through a setuid binary. Neither is configurable:
/// a profile that needed them would be a profile whose blast radius is the
/// host's.
///
/// Everything this emits — every `--env` among it — is visible on the host's
/// process table for as long as the engine runs, so **no credential may ever be
/// added here**. The session's own secrets go into its window's environment over
/// the control connection instead (`docs/SANDBOX.md` §Failure modes).
pub fn create_argv(program: &str, plan: &InstancePlan) -> Vec<String> {
    let mut argv: Vec<String> = vec![program.to_string(), "run".to_string()];
    let mut push = |tokens: &[&str]| argv.extend(tokens.iter().map(|t| (*t).to_string()));

    push(&["--detach", "--init", "--name", &plan.name]);
    push(&["--cap-drop", "ALL", "--security-opt", "no-new-privileges"]);
    match plan.network {
        NetworkSetting::None => push(&["--network", "none"]),
        NetworkSetting::Default => {}
    }
    if let Some(user) = &plan.user {
        push(&["--user", user]);
    }
    if plan.userns_keep_id {
        push(&["--userns", "keep-id"]);
    }
    if let Some(memory) = plan.memory_mb {
        push(&["--memory", &format!("{memory}m")]);
    }
    if let Some(cpus) = plan.cpus {
        push(&["--cpus", &cpus.to_string()]);
    }
    for (key, value) in &plan.labels {
        push(&["--label", &format!("{key}={value}")]);
    }
    for (key, value) in &plan.env {
        push(&["--env", &format!("{key}={value}")]);
    }
    for mount in &plan.mounts {
        push(&["--mount", &mount.spec()]);
    }
    push(&[&plan.image]);
    push(KEEPALIVE);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ReadScope, SandboxBackendKind, SandboxPath, SandboxProfile};

    const DB: &str = "/home/u/.local/share/friring/friring.db";
    const HOME: &str = "/home/u";
    const IMAGE: &str = "friring/sandbox:1";

    fn resolved(profile: SandboxProfile) -> SandboxPolicy {
        profile
            .resolve(SandboxBackendKind::Docker, "/home/u")
            .unwrap()
    }

    fn workspace_policy() -> SandboxPolicy {
        resolved(SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        ))
    }

    fn place_dir() -> String {
        dirs::place_dir("dev").unwrap().display().to_string()
    }

    fn home_dir() -> String {
        dirs::place_home_dir("dev").unwrap().display().to_string()
    }

    /// Everything exists — the common case, and the one a mount plan is
    /// interesting in.
    fn everything(_: &str) -> bool {
        true
    }

    /// Every source already spells its own canonical path, which is what the
    /// check demands of a real one. The test about symlinks passes the
    /// production resolver and a filesystem it planted itself.
    fn itself(path: &str) -> Result<String, String> {
        Ok(path.to_string())
    }

    fn input<'a>(policy: &'a SandboxPolicy, place: &'a str, home: &'a str) -> PlanInput<'a> {
        PlanInput {
            policy,
            image: IMAGE,
            place_dir: place,
            home_dir: home,
            user: Some("1000:1000"),
            userns_keep_id: false,
            check: MountCheck {
                friring_db: Some(DB),
                home: Some(HOME),
                exists: &everything,
                resolve: &itself,
            },
        }
    }

    fn plan_for(policy: &SandboxPolicy) -> SandboxResult<InstancePlan> {
        let place = place_dir();
        let home = home_dir();
        plan_instance(input(policy, &place, &home))
    }

    /// Whether `argv` contains `flag value` in sequence.
    fn has_flag(argv: &[String], flag: &str, value: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn every_profile_path_is_mounted_at_its_own_path() {
        let policy = workspace_policy();
        let plan = plan_for(&policy).unwrap();
        let workspace = plan
            .mounts
            .iter()
            .find(|m| m.source == "/home/u/dev/app")
            .expect("the workspace is mounted");
        assert_eq!(workspace.target, workspace.source);
        assert!(workspace.writable);
        let shared = plan
            .mounts
            .iter()
            .find(|m| m.source == "/srv/shared")
            .expect("the read-only path is mounted");
        assert_eq!(shared.target, shared.source);
        assert!(!shared.writable);

        // The synthetic home is the one path that is *not* identical, and it is
        // friring's own directory rather than the host's home.
        let home = plan
            .mounts
            .iter()
            .find(|m| m.target == CONTAINER_HOME)
            .expect("the place has a home");
        assert_eq!(home.source, home_dir());
        assert!(home.writable);
    }

    #[test]
    fn a_nested_read_only_path_is_mounted_after_the_root_it_narrows() {
        let policy = resolved(SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("/repo"),
                SandboxPath::read_only("/repo/vendor"),
            ],
        ));
        let plan = plan_for(&policy).unwrap();
        let index = |path: &str| {
            plan.mounts
                .iter()
                .position(|m| m.source == path)
                .unwrap_or_else(|| panic!("{path} missing from {:?}", plan.mounts))
        };
        assert!(index("/repo") < index("/repo/vendor"));
        // And `.git/hooks` stays read-only inside the writable root.
        let hooks = plan
            .mounts
            .iter()
            .find(|m| m.source == "/repo/.git/hooks")
            .expect("hooks are protected");
        assert!(!hooks.writable);
    }

    /// ADR-29 is absolute, and a container makes the read-only half matter: a
    /// read-only bind of the data directory would still carry the automation
    /// commands the *host* executes.
    #[test]
    fn no_mount_may_reach_the_data_directory() {
        let data = dirs::data_dir().unwrap().display().to_string();
        for (paths, needle) in [
            (vec![SandboxPath::workspace("~")], "/home/u"),
            (vec![SandboxPath::read_only("~")], "/home/u"),
            (
                vec![SandboxPath::read_only("/home/u/.local/share/friring")],
                "/home/u/.local/share/friring",
            ),
            (vec![SandboxPath::read_only(&data)], data.as_str()),
            (vec![SandboxPath::workspace("/")], "/"),
        ] {
            let policy = resolved(SandboxProfile::new("dev", paths));
            let err = plan_for(&policy).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(needle), "{needle}: {text}");
            assert!(
                text.contains("ADR-29") || text.contains("tmux socket"),
                "{text}"
            );
        }

        // And the other half of the claim: no shape a profile *can* take
        // produces a mount that reaches the database, its sidecars, or the
        // directory friring keeps them in — including the two mounts friring
        // adds itself, which live under the data directory by design.
        let mut profile = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        );
        let protected = [
            DB.to_string(),
            format!("{DB}-wal"),
            format!("{DB}-shm"),
            data.clone(),
            "/home/u/.local/share/friring".to_string(),
        ];
        for scope in ReadScope::ALL {
            for network in NetworkMode::ALL {
                for limits in [None, Some(2048)] {
                    profile.read_scope = *scope;
                    profile.network_mode = *network;
                    profile.memory_mb = limits;
                    let policy = resolved(profile.clone());
                    let plan = plan_for(&policy).unwrap();
                    for mount in &plan.mounts {
                        for path in &protected {
                            assert!(
                                !dirs::encloses(&mount.source, path),
                                "{scope}/{network} mounted '{}', which reaches '{path}'",
                                mount.source
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_mount_that_cannot_be_honoured_is_refused_rather_than_dropped() {
        let policy = workspace_policy();
        let place = place_dir();
        let home = home_dir();

        // Absent on the host: `--mount` would fail, and `-v` would invent a
        // root-owned directory. Neither is a boundary anyone can reason about.
        let missing = |path: &str| path != "/home/u/dev/app";
        let mut plan_input = input(&policy, &place, &home);
        plan_input.check.exists = &missing;
        let err = plan_instance(plan_input).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");

        // A path that cannot be spelled in a `--mount` value.
        let comma = resolved(SandboxProfile::new(
            "dev",
            vec![SandboxPath::workspace("/srv/a,b")],
        ));
        let err = plan_for(&comma).unwrap_err();
        assert!(err.to_string().contains("cannot be spelled"), "{err}");

        // Two paths landing on one target inside.
        let clash = resolved(SandboxProfile::new(
            "dev",
            vec![SandboxPath::workspace(CONTAINER_HOME)],
        ));
        let err = plan_for(&clash).unwrap_err();
        assert!(err.to_string().contains("only one of them"), "{err}");
    }

    /// The tmux socket directory is refused from **below** as well as from
    /// above: a profile naming one server's `tmux-<uid>` directory does not
    /// enclose the socket root, and mounting it read-only is still a socket the
    /// agent can `connect(2)` — a read-only superblock does not take write
    /// permission away from a socket inode.
    #[test]
    fn no_mount_may_reach_a_tmux_socket_directory_from_either_side() {
        let socket_root = dirs::tmux_socket_root().display().to_string();
        for path in [
            socket_root.clone(),
            format!("{socket_root}/tmux-1000"),
            format!("{socket_root}/tmux-1000/default"),
        ] {
            let policy = resolved(SandboxProfile::new(
                "dev",
                vec![SandboxPath::read_only(&path)],
            ));
            let err = plan_for(&policy).unwrap_err();
            assert!(err.to_string().contains("tmux socket"), "{path}: {err}");
        }
        // Something else under the same root is ordinary scratch space.
        let ordinary = resolved(SandboxProfile::new(
            "dev",
            vec![SandboxPath::read_only(format!("{socket_root}/build-cache"))],
        ));
        plan_for(&ordinary).unwrap();
    }

    /// A sandbox holding the engine's own control socket owns the host: it can
    /// start a privileged container with the host's filesystem in it. So this is
    /// refused outright rather than treated as a service the profile chose to
    /// share.
    #[test]
    fn no_mount_may_carry_the_engine_s_own_control_socket() {
        for path in [
            "/var/run/docker.sock",
            "/var/run",
            "/run",
            "/run/docker.sock",
            "/run/podman/podman.sock",
            "/run/user",
            "/run/user/1000",
            "/run/user/1000/podman",
            "/run/user/1000/podman/podman.sock",
            "/home/u/.docker",
            "/home/u/.docker/run/docker.sock",
            "/home/u/.local/share/containers/podman/machine/qemu/podman.sock",
        ] {
            for paths in [
                vec![SandboxPath::read_only(path)],
                vec![SandboxPath::workspace(path)],
            ] {
                let policy = resolved(SandboxProfile::new("dev", paths));
                let err = plan_for(&policy).unwrap_err();
                assert!(err.to_string().contains("control socket"), "{path}: {err}");
            }
        }
        // A path that merely lives near one is ordinary.
        let ordinary = resolved(SandboxProfile::new(
            "dev",
            vec![SandboxPath::read_only("/var/lib/docker")],
        ));
        plan_for(&ordinary).unwrap();
    }

    /// Resolving a mount source may never *rewrite* one: a plan mounts each
    /// path at exactly the string the profile wrote, and a git linked worktree
    /// is why.
    ///
    /// A worktree names its main repository by absolute path and the repository
    /// names the worktree back the same way, so a place that mounted either at
    /// its canonical spelling instead would leave `git status` looking for a
    /// path that is not there. Which is exactly the case canonicalisation is
    /// about — so the rule is fail-closed rather than helpful: a source that is
    /// not already its own resolved spelling is *refused*, never silently
    /// swapped, and the check's answer is used only to compare against the
    /// directories no mount may carry.
    #[cfg(unix)]
    #[test]
    fn resolving_a_source_never_rewrites_it_so_a_linked_worktree_still_works() {
        let base = dirs::test_temp_base("worktree-identity");
        // The layout a git linked worktree makes: a main repository and a
        // checkout elsewhere, each referring to the other by absolute path.
        let repo = base.join("repo");
        let tree = base.join("wt/feature");
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(repo.join(".git/worktrees/feature")).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".git"), format!("gitdir: {}\n", repo.display())).unwrap();
        let (place, home) = dirs::create_place_dirs("worktree-identity").unwrap();

        let declared = [repo.display().to_string(), tree.display().to_string()];
        let policy = resolved(SandboxProfile::new(
            "worktree-identity",
            declared
                .iter()
                .map(SandboxPath::workspace)
                .collect::<Vec<_>>(),
        ));
        let real_exists = |path: &str| std::path::Path::new(path).exists();
        let planned = plan_instance(PlanInput {
            policy: &policy,
            image: IMAGE,
            place_dir: &place.display().to_string(),
            home_dir: &home.display().to_string(),
            user: None,
            userns_keep_id: false,
            check: MountCheck {
                friring_db: None,
                home: Some(&base.display().to_string()),
                exists: &real_exists,
                resolve: &dirs::place_mount_source,
            },
        })
        .expect("an ordinary worktree layout plans");

        for path in &declared {
            let mount = planned
                .mounts
                .iter()
                .find(|m| m.source == *path)
                .unwrap_or_else(|| panic!("'{path}' must be mounted under its own name"));
            assert_eq!(
                mount.target, mount.source,
                "identical absolute paths: the profile's spelling on both sides"
            );
        }
        // Nothing was invented either: every source a plan carries is one the
        // profile named or one friring minted, never a resolved variant of one.
        let minted = [place.display().to_string(), home.display().to_string()];
        for mount in &planned.mounts {
            let known = declared.iter().chain(&minted).any(|path| {
                mount.source == *path
                    || mount.source == format!("{path}/{PROTECTED_IN_WRITABLE_ROOT}")
            });
            assert!(known, "unexpected mount source {}", mount.source);
        }
        let _ = std::fs::remove_dir_all(&base);
        dirs::cleanup_place("worktree-identity");
    }

    /// The escape, end to end at the plan level and against a real filesystem.
    ///
    /// `plan_instance` adds `<writable root>/.git/hooks` wherever it exists, so
    /// an agent inside a place can plant that path as a link to friring's tmux
    /// socket directory and have the *next* ensure mount it — no user error
    /// anywhere. friring's own two mounts are no different: the place directory
    /// is mounted read-write, so the synthetic home underneath it is a path the
    /// sandbox can replace with a link of its own.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_never_becomes_a_mount() {
        let base = dirs::test_temp_base("planted-mount");
        let repo = base.join("repo");
        let hooks = repo.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        // friring's own two directories, named exactly as a place mounts them —
        // which on macOS is through the temp root's `/var` link, so this is also
        // the case that must *not* be refused. A tree of this test's own:
        // they are keyed by profile name, and a sibling test cleaning up "dev"
        // would take these away mid-test.
        let (place, home) = dirs::create_place_dirs("planted").unwrap();
        let place_dir = place.display().to_string();
        let home_dir = home.display().to_string();

        let policy = resolved(SandboxProfile::new(
            "planted",
            vec![SandboxPath::workspace(repo.display().to_string())],
        ));
        let real_exists = |path: &str| std::path::Path::new(path).exists();
        let plan = |policy: &SandboxPolicy| {
            plan_instance(PlanInput {
                policy,
                image: IMAGE,
                place_dir: &place_dir,
                home_dir: &home_dir,
                user: None,
                userns_keep_id: false,
                check: MountCheck {
                    friring_db: None,
                    home: Some(&base.display().to_string()),
                    exists: &real_exists,
                    // The real thing, against the filesystem this test planted.
                    resolve: &dirs::place_mount_source,
                },
            })
        };

        // The control: an ordinary repository, and friring's own directories,
        // all plan cleanly.
        let planned = plan(&policy).unwrap();
        assert!(planned
            .mounts
            .iter()
            .any(|m| m.source == hooks.display().to_string() && !m.writable));

        // Act one: the agent replaces `.git/hooks` with a link to the directory
        // friring's own tmux server listens in. A read-only bind of it would be
        // arbitrary command execution on the host.
        std::fs::remove_dir(&hooks).unwrap();
        let sockets = dirs::tmux_socket_root();
        std::os::unix::fs::symlink(&sockets, &hooks).unwrap();
        let err = plan(&policy).unwrap_err().to_string();
        assert!(err.contains(&hooks.display().to_string()), "{err}");
        assert!(err.contains("symlink"), "{err}");
        // Named, not merely refused: the message says what to act on.
        assert!(err.contains(&sockets.display().to_string()), "{err}");

        // Act two: the same trick on friring's own synthetic home, which lives
        // inside the read-write place directory the agent already has.
        // (`remove_file` unlinks the link itself; `remove_dir` on one is
        // `ENOTDIR`.)
        std::fs::remove_file(&hooks).unwrap();
        std::fs::create_dir(&hooks).unwrap();
        plan(&policy).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&sockets, &home).unwrap();
        let err = plan(&policy).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(err.contains(&home_dir), "{err}");

        // Act three: a legitimately symlinked profile path — a repository
        // reached through a link somebody meant to be there. friring cannot
        // tell it from the planted one, so it is refused too, with a message
        // naming the link and where it goes.
        let _ = std::fs::remove_file(&home);
        std::fs::create_dir_all(&home).unwrap();
        let linked = base.join("checkouts");
        std::os::unix::fs::symlink(&repo, &linked).unwrap();
        let via_link = resolved(SandboxProfile::new(
            "planted",
            vec![SandboxPath::workspace(linked.display().to_string())],
        ));
        let err = plan(&via_link).unwrap_err().to_string();
        assert!(err.contains(&linked.display().to_string()), "{err}");
        assert!(err.contains(&repo.display().to_string()), "{err}");
        assert!(err.contains("Name the resolved path"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
        dirs::cleanup_place("planted");
    }

    #[test]
    fn the_command_line_carries_the_limits_the_profile_asked_for() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.memory_mb = Some(4096);
        profile.cpus = Some(2);
        let policy = resolved(profile);
        let plan = plan_for(&policy).unwrap();
        let argv = create_argv("/usr/bin/docker", &plan);

        assert_eq!(argv[0], "/usr/bin/docker");
        assert_eq!(argv[1], "run");
        assert!(has_flag(&argv, "--memory", "4096m"));
        assert!(has_flag(&argv, "--cpus", "2"));
        assert!(has_flag(&argv, "--network", "none"));
        assert!(has_flag(&argv, "--user", "1000:1000"));
        assert!(has_flag(&argv, "--cap-drop", "ALL"));
        assert!(has_flag(&argv, "--security-opt", "no-new-privileges"));
        assert!(argv.contains(&"--init".to_string()));
        assert!(has_flag(&argv, "--name", &plan.name));
        assert!(has_flag(&argv, "--label", &format!("{LABEL_PROFILE}=dev")));
        assert!(has_flag(&argv, "--label", &format!("{LABEL_OWNER}=1")));
        // The image, then what holds the place open, and nothing after it.
        let image = argv.iter().position(|a| a == IMAGE).unwrap();
        assert_eq!(&argv[image + 1..], KEEPALIVE);
    }

    #[test]
    fn git_can_act_on_a_bind_mounted_repository_and_says_who_committed() {
        let policy = workspace_policy();
        let plan = plan_for(&policy).unwrap();
        assert_eq!(plan.env.get("GIT_CONFIG_KEY_0").unwrap(), "safe.directory");
        assert_eq!(plan.env.get("GIT_CONFIG_VALUE_0").unwrap(), "*");
        assert_eq!(plan.env.get("GIT_CONFIG_COUNT").unwrap(), "1");
        assert!(plan.env.get("GIT_AUTHOR_NAME").unwrap().contains("dev"));
        assert!(plan
            .env
            .get("GIT_COMMITTER_EMAIL")
            .unwrap()
            .ends_with("@sandbox.friring.invalid"));
        assert_eq!(plan.env.get("HOME").unwrap(), CONTAINER_HOME);
    }

    /// A place is created before any session's proxy exists, so the network is
    /// decided by the profile alone — and everything but an unrestricted `full`
    /// is cut off at the kernel.
    #[test]
    fn only_an_unrestricted_full_keeps_a_network() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        for (mode, deny, expected) in [
            (NetworkMode::None, vec![], NetworkSetting::None),
            (NetworkMode::Allowlist, vec![], NetworkSetting::None),
            (NetworkMode::Full, vec![], NetworkSetting::Default),
            (
                NetworkMode::Full,
                vec!["evil.example".to_string()],
                NetworkSetting::None,
            ),
        ] {
            profile.network_mode = mode;
            profile.network_deny = deny;
            let policy = resolved(profile.clone());
            assert_eq!(plan_for(&policy).unwrap().network, expected, "{mode}");
        }
    }

    /// A profile edit has to mean a new container: mounts, limits, the image and
    /// the network are fixed when one is created.
    #[test]
    fn an_edited_profile_produces_a_different_spec_and_name() {
        let base = plan_for(&workspace_policy()).unwrap();
        let same = plan_for(&workspace_policy()).unwrap();
        assert_eq!(base.spec, same.spec, "the same profile is the same place");
        assert_eq!(base.name, same.name);
        assert!(base.name.starts_with(NAME_PREFIX));

        let wider = resolved(SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
                SandboxPath::read_only("/srv/extra"),
            ],
        ));
        let edited = plan_for(&wider).unwrap();
        assert_ne!(base.spec, edited.spec);
        assert_ne!(base.name, edited.name);
        assert_eq!(edited.labels.get(LABEL_SPEC).unwrap(), &edited.spec);
    }
}
