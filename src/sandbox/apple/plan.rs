//! What one Apple container is made of, as a pure function of a plan.
//!
//! The *plan* is shared with the container engines
//! ([`crate::sandbox::container::plan`]): identical absolute paths, the
//! `.git/hooks` protection inside every writable root, friring's own two mounts,
//! the labels, the synthetic home, the spec digest that makes an edited profile
//! ask for a new place. All of that is one rule for every place backend, and a
//! second copy of it would be a second set of escapes. Only the *rendering*
//! differs, because this CLI is not the container engines' CLI — which is what
//! this file is.
//!
//! Two renderings genuinely differ, and both are stated here rather than left to
//! be noticed:
//!
//! - **No `--cap-drop` / `--security-opt` / `--user`.** Those harden a process
//!   sharing the host kernel. Here the guest kernel is not the host's, so a
//!   capability inside a place is a capability over that VM — and the file
//!   sharing is virtiofs, which performs every host-side access as the user
//!   running the VM, so a bind mount stays writable without friring naming a
//!   uid. Passing flags this CLI does not have would fail the create; claiming
//!   they were applied would be worse.
//! - **The network is friring's own, always.** A place is attached to
//!   [`NETWORK`], created once, rather than to the network every other container
//!   on the Mac shares. What that network *grants* is the tool's business and
//!   friring claims nothing about it — which is exactly why [`egress_refusal`]
//!   turns down every network mode but an unrestricted `full`.

use crate::sandbox::container::plan::{InstancePlan, Mount, NetworkSetting, KEEPALIVE};
use crate::session::{NetworkMode, SandboxPolicy};

/// The network friring puts every place on.
///
/// One network for all of friring's places rather than one per profile: each is
/// a virtual interface on the host and macOS does not have unlimited ones, and
/// the boundary friring promises is the place itself (`docs/SANDBOX.md` §The two
/// sandbox shapes). What it buys is real all the same — a place cannot reach the
/// containers of whatever else runs on this Mac, and they cannot reach it — and
/// what it does not buy is stated where it matters: places of two profiles share
/// this network, so it is not a boundary between them.
///
/// It is adopted by **name**, unlike a container: this tool has no owner label
/// for a network, so one somebody else created under this name is used rather
/// than refused. That is a smaller claim than it looks — anyone who can create a
/// network here can already run containers here — and it is the reason nothing
/// about egress is inferred from being on it ([`egress_refusal`]).
pub const NETWORK: &str = "friring-sandbox";

/// The `run` command line for a plan.
///
/// `--detach` because a place outlives the command that made it, and
/// [`KEEPALIVE`] because something has to hold it open until the sessions arrive
/// through `exec`.
///
/// Everything this emits is on the host's process table for as long as the tool
/// runs, so **no credential may ever be added here** — a session's own secrets
/// reach it through the tmux window's environment, over the control connection
/// (`docs/SANDBOX.md` §Failure modes). The plan's `env` is per profile and
/// carries none.
pub fn create_argv(program: &str, plan: &InstancePlan, network: &str) -> Vec<String> {
    let mut argv: Vec<String> = vec![program.to_string(), "run".to_string()];
    let mut push = |tokens: &[&str]| argv.extend(tokens.iter().map(|t| (*t).to_string()));

    push(&["--detach", "--name", &plan.name]);
    push(&["--network", network]);
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
        push(&["--mount", &mount_spec(mount)]);
    }
    push(&[&plan.image]);
    push(KEEPALIVE);
    argv
}

/// One bind mount as a `--mount` value.
///
/// The long spellings (`source=`, `target=`) rather than the engines' `src=` /
/// `dst=` abbreviations. Comma-separated `key=value`, which is why
/// [`crate::sandbox::container::plan::MountCheck`] refuses a path carrying a
/// comma or a quote instead of escaping one: the value is parsed as a list and
/// nothing here can quote its way out of that.
fn mount_spec(mount: &Mount) -> String {
    let mut spec = format!("type=bind,source={},target={}", mount.source, mount.target);
    if !mount.writable {
        spec.push_str(",readonly");
    }
    spec
}

/// Why this policy's network mode cannot be honoured in an Apple container, or
/// `None` when it can.
///
/// This is the one place this backend genuinely differs from the container
/// engines, and getting it wrong would silently disable the firewall — so it
/// refuses rather than approximates.
///
/// **A bind-mounted unix socket does not cross this boundary.** Every filtered
/// mode is enforced by friring's proxy *outside* the boundary, reached over a
/// socket the mount carries across and a relay inside the namespace (ADR-27).
/// That works where the sandbox shares friring's kernel. An Apple container is a
/// virtual machine with a kernel of its own, and an `AF_UNIX` listener lives in
/// the kernel that called `bind(2)`: virtiofs carries the socket *file* across,
/// and `connect(2)` on it inside the guest finds no listener in the guest's own
/// table. So a place here cannot reach the proxy at all, and `allowlist` — or
/// `full` carrying denies, which is enforced by the same proxy — would be a
/// sandbox that believes it is filtered and is not.
///
/// **And `none` is not friring's to claim either.** The tool attaches every
/// container to a network; friring gives its places one of their own
/// ([`NETWORK`]), but what that network reaches is the tool's decision, not
/// friring's, and starting a place on it under a profile that says `none` would
/// hand the agent whatever egress it happens to have. Refusing keeps the
/// profile's word: `none` means no route off the machine, and friring will not
/// pretend to have cut one it did not.
///
/// What is left is an unrestricted `full`, which claims nothing about egress —
/// and still gets the VM boundary and only the paths the profile mounted.
pub fn egress_refusal(policy: &SandboxPolicy) -> Option<String> {
    let alternatives = "run this profile on seatbelt (which shares the host's network stack and \
                        can reach the proxy), or on docker/podman";
    match policy.network {
        NetworkMode::Full if policy.deny.is_empty() => None,
        NetworkMode::None => Some(format!(
            "network mode 'none' means no route off the machine, and friring has no way to give \
             an Apple container one it can verify: the tool attaches every container to a \
             network, and friring's own ('{NETWORK}') separates its places from the Mac's other \
             containers rather than from the internet. Starting one anyway would give the agent \
             whatever egress that network has, under a profile that says none — so set this \
             profile's network to 'full' here, or {alternatives}"
        )),
        mode => Some(format!(
            "network mode '{mode}' is enforced by friring's egress proxy outside the boundary, \
             which a place reaches over a bind-mounted unix socket — and that does not cross this \
             one. An Apple container is a virtual machine with its own kernel, so the socket file \
             virtiofs carries across has no listener on the far side of it and `connect` inside \
             the place fails. friring will not start a place that believes it is filtered: set \
             this profile's network to 'full' here, or {alternatives}"
        )),
    }
}

/// Whether a plan's network setting is one this backend can carry out.
///
/// [`egress_refusal`] decides from the policy, which is the authority; this is
/// the same question asked of the plan, so a plan built from a policy nobody
/// checked cannot be rendered into a `run` that quietly means something else.
pub fn network_is_open(plan: &InstancePlan) -> bool {
    matches!(plan.network, NetworkSetting::Default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::container::plan::{plan_instance, MountCheck, PlanInput, CONTAINER_HOME};
    use crate::sandbox::dirs;
    use crate::session::{SandboxBackendKind, SandboxPath, SandboxProfile};

    const IMAGE: &str = "friring/sandbox:1";

    fn resolved(mutate: impl FnOnce(&mut SandboxProfile)) -> SandboxPolicy {
        let mut profile = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        );
        profile.network_mode = NetworkMode::Full;
        mutate(&mut profile);
        profile
            .resolve(SandboxBackendKind::AppleContainer, "/Users/u")
            .unwrap()
    }

    fn plan_for(policy: &SandboxPolicy) -> InstancePlan {
        let place = dirs::place_dir("dev").unwrap().display().to_string();
        let home = dirs::place_home_dir("dev").unwrap().display().to_string();
        plan_instance(PlanInput {
            policy,
            image: IMAGE,
            place_dir: &place,
            home_dir: &home,
            // Neither is passed to this CLI (see the module docs), so neither is
            // in the plan a place here is built from.
            user: None,
            userns_keep_id: false,
            check: MountCheck {
                friring_db: None,
                home: Some("/Users/u"),
                exists: &|_| true,
                resolve: &|path: &str| Ok(path.to_string()),
            },
        })
        .unwrap()
    }

    fn has_flag(argv: &[String], flag: &str, value: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn a_place_is_named_networked_and_mounted_at_identical_paths() {
        let plan = plan_for(&resolved(|_| {}));
        let argv = create_argv("/usr/bin/container", &plan, NETWORK);
        assert_eq!(argv[0], "/usr/bin/container");
        assert_eq!(argv[1], "run");
        assert!(argv.contains(&"--detach".to_string()));
        assert!(has_flag(&argv, "--name", &plan.name));
        // Never the network every other container on this Mac is on.
        assert!(has_flag(&argv, "--network", NETWORK));
        assert!(has_flag(
            &argv,
            "--mount",
            "type=bind,source=/Users/u/dev/app,target=/Users/u/dev/app"
        ));
        assert!(has_flag(
            &argv,
            "--mount",
            "type=bind,source=/srv/shared,target=/srv/shared,readonly"
        ));
        // The synthetic home is the one path that is deliberately not identical.
        let home = dirs::place_home_dir("dev").unwrap().display().to_string();
        assert!(has_flag(
            &argv,
            "--mount",
            &format!("type=bind,source={home},target={CONTAINER_HOME}")
        ));
        // The image, then what holds the place open, and nothing after it.
        let image = argv.iter().position(|a| a == IMAGE).unwrap();
        assert_eq!(&argv[image + 1..], KEEPALIVE);
        // Nothing that hardens a process sharing the host's kernel: this one
        // does not share it, and a flag the CLI has never had would fail the
        // create rather than tighten anything.
        for absent in [
            "--cap-drop",
            "--security-opt",
            "--user",
            "--userns",
            "--init",
        ] {
            assert!(!argv.iter().any(|token| token == absent), "{absent}");
        }
    }

    #[test]
    fn the_limits_a_profile_asked_for_reach_the_command_line() {
        let plan = plan_for(&resolved(|profile| {
            profile.memory_mb = Some(4096);
            profile.cpus = Some(2);
        }));
        let argv = create_argv("/usr/bin/container", &plan, NETWORK);
        assert!(has_flag(&argv, "--memory", "4096m"));
        assert!(has_flag(&argv, "--cpus", "2"));
    }

    /// The whole egress story of this backend, stated as a table.
    #[test]
    fn only_an_unrestricted_full_can_be_honoured_here() {
        assert_eq!(egress_refusal(&resolved(|_| {})), None);
        assert!(network_is_open(&plan_for(&resolved(|_| {}))));

        // Proxied: the socket does not cross a VM boundary, and the refusal
        // says so rather than starting a place that believes it is filtered.
        for mutate in [
            |p: &mut SandboxProfile| p.network_mode = NetworkMode::Allowlist,
            |p: &mut SandboxProfile| p.network_deny = vec!["evil.example".to_string()],
        ] {
            let policy = resolved(mutate);
            let refusal = egress_refusal(&policy).expect("a filtered mode is refused here");
            assert!(refusal.contains("virtual machine"), "{refusal}");
            assert!(refusal.contains("egress proxy"), "{refusal}");
            assert!(refusal.contains("seatbelt"), "{refusal}");
            assert!(!network_is_open(&plan_for(&policy)));
        }

        // And `none` is refused in the other direction: friring cannot prove it
        // cut the route, so it will not claim to have.
        let closed = resolved(|p| p.network_mode = NetworkMode::None);
        let refusal = egress_refusal(&closed).expect("'none' is refused here too");
        assert!(refusal.contains("no route off the machine"), "{refusal}");
        assert!(refusal.contains(NETWORK), "{refusal}");
        assert!(!network_is_open(&plan_for(&closed)));
    }
}
