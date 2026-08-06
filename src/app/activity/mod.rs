//! The F9 activity view's app layer: scan scheduling and view building.
//!
//! The provider dispatch, source discovery, and the on-disk scan itself live in
//! [`crate::activity`], shared with `friring-cli session activity`. What stays
//! here is what genuinely needs the app: moving each session's accumulator into
//! a `spawn_blocking` pass and back ([`App::start_activity_refresh`] /
//! [`App::poll_activity_refresh`]), and turning the resulting event stream into
//! the view's rows and blocks.
//!
//! Ownership model: each session's [`SessionActivity`] accumulator is *moved*
//! into the scan thread and back each cadence. While in flight the view keeps
//! its last built rows, so rendering never blocks on the scan.

use std::path::PathBuf;

use crate::activity::{
    claude_sub_sources, collect_activity, unsupported_reason, ActivityInput, ScanRoots,
};
pub(crate) use crate::activity::{ActivityRefresh, ProviderKind, SessionActivity};
use crate::session::activity::{
    aggregate_files, ActionKind, ActivityCounts, ActivityEvent, ActivityMeta, FileTouch,
};
use crate::session::cc_activity::TranscriptBlock;
use crate::session::{SessionId, SessionInfo};

use super::background::TaskPoll;
use super::cc_activity::{CcRow, SparkRow, StatTile, Tone};
use super::App;

/// The navigator sections of the activity view, in display order. `Agents`
/// hosts the (Claude-specific) workflow/subagent tree beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    Overview,
    Timeline,
    Commands,
    Files,
    Web,
    Agents,
}

pub(crate) const SECTIONS: [Section; 6] = [
    Section::Overview,
    Section::Timeline,
    Section::Commands,
    Section::Files,
    Section::Web,
    Section::Agents,
];

impl Section {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Section::Overview => "Overview",
            Section::Timeline => "Timeline",
            Section::Commands => "Commands",
            Section::Files => "Files",
            Section::Web => "Web",
            Section::Agents => "Agents",
        }
    }
}

impl App {
    /// The CLI command behind a session's agent (the registry name itself
    /// when the entry is gone) — the provider-dispatch key.
    pub(super) fn session_command(&self, info: &SessionInfo) -> String {
        self.agents
            .get(&info.agent)
            .map(|a| a.command.clone())
            .unwrap_or_else(|| info.agent.clone())
    }

    /// The activity provider for a session, from its registry entry's command
    /// basename.
    pub(crate) fn session_provider(&self, info: &SessionInfo) -> Option<ProviderKind> {
        ProviderKind::for_command(&self.session_command(info))
    }

    /// Kick off a background event scan for every local session with a
    /// provider. Accumulators are moved into the pass and returned by
    /// [`Self::poll_activity_refresh`].
    pub(super) fn start_activity_refresh(&mut self) {
        if self.activity_refresh.in_progress() {
            return;
        }
        type PreInput = (
            SessionId,
            ProviderKind,
            Option<String>,
            Vec<String>,
            Vec<(PathBuf, String)>,
        );
        let pre: Vec<PreInput> = self
            .sessions
            .iter()
            .filter(|s| s.info.remote_host.is_none())
            .filter_map(|s| {
                let provider = self.session_provider(&s.info)?;
                let sub_sources = if provider == ProviderKind::Claude {
                    claude_sub_sources(&s.info)
                } else {
                    Vec::new()
                };
                Some((
                    s.info.id,
                    provider,
                    s.info.agent_session_id.clone(),
                    self.session_candidate_dirs(&s.info),
                    sub_sources,
                ))
            })
            .collect();
        // Evict accumulators for sessions no longer eligible (deleted, gone
        // remote, or repointed to an unsupported command) so their event
        // vectors don't leak across session churn. Safe here: the in_progress
        // guard means no accumulator is checked out, and it must run even when
        // `pre` is empty so every stale entry clears.
        let ids: std::collections::HashSet<SessionId> = pre.iter().map(|(id, ..)| *id).collect();
        self.activity.retain(|id, _| ids.contains(id));
        if pre.is_empty() {
            return;
        }
        let inputs: Vec<ActivityInput> = pre
            .into_iter()
            .map(|(id, provider, own_id, dirs, sub_sources)| {
                let state = self
                    .activity
                    .remove(&id)
                    // An agents.toml edit can repoint a session's provider —
                    // stale accumulators must not survive that.
                    .filter(|a| a.provider == provider)
                    .unwrap_or_else(|| SessionActivity::new(provider));
                ActivityInput {
                    id,
                    own_id,
                    dirs,
                    sub_sources,
                    state,
                }
            })
            .collect();
        let roots = ScanRoots::discover();
        let tx = self.activity_refresh.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(collect_activity(roots, inputs));
        });
    }

    /// Apply a finished scan pass: put the accumulators back and live-refresh
    /// the open view.
    pub(super) fn poll_activity_refresh(&mut self) {
        let TaskPoll::Done(refresh) = self.activity_refresh.poll() else {
            return;
        };
        let mut any_changed = false;
        for (id, state, changed) in refresh.updates {
            self.activity.insert(id, state);
            any_changed |= changed;
        }
        // The open view rebuilds its snapshot every pass (not only on change):
        // a section opened while the accumulator was in flight rendered from
        // nothing and needs the returned state even if the scan saw no growth.
        self.refresh_activity_view(any_changed);
    }
}

fn section_includes(section: Section, kind: ActionKind) -> bool {
    match section {
        Section::Timeline => true,
        Section::Commands => kind == ActionKind::Command,
        Section::Web => matches!(kind, ActionKind::WebSearch | ActionKind::WebFetch),
        _ => false,
    }
}

/// Blocks for an event-list section (timeline / commands / web) — rendered by
/// the same engine as transcripts, so folds/find/wrap come for free. The
/// Timeline additionally folds repetition runs (see [`timeline_blocks`]).
pub(super) fn section_blocks(events: &[ActivityEvent], section: Section) -> Vec<TranscriptBlock> {
    if section == Section::Timeline {
        return timeline_blocks(events);
    }
    events
        .iter()
        .filter(|e| section_includes(section, e.kind))
        .cloned()
        .map(TranscriptBlock::Event)
        .collect()
}

/// The Timeline's block list: every event, with consecutive runs of the same
/// read/search/bookkeeping action folded into one `detail ×N` row (keeping
/// the run's newest timestamp/result) so a re-read loop doesn't drown the
/// turn it belongs to. Commands/edits never fold — each is its own action.
fn timeline_blocks(events: &[ActivityEvent]) -> Vec<TranscriptBlock> {
    let foldable =
        |e: &ActivityEvent| matches!(e.kind, ActionKind::Read | ActionKind::Search) || e.minor;
    let same_run = |a: &ActivityEvent, b: &ActivityEvent| {
        a.kind == b.kind && a.detail == b.detail && a.origin == b.origin && a.minor == b.minor
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let e = &events[i];
        let mut n = 1;
        if foldable(e) {
            while i + n < events.len() && same_run(e, &events[i + n]) {
                n += 1;
            }
        }
        if n > 1 {
            // The newest occurrence carries the freshest result/timestamp.
            let mut folded = events[i + n - 1].clone();
            folded.detail = format!("{}  ×{n}", folded.detail);
            out.push(TranscriptBlock::Event(folded));
        } else {
            out.push(TranscriptBlock::Event(e.clone()));
        }
        i += n;
    }
    out
}

/// The Files section: edited then read-only paths, most recently touched
/// first within each group.
pub(super) fn files_rows(events: &[ActivityEvent]) -> Vec<CcRow> {
    let files = aggregate_files(events);
    if files.is_empty() {
        return vec![CcRow::Info("No file activity yet.".to_string())];
    }
    let mut rows = Vec::new();
    let (edited, read_only): (Vec<&FileTouch>, Vec<&FileTouch>) =
        files.iter().partition(|f| f.edits > 0);
    if !edited.is_empty() {
        rows.push(CcRow::Header(format!("Edited ({})", edited.len())));
        rows.extend(edited.iter().map(|f| CcRow::Text(file_line(f))));
    }
    if !read_only.is_empty() {
        if !rows.is_empty() {
            rows.push(CcRow::Info(String::new()));
        }
        rows.push(CcRow::Header(format!("Read ({})", read_only.len())));
        rows.extend(read_only.iter().map(|f| CcRow::Text(file_line(f))));
    }
    rows
}

fn file_line(f: &FileTouch) -> String {
    let mut line = format!("  {}", f.path);
    if f.edits > 0 {
        line.push_str(&format!("  ✎{}", f.edits));
    }
    if f.reads > 0 {
        line.push_str(&format!("  r{}", f.reads));
    }
    if let Some(ts) = f.last_ts_ms {
        line.push_str(&format!("  {}", fmt_time(ts)));
    }
    line
}

/// Local wall-clock `HH:MM:SS` of an epoch-ms timestamp. Out-of-range values
/// (a malformed record's garbage `ts`) render a placeholder rather than
/// panicking the render thread — `from_timestamp_millis` rejects them.
pub(crate) fn fmt_time(ts_ms: u64) -> String {
    match i64::try_from(ts_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string(),
        None => "--:--:--".to_string(),
    }
}

/// The Overview section, as a dashboard: identity line, stat tiles, an
/// activity sparkline, hottest files, the last failure, and the agents-tree
/// summary. `provider` reflects whether the agent is supported at all;
/// `activity` is its accumulator once a scan pass has run.
pub(super) fn overview_rows(
    agent_name: &str,
    command: &str,
    provider: Option<ProviderKind>,
    activity: Option<&SessionActivity>,
    info: &SessionInfo,
) -> Vec<CcRow> {
    let mut rows = Vec::new();
    match activity {
        None => {
            rows.push(CcRow::Text(format!("Agent: {agent_name}")));
            rows.push(CcRow::Info(String::new()));
            rows.push(CcRow::Info(match provider {
                Some(_) => "No activity captured yet.".to_string(),
                None => unsupported_reason(command)
                    .unwrap_or("Activity capture isn't supported for this agent yet.")
                    .to_string(),
            }));
        }
        Some(act) => {
            let meta = act.meta();
            let mut identity = format!("{agent_name} · {}", act.provider.id());
            if let Some(m) = &meta.model {
                identity.push_str(&format!(" · {m}"));
            }
            rows.push(CcRow::Text(identity));
            if let Some(t) = &meta.title {
                rows.push(CcRow::Text(format!("Title: {t}")));
            }
            if let Some(line) = token_line(&meta) {
                rows.push(CcRow::Text(line));
            }
            let events = act.events();
            let c = ActivityCounts::tally(events);
            rows.push(CcRow::Info(String::new()));
            rows.push(CcRow::Tiles(stat_tiles(&c)));
            if let Some(spark) = spark_row(events) {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Spark(spark));
            }
            if let Some(line) = turns_line(&c, events) {
                rows.push(CcRow::Text(line));
            }
            if act.backfilling() {
                rows.push(CcRow::Info(
                    "⟳ indexing history — older activity still loading…".to_string(),
                ));
            } else if act.truncated() {
                rows.push(CcRow::Info(
                    "(long history — oldest activity clipped)".to_string(),
                ));
            }
            let files = aggregate_files(events);
            if !files.is_empty() {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Hot files".to_string()));
                rows.extend(files.iter().take(5).map(|f| CcRow::Text(file_line(f))));
            }
            let top = top_commands(events);
            if !top.is_empty() {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Top commands".to_string()));
                rows.extend(
                    top.into_iter()
                        .map(|(n, cmd)| CcRow::Text(format!("  ×{n}  {cmd}"))),
                );
            }
            let recent = recent_rows(events);
            if !recent.is_empty() {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Recent".to_string()));
                rows.extend(recent);
            }
            if let Some(e) = events
                .iter()
                .rev()
                .find(|e| e.ok == Some(false) && !e.minor)
            {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Last error".to_string()));
                let mut line = String::from("✗ ");
                if let Some(ts) = e.ts_ms {
                    line.push_str(&format!("{}  ", fmt_time(ts)));
                }
                line.push_str(e.detail.lines().next().unwrap_or(""));
                rows.push(CcRow::Text(line));
                if let Some(r) = &e.result_head {
                    if let Some(first) = r.lines().find(|l| !l.trim().is_empty()) {
                        rows.push(CcRow::Info(format!("  ⎿ {first}")));
                    }
                }
            }
        }
    }
    let (wf, sub) = info
        .cc_activity
        .as_ref()
        .map(|a| (a.workflows.len(), a.subagents.len()))
        .unwrap_or((0, 0));
    if wf + sub > 0 {
        rows.push(CcRow::Info(String::new()));
        rows.push(CcRow::Text(format!(
            "{wf} workflows · {sub} subagents — see the Agents section"
        )));
    }
    rows
}

/// The Overview's tile row. Zero-count tiles stay (a stable layout reads
/// faster than a shifting one); the failure tile appears only on failure.
fn stat_tiles(c: &ActivityCounts) -> Vec<StatTile> {
    let tile = |glyph, value: usize, label, tone| StatTile {
        glyph,
        value: value.to_string(),
        label,
        tone,
    };
    let mut tiles = vec![
        tile("$", c.commands, "cmds", Tone::Accent),
        tile("✎", c.edits, "edits", Tone::Working),
        tile("⊙", c.reads, "reads", Tone::Normal),
        tile("⌕", c.searches, "greps", Tone::Normal),
        tile("⚲", c.web, "web", Tone::Done),
        tile("⚙", c.subagents, "agents", Tone::Accent),
    ];
    if c.failed > 0 {
        tiles.push(tile("✗", c.failed, "failed", Tone::Danger));
    }
    tiles
}

/// Bucket the timestamped, non-minor events across the session's span. `None`
/// when fewer than two distinct timestamps exist (no span to draw).
fn spark_row(events: &[ActivityEvent]) -> Option<SparkRow> {
    const BUCKETS: u64 = 32;
    let ts: Vec<u64> = events
        .iter()
        .filter(|e| !e.minor)
        .filter_map(|e| e.ts_ms)
        .collect();
    let first = *ts.iter().min()?;
    let last = *ts.iter().max()?;
    if first == last {
        return None;
    }
    let span = last - first + 1;
    let mut buckets = vec![0u64; BUCKETS as usize];
    let last_bucket = buckets.len() - 1;
    for t in &ts {
        let i = ((t - first) * BUCKETS / span) as usize;
        buckets[i.min(last_bucket)] += 1;
    }
    Some(SparkRow {
        start: fmt_hm(first),
        end: fmt_hm(last),
        buckets,
        caption: format!("{} events · {}", ts.len(), fmt_span_ms(span)),
    })
}

/// Compact token count: `1.2M`, `128k`, exact below five digits.
fn fmt_tok(t: u64) -> String {
    if t >= 1_000_000 {
        format!("{:.1}M", t as f64 / 1e6)
    } else if t >= 10_000 {
        format!("{}k", t / 1000)
    } else {
        t.to_string()
    }
}

/// `tokens  12k in · 128k out · cache 1.2M r / 340k w` — whatever tallies the
/// stream records (main + subagent/workflow transcripts, via
/// [`SessionActivity::meta`]); `None` when nothing was recorded.
fn token_line(meta: &ActivityMeta) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(t) = meta.input_tokens.filter(|&t| t > 0) {
        parts.push(format!("{} in", fmt_tok(t)));
    }
    if let Some(t) = meta.output_tokens.filter(|&t| t > 0) {
        parts.push(format!("{} out", fmt_tok(t)));
    }
    let read = meta.cache_read_tokens.filter(|&t| t > 0);
    let write = meta.cache_write_tokens.filter(|&t| t > 0);
    match (read, write) {
        (Some(r), Some(w)) => parts.push(format!("cache {} r / {} w", fmt_tok(r), fmt_tok(w))),
        (Some(r), None) => parts.push(format!("cache {} r", fmt_tok(r))),
        (None, Some(w)) => parts.push(format!("cache {} w", fmt_tok(w))),
        (None, None) => {}
    }
    (!parts.is_empty()).then(|| format!("tokens  {}", parts.join(" · ")))
}

/// `8 turns · last action 14:41:07`, under the sparkline.
fn turns_line(c: &ActivityCounts, events: &[ActivityEvent]) -> Option<String> {
    let mut parts = Vec::new();
    match c.prompts {
        0 => {}
        1 => parts.push("1 turn".to_string()),
        n => parts.push(format!("{n} turns")),
    }
    // The last *action* — not a turn marker or bookkeeping row — so a freshly
    // submitted prompt can't misreport when the agent last did work.
    if let Some(ts) = events
        .iter()
        .rev()
        .filter(|e| e.kind != ActionKind::Prompt && !e.minor)
        .find_map(|e| e.ts_ms)
    {
        parts.push(format!("last action {}", fmt_time(ts)));
    }
    (!parts.is_empty()).then(|| format!(" {}", parts.join(" · ")))
}

/// The most-repeated command lines (≥2 runs), count-desc — surfacing what the
/// agent looped on. At most three.
fn top_commands(events: &[ActivityEvent]) -> Vec<(usize, String)> {
    let mut freq: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in events {
        if e.kind == ActionKind::Command && !e.minor {
            *freq
                .entry(e.detail.lines().next().unwrap_or(""))
                .or_default() += 1;
        }
    }
    let mut top: Vec<(usize, String)> = freq
        .into_iter()
        .filter(|&(_, n)| n >= 2)
        .map(|(cmd, n)| (n, cmd.to_string()))
        .collect();
    top.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    top.truncate(3);
    top
}

/// The newest few actions, oldest-first — "what just happened" without
/// leaving the Overview.
fn recent_rows(events: &[ActivityEvent]) -> Vec<CcRow> {
    let newest: Vec<&ActivityEvent> = events
        .iter()
        .rev()
        .filter(|e| !e.minor && e.kind != ActionKind::Prompt)
        .take(4)
        .collect();
    newest
        .into_iter()
        .rev()
        .map(|e| {
            let mut line = String::from("  ");
            if let Some(ts) = e.ts_ms {
                line.push_str(&format!("{}  ", fmt_time(ts)));
            }
            line.push_str(&format!(
                "{:<5} {}",
                e.kind.tag(),
                e.detail.lines().next().unwrap_or("")
            ));
            if e.ok == Some(false) {
                line.push_str("  ✗");
            }
            CcRow::Text(line)
        })
        .collect()
}

/// Local wall-clock `HH:MM` (sparkline endpoints).
fn fmt_hm(ts_ms: u64) -> String {
    match i64::try_from(ts_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(dt) => dt.with_timezone(&chrono::Local).format("%H:%M").to_string(),
        None => "--:--".to_string(),
    }
}

/// Compact duration: `38s`, `42m`, `1h12m`.
fn fmt_span_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: ActionKind, detail: &str) -> ActivityEvent {
        ActivityEvent {
            ts_ms: None,
            kind,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        }
    }

    #[test]
    fn fmt_time_never_panics_on_out_of_range_ts() {
        // A malformed record can carry an arbitrary `ts`; the conversion must
        // degrade to a placeholder instead of panicking the render thread.
        assert_eq!(fmt_time(u64::MAX), "--:--:--");
        // A sane in-range timestamp still formats to wall-clock.
        assert_ne!(fmt_time(1_783_512_000_000), "--:--:--");
    }

    #[test]
    fn section_blocks_filter_by_kind() {
        let events = [
            ev(ActionKind::Command, "ls"),
            ev(ActionKind::Edit, "/a.rs"),
            ev(ActionKind::WebSearch, "docs"),
            ev(ActionKind::WebFetch, "https://x"),
        ];
        assert_eq!(section_blocks(&events, Section::Timeline).len(), 4);
        assert_eq!(section_blocks(&events, Section::Commands).len(), 1);
        assert_eq!(section_blocks(&events, Section::Web).len(), 2);
    }

    #[test]
    fn overview_rows_cover_supported_and_unsupported() {
        let mut info = SessionInfo::new("s".to_string());
        let rows = overview_rows("my-agent", "my-agent-cli", None, None, &info);
        assert!(rows.iter().any(
            |r| matches!(r, CcRow::Info(s) if s.contains("isn't supported for this agent yet"))
        ));
        // Supported provider, but no scan pass yet.
        let rows = overview_rows("claude", "claude", Some(ProviderKind::Claude), None, &info);
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Info(s) if s.contains("No activity captured yet"))));
        // A researched-but-unsupported CLI names its reason.
        let rows = overview_rows("antigravity", "agy", None, None, &info);
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Info(s) if s.contains("encrypts its conversation store"))));

        // A vibe accumulator as a scan pass would leave it: one command event
        // and a sidecar title. (The record→event parsing itself is covered in
        // `session::activity::vibe`; this asserts the rendering.)
        let act = SessionActivity::seeded_with_meta(
            ProviderKind::Vibe,
            vec![ev(ActionKind::Command, "make")],
            ActivityMeta {
                title: Some("Build it".into()),
                ..Default::default()
            },
        );
        info.cc_activity = None;
        let rows = overview_rows("vibe", "vibe", Some(ProviderKind::Vibe), Some(&act), &info);
        // Identity line: `<agent> · <provider id> · …`.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.starts_with("vibe · vibe"))));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains("Title: Build it"))));
        // The counts render as stat tiles now — one command tile with value 1.
        assert!(rows.iter().any(|r| matches!(
            r,
            CcRow::Tiles(t) if t.iter().any(|x| x.label == "cmds" && x.value == "1")
        )));
    }

    #[test]
    fn overview_dashboard_rows_cover_spark_error_and_loader() {
        let ts = |m: u64| Some(1_783_512_000_000 + m * 60_000);
        let mk = |kind, detail: &str, ts_ms: Option<u64>, ok| ActivityEvent {
            ts_ms,
            kind,
            detail: detail.into(),
            note: None,
            result_head: Some("boom: exit 101".into()),
            ok,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        let events = vec![
            mk(ActionKind::Prompt, "Fix it", ts(0), None),
            mk(ActionKind::Command, "cargo test", ts(1), Some(false)),
            mk(ActionKind::Edit, "/repo/a.rs", ts(30), Some(true)),
        ];
        let act = SessionActivity::seeded(ProviderKind::Claude, events);
        let info = SessionInfo::new("s".to_string());
        let rows = overview_rows(
            "claude",
            "claude",
            Some(ProviderKind::Claude),
            Some(&act),
            &info,
        );
        // A sparkline spans the 30-minute session.
        assert!(rows.iter().any(|r| matches!(
            r,
            CcRow::Spark(s) if s.caption.contains("events") && s.caption.contains("30m")
        )));
        // The failed command surfaces as the Last error block.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Header(s) if s == "Last error")));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains('✗') && s.contains("cargo test"))));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Info(s) if s.contains("boom: exit 101"))));
        // The failed tile appears only because a failure exists.
        assert!(rows.iter().any(|r| matches!(
            r,
            CcRow::Tiles(t) if t.iter().any(|x| x.label == "failed" && x.value == "1")
        )));
        // Hot files header present for the edited file.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Header(s) if s == "Hot files")));
        // The turns line counts the single prompt.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains("1 turn ·"))));
        // Recent lists the newest actions with their kind tags.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Header(s) if s == "Recent")));
        assert!(rows.iter().any(
            |r| matches!(r, CcRow::Text(s) if s.contains("edit") && s.contains("/repo/a.rs"))
        ));
    }

    #[test]
    fn token_line_reports_all_recorded_tallies() {
        let meta = ActivityMeta {
            output_tokens: Some(128_000),
            input_tokens: Some(12_400),
            cache_read_tokens: Some(1_234_567),
            cache_write_tokens: Some(340_000),
            ..Default::default()
        };
        assert_eq!(
            token_line(&meta).as_deref(),
            Some("tokens  12k in · 128k out · cache 1.2M r / 340k w")
        );
        // Output-only stream (most non-claude providers).
        let meta = ActivityMeta {
            output_tokens: Some(500),
            ..Default::default()
        };
        assert_eq!(token_line(&meta).as_deref(), Some("tokens  500 out"));
        assert_eq!(token_line(&ActivityMeta::default()), None);
    }

    #[test]
    fn top_commands_surface_only_repeats() {
        let cmd = |detail: &str| ActivityEvent {
            ts_ms: None,
            kind: ActionKind::Command,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        let events = vec![
            cmd("cargo test"),
            cmd("cargo test"),
            cmd("cargo test"),
            cmd("cargo fmt"),
            cmd("cargo fmt"),
            cmd("git status"), // ran once → not "top"
        ];
        let top = top_commands(&events);
        assert_eq!(
            top,
            vec![(3, "cargo test".to_string()), (2, "cargo fmt".to_string())]
        );
        assert!(top_commands(&[cmd("once")]).is_empty());
    }

    #[test]
    fn timeline_blocks_fold_repeated_reads() {
        let mk = |kind, detail: &str| ActivityEvent {
            ts_ms: None,
            kind,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        let events = vec![
            mk(ActionKind::Read, "/a.rs"),
            mk(ActionKind::Read, "/a.rs"),
            mk(ActionKind::Read, "/a.rs"),
            mk(ActionKind::Command, "ls"),
            mk(ActionKind::Command, "ls"), // commands never fold
            mk(ActionKind::Read, "/a.rs"), // non-consecutive → its own row
        ];
        let blocks = timeline_blocks(&events);
        assert_eq!(blocks.len(), 4);
        let TranscriptBlock::Event(first) = &blocks[0] else {
            panic!("event block");
        };
        assert_eq!(first.detail, "/a.rs  ×3");
        let TranscriptBlock::Event(last) = &blocks[3] else {
            panic!("event block");
        };
        assert_eq!(last.detail, "/a.rs");
    }

    #[test]
    fn timeline_folding_covers_searches_minor_and_boundaries() {
        let mk = |kind, detail: &str, origin: Option<&str>, minor: bool| ActivityEvent {
            ts_ms: None,
            kind,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: origin.map(String::from),
            minor,
            dur_ms: None,
        };
        // Searches fold; a changed detail breaks the run; bookkeeping (minor)
        // folds; and a differing origin is a separate run even at equal detail.
        let events = vec![
            mk(ActionKind::Search, "fn main", None, false),
            mk(ActionKind::Search, "fn main", None, false),
            mk(ActionKind::Search, "fn other", None, false), // detail changed → new row
            mk(ActionKind::Other, "TodoWrite", None, true),
            mk(ActionKind::Other, "TodoWrite", None, true), // minor run folds
            mk(ActionKind::Read, "/a.rs", None, false),
            mk(ActionKind::Read, "/a.rs", Some("sub"), false), // origin differs → separate
        ];
        let details: Vec<String> = timeline_blocks(&events)
            .iter()
            .map(|b| match b {
                TranscriptBlock::Event(e) => e.detail.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            details,
            vec![
                "fn main  ×2".to_string(),
                "fn other".to_string(),
                "TodoWrite  ×2".to_string(),
                "/a.rs".to_string(),
                "/a.rs".to_string(), // not folded across the origin boundary
            ]
        );
    }

    #[test]
    fn turns_line_last_action_ignores_prompts_and_minor() {
        let ev = |kind, ts: u64, minor: bool| ActivityEvent {
            ts_ms: Some(ts),
            kind,
            detail: "x".into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor,
            dur_ms: None,
        };
        // A command at T, then a bookkeeping row and a fresh prompt after it —
        // "last action" must report the command's time, not the later rows.
        let base = 1_783_512_000_000;
        let events = vec![
            ev(ActionKind::Command, base, false),
            ev(ActionKind::Other, base + 60_000, true), // minor
            ev(ActionKind::Prompt, base + 120_000, false),
        ];
        let counts = ActivityCounts::tally(&events);
        let line = turns_line(&counts, &events).expect("a turn was counted");
        assert!(line.contains("1 turn"), "{line}");
        assert!(
            line.contains(&format!("last action {}", fmt_time(base))),
            "last action must be the command, not the prompt/minor: {line}"
        );
    }
}
