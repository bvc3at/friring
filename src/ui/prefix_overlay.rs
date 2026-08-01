//! The which-key overlay: everything reachable from the armed leader key,
//! grouped into columns.
//!
//! Rendered whenever `App::prefix_state` is armed (after
//! `prefix.hint_delay_ms`, which defaults to 0 — the overlay *is* the leader's
//! discoverability, so hiding it behind a delay defeats the point). Like the
//! perf HUD it never captures input: `App::handle_prefix_key` owns the keys,
//! this only paints them.
//!
//! Rows come from [`crate::session::prefix_sections`], the same table the
//! dispatcher resolves against, so the overlay can never advertise a key that
//! does nothing (enforced by `prefix_sections_match_the_leader_table`).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::session::{Action, KeyChord, PrefixEntry};

use super::theme::Theme;

/// Width of one column: `  key   label` with room for the longest label.
const COL_WIDTH: u16 = 30;
/// Leave this much frame margin so the overlay never touches the edges.
const MARGIN: u16 = 2;

/// What a leader key does, as one row: the key to press and its description.
struct Row {
    key: String,
    label: &'static str,
}

/// One section rendered as a column: its title and its rows.
type Column = (&'static str, Vec<Row>);

/// Height of one band of side-by-side columns: its tallest column's rows plus
/// the section title above them.
fn band_height(band: &[Column]) -> u16 {
    band.iter().map(|(_, rows)| rows.len()).max().unwrap_or(0) as u16 + 1
}

/// Height of every band stacked, with one blank separator line between them.
fn stacked_height(bands: &[&[Column]]) -> u16 {
    bands.iter().map(|b| band_height(b)).sum::<u16>() + bands.len().saturating_sub(1) as u16
}

/// The human label for an action in the leader table. Deliberately terser than
/// the F1 help text — these are scanned in a grid while a key is held pending,
/// not read as documentation.
fn action_label(action: Action) -> &'static str {
    use Action::*;
    match action {
        NextSession => "next session",
        PreviousSession => "prev session",
        NextLoadedSession => "next loaded",
        PreviousLoadedSession => "prev loaded",
        FocusBackward => "focus prev panel",
        FocusForward => "focus next panel",
        LastSession => "last session",
        NextBlockedSession => "next blocked",
        JumpToBlocked => "blocked 1-9…",
        NewSession => "new session",
        DeleteSession => "delete session",
        RestartSession => "restart session",
        UnloadSession => "unload (ghost)",
        ForkSession => "fork session",
        UndoDelete => "undo delete",
        OpenRestoreSessions => "restore deleted",
        OpenAutomations => "automations",
        FocusTasks => "tasks",
        OpenInEditor => "open in $EDITOR",
        StartSync => "sync worktrees",
        QuitApp => "quit",
        ReloadApp => "reload friring",
        ToggleShell => "shell pane",
        ToggleReview => "code review",
        ToggleCcActivity => "activity view",
        ToggleHelp => "help",
        ToggleInfoPanel => "info panel",
        ToggleFileViewer => "file viewer",
        OpenThemePicker => "theme",
        GlobalSearch => "search",
        OpenSettings => "settings",
        TogglePerfHud => "perf HUD",
        // Scoped actions never get a leader key, so they never reach a row.
        _ => "",
    }
}

/// Turn one section's entries into printable rows. `leader` is the armed
/// chord, needed for the send-literal row (which shows the actual key the
/// user configured, not a hardcoded `Ctrl+F`).
fn rows_for(entries: &[PrefixEntry], leader: &KeyChord) -> Vec<Row> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            PrefixEntry::Action(a) => Some(Row {
                key: a.prefix_key()?.display(),
                label: action_label(*a),
            }),
            PrefixEntry::SessionDigits => Some(Row {
                key: "1-9".into(),
                label: "go to session N",
            }),
            PrefixEntry::MoveSession { up } => Some(Row {
                key: crate::session::keybindings::move_session_key(*up).display(),
                label: if *up {
                    "move up 1-9…"
                } else {
                    "move down 1-9…"
                },
            }),
            PrefixEntry::SendLiteral => Some(Row {
                key: leader.display(),
                label: "send key to agent",
            }),
        })
        .collect()
}

/// Render the overlay centred in `area` (the full frame).
///
/// Degrades rather than disappearing on a small terminal: groups that don't fit
/// side by side wrap onto stacked bands, and the whole overlay is skipped only
/// when even one column cannot fit. A leader that armed with no visible
/// feedback would look like a frozen app.
pub(crate) fn render_prefix_overlay(frame: &mut Frame, area: Rect, leader: &KeyChord) {
    let sections = crate::session::prefix_sections();
    let columns: Vec<Column> = sections
        .iter()
        .map(|(title, entries)| (*title, rows_for(entries, leader)))
        .filter(|(_, rows)| !rows.is_empty())
        .collect();
    if columns.is_empty() {
        return;
    }

    // How many columns fit side by side, capped at what we actually have.
    let usable = area.width.saturating_sub(MARGIN * 2 + 2);
    let fit = (usable / COL_WIDTH).max(1) as usize;
    let shown = fit.min(columns.len());
    if usable < COL_WIDTH {
        return;
    }

    // The leftovers wrap onto further bands rather than being dropped: the
    // overlay's whole job is to list *everything* reachable from the leader,
    // so a narrow terminal must cost height, not entries.
    let mut bands: Vec<&[Column]> = columns.chunks(fit).collect();
    let max_h = area.height.saturating_sub(MARGIN);
    // Still too tall: shed trailing bands, so what survives is whole groups
    // rather than half of one.
    while bands.len() > 1 && stacked_height(&bands) + 2 > max_h {
        bands.pop();
    }
    let inner_h = stacked_height(&bands);
    let w = (COL_WIDTH * shown as u16 + 2).min(area.width.saturating_sub(MARGIN * 2));
    let h = (inner_h + 2).min(max_h);
    if w < COL_WIDTH || h < 5 {
        return;
    }

    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Theme::accent()))
        .title(Span::styled(
            format!(" {} ", leader.display()),
            Style::default()
                .fg(Theme::accent())
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " esc cancel ",
            Style::default().fg(Theme::text_muted()),
        ))
        .style(Style::default().bg(Theme::modal_bg()));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // Paint column by column so each keeps its own width regardless of how
    // many rows its neighbours have.
    let mut band_y = inner.y;
    for band in &bands {
        let h = band_height(band).min(inner.bottom().saturating_sub(band_y));
        if h == 0 {
            break;
        }
        for (i, (title, rows)) in band.iter().enumerate() {
            let col_x = inner.x + i as u16 * COL_WIDTH;
            if col_x >= inner.right() {
                break;
            }
            let col_w = COL_WIDTH.min(inner.right() - col_x);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                *title,
                Style::default()
                    .fg(Theme::text_muted())
                    .add_modifier(Modifier::BOLD),
            ))];
            for r in rows.iter() {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:>5} ", r.key),
                        Style::default()
                            .fg(Theme::accent())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(r.label, Style::default().fg(Theme::text_primary())),
                ]));
            }
            frame.render_widget(Paragraph::new(lines), Rect::new(col_x, band_y, col_w, h));
        }
        band_y += h + 1; // blank separator between stacked bands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::prefix_sections;

    /// Every advertised row has a non-empty label — an unlabelled key in a
    /// which-key overlay is worse than no row at all.
    #[test]
    fn every_leader_row_has_a_label() {
        let leader = KeyChord::ctrl('f');
        for (title, entries) in prefix_sections() {
            for row in rows_for(&entries, &leader) {
                assert!(
                    !row.label.is_empty(),
                    "section `{title}` has an unlabelled key `{}`",
                    row.key
                );
                assert!(!row.key.is_empty(), "section `{title}` has an empty key");
            }
        }
    }

    /// The send-literal row shows the *configured* leader, so a user who
    /// rebound it doesn't read a stale `ctrl+f`.
    #[test]
    fn send_literal_row_reflects_the_configured_leader() {
        let rows = rows_for(&[PrefixEntry::SendLiteral], &KeyChord::function(12));
        assert_eq!(rows[0].key, "f12");
    }
}
