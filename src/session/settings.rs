//! User-tunable settings (`~/.config/friring/settings.toml`): scalar knobs
//! plus the `[features]` whole-feature switches.
//!
//! Pure data + parsing, per the `session/` architecture rule; the file IO and
//! seeding live in `crate::agent::settings_config`. Only knobs a user
//! plausibly wants to change are exposed — timing/buffer internals stay
//! hardcoded. The loaded value is published process-wide via [`init`] /
//! [`global`] because the consumers span modules that must not know about each
//! other (terminal wiring, layout, storage retention).

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::keybindings::{KeyChord, PrefixMode};

/// Settings loaded from `settings.toml`. Every field has a default, so
/// an absent file (the common case) behaves exactly like before the file
/// existed. Unknown keys are tolerated but reported: the loader names every
/// unrecognized key in a startup warning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Config-format version, for future migrations. Currently `1`.
    #[serde(default)]
    pub config_version: Option<u32>,
    /// Scrollback lines kept per session terminal (vt100 parser history).
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: usize,
    /// Terminal width (columns) below which only the terminal pane renders.
    #[serde(default = "default_two_panel_min_cols")]
    pub two_panel_min_cols: u16,
    /// Terminal width (columns) at which the optional third column (info /
    /// tasks / file viewer) becomes available.
    #[serde(default = "default_three_panel_min_cols")]
    pub three_panel_min_cols: u16,
    /// Days of audit-log history kept (pruned on startup).
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u64,
    /// Where the info panel (F2) docks — see [`InfoPanelPosition`]. Applies
    /// live (mirrored into `App` state like the UI-panel feature flags).
    #[serde(default)]
    pub info_panel_position: InfoPanelPosition,
    /// Per-feature on/off switches (`[features]` table). Absent table = all
    /// enabled.
    #[serde(default)]
    pub features: FeatureFlags,
    /// Desktop-notification settings (`[notifications]` table). Absent table =
    /// defaults (fire on `Attention`, skip the currently-focused session, 5s
    /// per-session dedup).
    #[serde(default)]
    pub notifications: NotificationSettings,
    /// Native code-review knobs (`[review]` table). Absent table = defaults
    /// (structured handoff, idle nudge on).
    #[serde(default)]
    pub review: ReviewSettings,
    /// Leader-key settings (`[prefix]` table). Absent table = `Ctrl+F` leader
    /// alongside the direct chords, `F12` as the second leader.
    #[serde(default)]
    pub prefix: PrefixSettings,
}

/// Whole-feature switches (`[features]` in settings.toml). Each flag hides the
/// feature's UI and blocks its keybinding; disabling `automations` also stops
/// the TUI firing schedules and arming the tmux heartbeat. Data and
/// `friring-cli` surfaces stay fully functional regardless, so re-enabling a
/// flag is lossless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureFlags {
    /// Tasks panel (F5/Ctrl+W) and task search results.
    #[serde(default = "default_true")]
    pub tasks: bool,
    /// Automations pane, Ctrl+P editor, TUI schedule firing, heartbeat arming.
    #[serde(default = "default_true")]
    pub automations: bool,
    /// File viewer column (F3) and file search results.
    #[serde(default = "default_true")]
    pub file_viewer: bool,
    /// Global search popup (Ctrl+/ or double-Shift).
    #[serde(default = "default_true")]
    pub global_search: bool,
    /// The double-`Shift` opener for the global search. Only effective on
    /// kitty-keyboard-protocol terminals (legacy terminals never report bare
    /// modifier presses); `Ctrl+/` works regardless. Off = only the chord.
    #[serde(default = "default_true")]
    pub double_shift_search: bool,
    /// Info panel column (F2).
    #[serde(default = "default_true")]
    pub info_panel: bool,
    /// Per-session shell pane toggle (Ctrl+T).
    #[serde(default = "default_true")]
    pub shell_pane: bool,
    /// Native code-review view (tuicr-like): the diff/comment view + its
    /// keybinding.
    #[serde(default = "default_true")]
    pub code_review: bool,
    /// Agent activity view (F9): the per-session retrospective (commands /
    /// edits / reads / web / subagents across supported agent CLIs) + its
    /// keybinding, the off-thread scans behind it, and conversation import.
    /// Local sessions only. The key stays `cc_activity` for config
    /// compatibility with the view's Claude-only v1.
    #[serde(default = "default_true")]
    pub cc_activity: bool,
    /// Perf HUD overlay (`<leader> m`): live perf counters + frame/tick timing. Opening
    /// it also turns on wall-clock timing collection (see docs/PERFORMANCE.md).
    #[serde(default = "default_true")]
    pub perf_hud: bool,
    /// Mouse support: terminal mouse capture plus all click/scroll/hover
    /// handling (click-to-select, drag selection, Ctrl+Click URLs,
    /// scrollbars). Disable to keep the terminal's native mouse behavior
    /// (e.g. its own text selection).
    #[serde(default = "default_true")]
    pub mouse: bool,
    /// OS desktop notifications when a session needs the user's attention.
    /// Disabled = no notifications fire and the dispatcher thread never
    /// starts (zero overhead). Linux gets click-to-focus; macOS shows a
    /// passive banner only (the modern API requires a signed app bundle).
    #[serde(default = "default_true")]
    pub notifications: bool,
    /// Soft-delete sessions in the TUI (Ctrl+D): mark the DB row deleted and
    /// offer Ctrl+Z undo, leaving the tmux window + worktrees intact. Disabled
    /// = the TUI **hard-deletes** (kills the tmux window, removes worktrees +
    /// symlink workspace, disables send automations) after a confirmation
    /// prompt. `friring-cli session delete` is unaffected (always soft unless
    /// `--force`).
    #[serde(default = "default_true")]
    pub soft_delete: bool,
    /// Version-update check: the TUI header "update available" badge and the
    /// `friring-cli version --check` command. **Off by default** — unlike the
    /// other flags, this one is opt-in because it makes a network call to
    /// GitHub. Enable it to learn when a newer release is available.
    #[serde(default = "default_false")]
    pub version_check: bool,
    /// Silent auto-update: the TUI silently downloads, verifies, and replaces
    /// the installed binaries on startup when a newer release exists, and the
    /// `friring-cli update` command does the same on demand. Also keeps installed
    /// extensions fresh — once the binary upgrades, the self-heal pass (TUI
    /// startup + headless tick) refreshes any extension that is now stale instead
    /// of merely nudging. **Off by default** — opt-in because it makes a network
    /// call and replaces files on disk. The new version applies on the next launch.
    #[serde(default = "default_false")]
    pub auto_update: bool,
}

/// Where the info panel (F2) docks (`info_panel_position`).
///
/// `Auto` (the default) inlines it at the bottom of the left/session column
/// whenever the full session list, the automations pane, and the full info
/// content all fit; otherwise it falls back to the dedicated column (which
/// needs `three_panel_min_cols`). `Column` always uses the dedicated column
/// (the classic layout). `Inline` always docks it in the left column, even
/// when that squeezes the session list down to its minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InfoPanelPosition {
    /// Inline under the session list when everything fits, else the column.
    #[default]
    Auto,
    /// Always the dedicated column (the classic layout).
    Column,
    /// Always under the session list, squeezing it if needed.
    Inline,
}

impl InfoPanelPosition {
    /// All variants in `←`/`→` cycle order (settings panel stepper).
    pub const ALL: [InfoPanelPosition; 3] = [
        InfoPanelPosition::Auto,
        InfoPanelPosition::Column,
        InfoPanelPosition::Inline,
    ];

    /// The `settings.toml` value string (mirrors serde's lowercase rename).
    pub fn as_str(self) -> &'static str {
        match self {
            InfoPanelPosition::Auto => "auto",
            InfoPanelPosition::Column => "column",
            InfoPanelPosition::Inline => "inline",
        }
    }
}

/// Which OS-notification delivery backend to use (`[notifications] backend`).
/// `Auto` (the default) detects the right one at startup: dbus on a normal
/// Linux desktop, a Windows toast (via `powershell.exe`) under WSL when no
/// dbus notification daemon is reachable, the native banner on macOS. The
/// other variants force a specific path, and `Off` disables delivery entirely
/// without touching the `[features] notifications` switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NotificationBackend {
    /// Detect the best backend for the host at startup.
    #[default]
    Auto,
    /// Force the freedesktop dbus path (`org.freedesktop.Notifications`).
    Dbus,
    /// Force the WSL → Windows toast path (`powershell.exe`).
    Windows,
    /// Disable delivery (the dispatcher still starts but drops every
    /// notification — a soft off-switch distinct from `[features]`).
    Off,
}

/// Knobs for the OS notification feature (`[notifications]` table). All
/// fields have defaults so an empty / absent table behaves like the seeded
/// configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSettings {
    /// Also notify when a session **finishes** (`Working → Done`, reported by an
    /// agent hook), not just when it becomes `Blocked` (which always fires).
    /// Off by default. (The field name is historical — it now governs the Done
    /// edge.)
    #[serde(default)]
    pub also_on_waiting: bool,
    /// Skip notifications for the session currently in focus (you're already
    /// looking at it). Defaults on; flip off if you run friring in a
    /// background window and want every transition surfaced.
    #[serde(default = "default_true")]
    pub suppress_for_active: bool,
    /// Play the OS default notification sound.
    #[serde(default = "default_true")]
    pub sound: bool,
    /// Per-session floor between two notifications, in seconds. Prevents an
    /// agent that flips Attention → Busy → Attention from spamming.
    #[serde(default = "default_notification_min_interval_secs")]
    pub min_interval_secs: u64,
    /// Delivery backend. `auto` (default) detects dbus vs. Windows-toast vs.
    /// macOS at startup; `dbus`/`windows` force one; `off` disables delivery.
    #[serde(default)]
    pub backend: NotificationBackend,
}

/// Which compiled-markdown shape the review handoff uses (`[review] handoff`)
/// — both `e` (send to agent) and `y` (copy) go through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReviewHandoff {
    /// v2: an in-band semantics preamble, stable `C<id>` headings, and quoted
    /// anchor lines the agent can grep for.
    #[default]
    Structured,
    /// The original bullet format (`## <path>` / `- **[Class]** (side:line)`),
    /// for users whose agent prompts/workflows depend on it.
    Legacy,
}

/// Knobs for the native code-review view (`[review]` table). All fields have
/// defaults so an absent table behaves like before the table existed. Applied
/// live (mirrored into `App` state on save/reload, like the UI feature flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSettings {
    /// Handoff markdown shape for `e` (send) and `y` (copy).
    #[serde(default)]
    pub handoff: ReviewHandoff,
    /// Toast a re-review nudge ("Agent idle — F7 to re-review…") when a
    /// session's agent goes idle after a review was sent to it.
    #[serde(default = "default_true")]
    pub nudge_on_idle: bool,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            handoff: ReviewHandoff::Structured,
            nudge_on_idle: true,
        }
    }
}

/// Leader-key settings (`[prefix]` table). All fields have defaults so an
/// absent table gives the shipped behaviour: `Ctrl+A` as leader, alongside the
/// direct chords.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixSettings {
    /// Whether the leader is live, and whether direct chords still dispatch.
    #[serde(default)]
    pub mode: PrefixMode,
    /// The leader chord.
    ///
    /// `Ctrl+F` is chosen for one property no other candidate has: **every
    /// program that claims it, claims it for something that already has a
    /// non-`Ctrl` route.** Claude Code leaves it unbound entirely; Codex,
    /// aider and opencode bind it only to cursor-right, co-bound to `→`. That
    /// makes it the cheapest collision available — unlike `Ctrl+R`
    /// (reverse-i-search, no alternative), `Ctrl+C`/`Ctrl+D` (reserved and
    /// unrebindable in both major agents), or `Ctrl+G` (external editor in
    /// Claude Code *and* Codex).
    ///
    /// It also survives the constraints that eliminate the obvious
    /// alternatives: `f` is the left-index home key on QWERTY, QWERTZ, AZERTY
    /// and Nordic alike (`Ctrl+\`, `Ctrl+]`, `Ctrl+^` need AltGr or a dead key
    /// on German/French/Nordic); byte `0x06` is plain C0, so it needs no
    /// kitty-protocol negotiation through terminal → ssh → tmux; and a slip to
    /// `Cmd+F` opens a find bar rather than quitting the terminal, which is
    /// what rules out `Ctrl+Q` next to `Cmd+Q`.
    ///
    /// No multiplexer has ever claimed it, so there is no ambiguity about
    /// which layer answered when friring is nested inside a `C-a`/`C-b` tmux.
    /// The cost is that `Ctrl+F` reads as "find" to most people — hence
    /// `<leader> /` for search — and that vim/less page-forward is shadowed
    /// inside a pane, which `<leader> <leader>` exists to recover.
    #[serde(default = "default_prefix_key")]
    pub key: String,
    /// Second leader, tmux's `prefix2`. `F12` by default because it is
    /// layout-independent (`Ctrl+\`/`Ctrl+]`-style chords need AltGr on
    /// DE/FR/Nordic keyboards) and survives an outer tmux that has claimed the
    /// primary. Set to `""` to disable — which also gives `F12` back to the
    /// perf HUD. Only consulted when [`mode`](Self::mode) is not `off`.
    #[serde(default = "default_prefix_key2")]
    pub key2: String,
    /// Delay in ms before the which-key overlay appears once the leader is
    /// armed. `0` (the default) shows it immediately: the overlay *is* the
    /// feature's discoverability, and a delay only hides it from the users who
    /// need it. Raise it if you know the table by heart and want a quiet
    /// screen while typing a chord quickly.
    #[serde(default)]
    pub hint_delay_ms: u64,
}

fn default_prefix_key() -> String {
    "ctrl+f".into()
}

fn default_prefix_key2() -> String {
    "f12".into()
}

impl Default for PrefixSettings {
    fn default() -> Self {
        Self {
            mode: PrefixMode::default(),
            key: default_prefix_key(),
            key2: default_prefix_key2(),
            hint_delay_ms: 0,
        }
    }
}

/// Leader chords that are a bad idea, and why — reported at config load so a
/// user learns *before* the key silently fails to arrive, rather than by
/// filing a bug. Mirrors what Claude Code does for its own keybindings, which
/// validates against multiplexer conflicts and warns.
///
/// These are warnings, never errors: the user's config wins. Someone who has
/// deliberately unbound their outer tmux prefix is entitled to `Ctrl+B`.
const RISKY_PREFIX_CHORDS: &[(&str, &str)] = &[
    (
        "ctrl+b",
        "tmux's default prefix — an outer tmux consumes it before friring sees it",
    ),
    (
        "ctrl+a",
        "GNU screen's prefix, the most common tmux rebind, and beginning-of-line \
         in every agent CLI",
    ),
    (
        "ctrl+c",
        "SIGINT, and reserved/unrebindable in Claude Code and Codex",
    ),
    (
        "ctrl+d",
        "EOF, and reserved/unrebindable in Claude Code and Codex",
    ),
    ("ctrl+z", "SIGTSTP — the shell suspends friring instead"),
    (
        "ctrl+q",
        "XON flow control, a near-miss for Cmd+Q (quit) on macOS, and it kills \
         the terminal in WSL",
    ),
    (
        "ctrl+s",
        "XOFF flow control — freezes output when a layer of the stack is not in raw mode",
    ),
];

impl PrefixSettings {
    /// Config-load warnings for a leader chord the user is likely to regret.
    /// Empty for the shipped defaults.
    pub fn warnings(&self) -> Vec<String> {
        if !self.mode.prefix_enabled() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (field, raw) in [("key", &self.key), ("key2", &self.key2)] {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some(chord) = KeyChord::parse(trimmed) else {
                out.push(format!(
                    "settings.toml: [prefix] {field} = \"{trimmed}\" is not a valid chord; ignored"
                ));
                continue;
            };
            let canonical = chord.display();
            if let Some((_, why)) = RISKY_PREFIX_CHORDS
                .iter()
                .find(|(risky, _)| *risky == canonical)
            {
                out.push(format!(
                    "settings.toml: [prefix] {field} = \"{canonical}\" — {why}"
                ));
            }
        }
        out
    }

    /// The parsed leader chords, primary first, skipping unset/unparseable
    /// entries. Empty when the mode is `off` — callers can treat "no leader
    /// chords" as "the leader is not live" without also checking the mode.
    pub fn chords(&self) -> Vec<KeyChord> {
        if !self.mode.prefix_enabled() {
            return Vec::new();
        }
        [&self.key, &self.key2]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .filter_map(|s| KeyChord::parse(s))
            .collect()
    }
}

fn default_notification_min_interval_secs() -> u64 {
    5
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            also_on_waiting: false,
            suppress_for_active: true,
            sound: true,
            min_interval_secs: default_notification_min_interval_secs(),
            backend: NotificationBackend::Auto,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self {
            tasks: true,
            automations: true,
            file_viewer: true,
            global_search: true,
            double_shift_search: true,
            info_panel: true,
            shell_pane: true,
            code_review: true,
            cc_activity: true,
            perf_hud: true,
            mouse: true,
            notifications: true,
            soft_delete: true,
            version_check: false,
            auto_update: false,
        }
    }
}

fn default_scrollback_lines() -> usize {
    1000
}
fn default_two_panel_min_cols() -> u16 {
    80
}
fn default_three_panel_min_cols() -> u16 {
    120
}
fn default_audit_retention_days() -> u64 {
    90
}

impl Settings {
    /// Whether any **restart-only** setting differs between `self` and `other`.
    ///
    /// These are the values read once at startup (the scalars, every
    /// `[notifications]` knob, and the feature flags whose effect is wired at
    /// launch — `automations`, `mouse`, `notifications`, `version_check`). The
    /// remaining feature flags gate UI panels read from `App.features` every
    /// frame, so they apply live and are intentionally excluded here — as is
    /// `info_panel_position`, which is mirrored into `App` state the same way.
    /// Drives the "some changes apply after restart" hint shown by the settings
    /// panel and the live-reload toast.
    pub fn restart_only_differs(&self, other: &Settings) -> bool {
        self.scrollback_lines != other.scrollback_lines
            || self.two_panel_min_cols != other.two_panel_min_cols
            || self.three_panel_min_cols != other.three_panel_min_cols
            || self.audit_retention_days != other.audit_retention_days
            || self.notifications != other.notifications
            || self.features.automations != other.features.automations
            || self.features.mouse != other.features.mouse
            || self.features.notifications != other.features.notifications
            || self.features.version_check != other.features.version_check
            || self.features.auto_update != other.features.auto_update
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config_version: None,
            scrollback_lines: default_scrollback_lines(),
            two_panel_min_cols: default_two_panel_min_cols(),
            three_panel_min_cols: default_three_panel_min_cols(),
            audit_retention_days: default_audit_retention_days(),
            info_panel_position: InfoPanelPosition::default(),
            features: FeatureFlags::default(),
            notifications: NotificationSettings::default(),
            review: ReviewSettings::default(),
            prefix: PrefixSettings::default(),
        }
    }
}

static GLOBAL: OnceLock<Settings> = OnceLock::new();

/// Publish the loaded settings process-wide. Call once at startup, before the
/// first [`global`] read; later calls are ignored (first writer wins).
pub fn init(settings: Settings) {
    let _ = GLOBAL.set(settings);
}

/// The process-wide settings; defaults when [`init`] was never called (tests,
/// or library use outside the binaries).
pub fn global() -> &'static Settings {
    GLOBAL.get_or_init(Settings::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_yields_defaults() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(s.scrollback_lines, 1000);
        assert_eq!(s.two_panel_min_cols, 80);
        assert_eq!(s.three_panel_min_cols, 120);
        assert_eq!(s.audit_retention_days, 90);
    }

    #[test]
    fn prefix_defaults_to_ctrl_f_plus_f12_alongside_direct_chords() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s.prefix.mode, PrefixMode::Both);
        assert_eq!(s.prefix.hint_delay_ms, 0, "the overlay is the feature");
        assert_eq!(
            s.prefix.chords(),
            vec![KeyChord::ctrl('f'), KeyChord::function(12)]
        );
    }

    #[test]
    fn prefix_mode_parses_kebab_case_and_off_disables_both_leaders() {
        let s: Settings = toml::from_str("[prefix]\nmode = \"prefix-only\"").unwrap();
        assert_eq!(s.prefix.mode, PrefixMode::PrefixOnly);
        assert!(
            !s.prefix.mode.direct_enabled(),
            "Ctrl goes back to the agent"
        );

        let off: Settings = toml::from_str("[prefix]\nmode = \"off\"").unwrap();
        // `off` means no leader at all — including `key2`, which is what gives
        // F12 back to the perf HUD.
        assert!(off.prefix.chords().is_empty());
    }

    #[test]
    fn shipped_prefix_defaults_warn_about_nothing() {
        assert!(PrefixSettings::default().warnings().is_empty());
    }

    #[test]
    fn risky_prefix_rebinds_are_reported_by_name() {
        let s: Settings = toml::from_str("[prefix]\nkey = \"ctrl+b\"").unwrap();
        let w = s.prefix.warnings();
        assert_eq!(w.len(), 1, "got: {w:?}");
        assert!(w[0].contains("ctrl+b") && w[0].contains("tmux"), "{}", w[0]);

        // Both slots are checked, and the chord is canonicalised first so
        // `CTRL+Z` and `ctrl+z` warn identically.
        let s: Settings = toml::from_str("[prefix]\nkey = \"ctrl+f\"\nkey2 = \"CTRL+Z\"").unwrap();
        let w = s.prefix.warnings();
        assert_eq!(w.len(), 1, "got: {w:?}");
        assert!(
            w[0].contains("key2") && w[0].contains("SIGTSTP"),
            "{}",
            w[0]
        );
    }

    #[test]
    fn an_unparseable_prefix_chord_warns_rather_than_failing_silently() {
        let s: Settings = toml::from_str("[prefix]\nkey = \"ctrl+nonsense\"").unwrap();
        let w = s.prefix.warnings();
        assert_eq!(w.len(), 1, "got: {w:?}");
        assert!(w[0].contains("not a valid chord"), "{}", w[0]);
    }

    /// `mode = "off"` means the chords are inert, so warning about them would
    /// be noise.
    #[test]
    fn prefix_warnings_are_silent_when_the_leader_is_off() {
        let s: Settings = toml::from_str("[prefix]\nmode = \"off\"\nkey = \"ctrl+b\"").unwrap();
        assert!(s.prefix.warnings().is_empty());
    }

    #[test]
    fn empty_prefix_key2_drops_the_second_leader() {
        let s: Settings = toml::from_str("[prefix]\nkey2 = \"\"").unwrap();
        assert_eq!(s.prefix.chords(), vec![KeyChord::ctrl('f')]);
    }

    #[test]
    fn unparseable_prefix_key_is_skipped_not_fatal() {
        let s: Settings = toml::from_str("[prefix]\nkey = \"ctrl+nonsense\"").unwrap();
        assert_eq!(
            s.prefix.chords(),
            vec![KeyChord::function(12)],
            "a typo in the primary must still leave the user a way in"
        );
    }

    #[test]
    fn partial_override_keeps_other_defaults() {
        let s: Settings = toml::from_str("scrollback_lines = 5000").unwrap();
        assert_eq!(s.scrollback_lines, 5000);
        assert_eq!(s.audit_retention_days, 90);
    }

    #[test]
    fn type_mismatch_is_rejected() {
        let err = toml::from_str::<Settings>("scrollback_lines = \"many\"").unwrap_err();
        assert!(err.to_string().contains("scrollback_lines"));
    }

    #[test]
    fn info_panel_position_defaults_to_auto_and_parses_each_variant() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s.info_panel_position, InfoPanelPosition::Auto);
        for (raw, want) in [
            ("auto", InfoPanelPosition::Auto),
            ("column", InfoPanelPosition::Column),
            ("inline", InfoPanelPosition::Inline),
        ] {
            let s: Settings =
                toml::from_str(&format!("info_panel_position = \"{raw}\"\n")).unwrap();
            assert_eq!(s.info_panel_position, want, "info_panel_position = {raw}");
            assert_eq!(want.as_str(), raw, "as_str mirrors the serde rename");
        }
    }

    #[test]
    fn info_panel_position_rejects_unknown_value() {
        let err = toml::from_str::<Settings>("info_panel_position = \"floating\"").unwrap_err();
        assert!(
            err.to_string().contains("info_panel_position") || err.to_string().contains("variant")
        );
    }

    #[test]
    fn info_panel_position_applies_live() {
        // Mirrored into `App` state on save/reload like the live feature
        // flags, so it must not register as a restart-only difference.
        let base = Settings::default();
        let mut moved = base.clone();
        moved.info_panel_position = InfoPanelPosition::Inline;
        assert!(!base.restart_only_differs(&moved));
    }

    #[test]
    fn absent_features_table_enables_everything() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s.features, FeatureFlags::default());
        assert!(s.features.tasks && s.features.automations);
    }

    #[test]
    fn empty_features_table_enables_everything() {
        let s: Settings = toml::from_str("[features]").unwrap();
        assert_eq!(s.features, FeatureFlags::default());
    }

    #[test]
    fn partial_features_override_keeps_other_flags_enabled() {
        let s: Settings = toml::from_str("[features]\ntasks = false").unwrap();
        assert!(!s.features.tasks);
        assert!(s.features.automations);
        assert!(s.features.file_viewer);
        assert!(s.features.global_search);
        assert!(s.features.info_panel);
        assert!(s.features.shell_pane);
        assert!(s.features.mouse);
    }

    #[test]
    fn mouse_feature_flag_parses() {
        let s: Settings = toml::from_str("[features]\nmouse = false").unwrap();
        assert!(!s.features.mouse);
        assert!(s.features.tasks, "untouched flags stay enabled");
    }

    #[test]
    fn notifications_feature_flag_parses() {
        let s: Settings = toml::from_str("[features]\nnotifications = false").unwrap();
        assert!(!s.features.notifications);
        assert!(s.features.tasks, "untouched flags stay enabled");
    }

    #[test]
    fn soft_delete_feature_flag_defaults_true_and_parses() {
        assert!(FeatureFlags::default().soft_delete);
        let s: Settings = toml::from_str("[features]\nsoft_delete = false").unwrap();
        assert!(!s.features.soft_delete);
        assert!(s.features.tasks, "untouched flags stay enabled");
    }

    #[test]
    fn notifications_table_defaults() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s.notifications, NotificationSettings::default());
        assert!(!s.notifications.also_on_waiting);
        assert!(s.notifications.suppress_for_active);
        assert!(s.notifications.sound);
        assert_eq!(s.notifications.min_interval_secs, 5);
        assert_eq!(s.notifications.backend, NotificationBackend::Auto);
    }

    #[test]
    fn notifications_backend_parses_each_variant() {
        for (raw, want) in [
            ("auto", NotificationBackend::Auto),
            ("dbus", NotificationBackend::Dbus),
            ("windows", NotificationBackend::Windows),
            ("off", NotificationBackend::Off),
        ] {
            let s: Settings =
                toml::from_str(&format!("[notifications]\nbackend = \"{raw}\"\n")).unwrap();
            assert_eq!(s.notifications.backend, want, "backend = {raw}");
        }
    }

    #[test]
    fn notifications_backend_rejects_unknown_value() {
        let err =
            toml::from_str::<Settings>("[notifications]\nbackend = \"telepathy\"").unwrap_err();
        assert!(err.to_string().contains("backend") || err.to_string().contains("variant"));
    }

    #[test]
    fn notifications_table_partial_override() {
        let s: Settings =
            toml::from_str("[notifications]\nalso_on_waiting = true\nmin_interval_secs = 30\n")
                .unwrap();
        assert!(s.notifications.also_on_waiting);
        assert_eq!(s.notifications.min_interval_secs, 30);
        assert!(s.notifications.suppress_for_active);
        assert!(s.notifications.sound);
    }

    #[test]
    fn notifications_type_mismatch_is_rejected() {
        let err = toml::from_str::<Settings>("[notifications]\nsound = \"loud\"").unwrap_err();
        assert!(err.to_string().contains("sound"));
    }

    #[test]
    fn version_check_flag_defaults_off_and_parses() {
        // Unlike the other flags, version_check is opt-in (network call).
        let s: Settings = toml::from_str("[features]").unwrap();
        assert!(!s.features.version_check, "version_check defaults off");
        assert!(s.features.tasks, "other flags still default on");

        let s: Settings = toml::from_str("[features]\nversion_check = true").unwrap();
        assert!(s.features.version_check);
        assert!(s.features.mouse, "untouched flags stay at their default");
    }

    #[test]
    fn auto_update_flag_defaults_off_and_parses() {
        // Like version_check, auto_update is opt-in (network call + writes to disk).
        let s: Settings = toml::from_str("[features]").unwrap();
        assert!(!s.features.auto_update, "auto_update defaults off");
        assert!(s.features.tasks, "other flags still default on");

        let s: Settings = toml::from_str("[features]\nauto_update = true").unwrap();
        assert!(s.features.auto_update);
        assert!(s.features.mouse, "untouched flags stay at their default");
    }

    #[test]
    fn feature_flag_type_mismatch_is_rejected() {
        let err = toml::from_str::<Settings>("[features]\ntasks = \"no\"").unwrap_err();
        assert!(err.to_string().contains("tasks"));
    }

    #[test]
    fn restart_only_differs_ignores_live_flags_but_catches_restart_ones() {
        let base = Settings::default();

        // A live UI-panel flag is not a restart-only difference.
        let mut live = base.clone();
        live.features.tasks = !live.features.tasks;
        assert!(!base.restart_only_differs(&live));

        // A restart-only feature flag, a scalar, and a notification knob all are.
        let mut mouse = base.clone();
        mouse.features.mouse = !mouse.features.mouse;
        assert!(base.restart_only_differs(&mouse));

        let mut scrollback = base.clone();
        scrollback.scrollback_lines += 1;
        assert!(base.restart_only_differs(&scrollback));

        let mut notif = base.clone();
        notif.notifications.sound = !notif.notifications.sound;
        assert!(base.restart_only_differs(&notif));

        // Identical settings never differ.
        assert!(!base.restart_only_differs(&base.clone()));
    }

    #[test]
    fn every_feature_flag_is_classified_restart_or_live() {
        // Destructuring WITHOUT `..` makes a newly-added feature flag fail to
        // compile here until it is explicitly classified below — the safety net
        // that stops a startup-wired flag from defaulting to "applies live" by
        // omission in `restart_only_differs` (which would wrongly toast "applied
        // live"). Each binding is consumed by `cases`, so none goes unused.
        let FeatureFlags {
            tasks,
            automations,
            file_viewer,
            global_search,
            double_shift_search,
            info_panel,
            shell_pane,
            code_review,
            cc_activity,
            perf_hud,
            mouse,
            notifications,
            soft_delete,
            version_check,
            auto_update,
        } = FeatureFlags::default();
        // Consume every binding so the no-`..` destructure above stays a hard
        // compile-time guard (an unused binding would otherwise be the only
        // warning, not an error).
        let _ = (
            tasks,
            automations,
            file_viewer,
            global_search,
            double_shift_search,
            info_panel,
            shell_pane,
            code_review,
            cc_activity,
            perf_hud,
            mouse,
            notifications,
            soft_delete,
            version_check,
            auto_update,
        );

        // `live` flags gate UI panels read from `App.features` every frame, so
        // flipping one is NOT a restart-only difference; the rest are read once
        // at startup and MUST register as one.
        let live: [fn(&mut FeatureFlags); 10] = [
            |f| f.tasks = !f.tasks,
            |f| f.file_viewer = !f.file_viewer,
            |f| f.global_search = !f.global_search,
            |f| f.double_shift_search = !f.double_shift_search,
            |f| f.info_panel = !f.info_panel,
            |f| f.shell_pane = !f.shell_pane,
            |f| f.code_review = !f.code_review,
            |f| f.cc_activity = !f.cc_activity,
            |f| f.perf_hud = !f.perf_hud,
            |f| f.soft_delete = !f.soft_delete,
        ];
        let restart: [fn(&mut FeatureFlags); 5] = [
            |f| f.automations = !f.automations,
            |f| f.mouse = !f.mouse,
            |f| f.notifications = !f.notifications,
            |f| f.version_check = !f.version_check,
            |f| f.auto_update = !f.auto_update,
        ];

        let base = Settings::default();
        let check = |flip: fn(&mut FeatureFlags), expect_restart: bool| {
            let mut other = base.clone();
            flip(&mut other.features);
            assert_eq!(
                base.restart_only_differs(&other),
                expect_restart,
                "a feature flag's restart classification disagrees with restart_only_differs",
            );
        };
        live.into_iter().for_each(|flip| check(flip, false));
        restart.into_iter().for_each(|flip| check(flip, true));
    }

    #[test]
    fn review_table_defaults_and_parses_each_handoff() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s.review, ReviewSettings::default());
        assert_eq!(s.review.handoff, ReviewHandoff::Structured);
        assert!(s.review.nudge_on_idle);

        for (raw, want) in [
            ("structured", ReviewHandoff::Structured),
            ("legacy", ReviewHandoff::Legacy),
        ] {
            let s: Settings = toml::from_str(&format!("[review]\nhandoff = \"{raw}\"\n")).unwrap();
            assert_eq!(s.review.handoff, want, "handoff = {raw}");
            assert!(s.review.nudge_on_idle, "untouched knobs keep their default");
        }

        let s: Settings = toml::from_str("[review]\nnudge_on_idle = false").unwrap();
        assert!(!s.review.nudge_on_idle);
        assert_eq!(s.review.handoff, ReviewHandoff::Structured);
    }

    #[test]
    fn review_handoff_rejects_unknown_value() {
        let err = toml::from_str::<Settings>("[review]\nhandoff = \"json\"").unwrap_err();
        assert!(err.to_string().contains("handoff") || err.to_string().contains("variant"));
    }

    #[test]
    fn review_settings_apply_live() {
        // Mirrored into `App` state on save/reload like the live feature
        // flags, so they must not register as a restart-only difference.
        let base = Settings::default();
        let mut moved = base.clone();
        moved.review.handoff = ReviewHandoff::Legacy;
        moved.review.nudge_on_idle = false;
        assert!(!base.restart_only_differs(&moved));
    }

    #[test]
    fn global_defaults_when_uninitialized() {
        // Note: other tests may have init()'d already; both paths are default.
        assert_eq!(global().two_panel_min_cols, 80);
    }
}
