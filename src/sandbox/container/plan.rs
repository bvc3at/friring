//! What one place is made of, as a pure function of its profile.
//!
//! [`plan_instance`] takes a resolved policy and answers with everything the
//! engine needs — the mounts, the labels, the environment, the limits, the
//! network — and [`create_argv`] renders that into a command line. Neither runs
//! anything, so the whole shape of a container is assertable in a unit test and
//! no test has to start one.
//!
//! Two rules drive the mounts:
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
    /// friring's database, to keep *out* (ADR-29). A launch input rather than
    /// something resolved here: the friring that owns the session is not
    /// necessarily the one on the host where the agent runs.
    pub friring_db: Option<&'a str>,
    pub user: Option<&'a str>,
    pub userns_keep_id: bool,
    /// Whether a path exists on the host the engine runs on. Injected, so the
    /// plan is a pure function of its inputs and no test consults the
    /// developer's own filesystem.
    pub exists: &'a dyn Fn(&str) -> bool,
}

/// Build the plan for one profile's place, or refuse with the reason.
///
/// # Errors
///
/// A path that cannot be mounted at its own path (relative, unspellable in a
/// `--mount` value, absent on the host), a mount that would reach friring's data
/// directory or a tmux socket directory, or two mounts landing on one target.
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
        if (input.exists)(&hooks) {
            mounts.push(Mount::identical(&hooks, false));
        }
    }

    // friring's own two: the egress directory at its own path (the socket inside
    // it is named identically on both sides), and the synthetic home.
    mounts.push(Mount::identical(input.place_dir, true));
    mounts.push(Mount::new(input.home_dir, CONTAINER_HOME, true));

    check_mounts(&mounts, &input, &refuse)?;

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

/// Refuse a mount set that cannot be honoured, or that would carry something
/// across the boundary that never may.
fn check_mounts(
    mounts: &[Mount],
    input: &PlanInput<'_>,
    refuse: &dyn Fn(String) -> SandboxError,
) -> SandboxResult<()> {
    let protected = dirs::protected_data_dirs(input.friring_db);
    let socket_root = dirs::tmux_socket_root().display().to_string();
    let mut targets: Vec<&str> = Vec::new();

    for mount in mounts {
        let source = mount.source.as_str();
        if !source.starts_with('/') {
            return Err(refuse(format!(
                "'{source}' is not an absolute path, and a place mounts every path at exactly \
                 its host path"
            )));
        }
        if let Some(bad) = source
            .chars()
            .chain(mount.target.chars())
            .find(|c| *c == ',' || *c == '"' || c.is_control())
        {
            // The engines parse a `--mount` value as comma-separated key=value
            // and disagree about quoting inside it, so a path carrying one of
            // these characters cannot be named exactly. Refusing beats mounting
            // whatever the parser makes of the fragments.
            return Err(refuse(format!(
                "'{source}' contains {} , which cannot be spelled in a container mount",
                bad.escape_debug()
            )));
        }
        // ADR-29, and it is *not* only about the writable set: the database is
        // never mounted into a sandbox, read-only or otherwise. A read-only bind
        // of the data directory still exposes the automation commands the host
        // executes, and a `-wal` written through any writable route is replayed
        // by the host on next open.
        for dir in &protected {
            if dirs::encloses(source, dir) {
                return Err(refuse(format!(
                    "mounting '{source}' would carry friring's data directory '{dir}' into the \
                     sandbox. The database there holds automation commands the host executes, so \
                     it never enters a boundary, read-only or otherwise (ADR-29) — mount the \
                     directories the agent needs instead of an ancestor of the data directory"
                )));
            }
        }
        if dirs::encloses(source, &socket_root) {
            return Err(refuse(format!(
                "mounting '{source}' would carry the tmux socket directory '{socket_root}' into \
                 the sandbox, and a sandbox that can reach friring's own tmux socket can run \
                 commands in any pane, outside the boundary"
            )));
        }
        if !(input.exists)(source) {
            return Err(refuse(format!(
                "'{source}' does not exist on this host, so it cannot be mounted into the \
                 sandbox. Create it, or take it out of the profile — a place that silently drops \
                 a path is a boundary nobody can reason about"
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

    fn input<'a>(policy: &'a SandboxPolicy, place: &'a str, home: &'a str) -> PlanInput<'a> {
        PlanInput {
            policy,
            image: IMAGE,
            place_dir: place,
            home_dir: home,
            friring_db: Some(DB),
            user: Some("1000:1000"),
            userns_keep_id: false,
            exists: &everything,
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
        plan_input.exists = &missing;
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
