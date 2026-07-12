use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::session::settings::InfoPanelPosition;

pub struct PanelAreas {
    pub header: Rect,
    /// Session list area (top of the left column).
    pub left_panel: Option<Rect>,
    /// Automations pane, below the session list in the left column. Present
    /// (even with zero automations) as long as the automations feature is
    /// enabled and the column is tall enough to fit both lists; its height
    /// grows with the automation count.
    pub automations_panel: Option<Rect>,
    /// Info panel (F2): either the dedicated column between the session list
    /// and the terminal, or — when [`InfoPanelPosition`] resolves to the
    /// inline dock — a pane at the bottom of the left column.
    pub info_panel: Option<Rect>,
    /// Tasks panel — a toggleable column on the right, between the terminal and
    /// the file viewer (behaves like the file viewer).
    pub tasks_panel: Option<Rect>,
    pub file_viewer: Option<Rect>,
    /// Global search popup — centered, floating over the content (JetBrains
    /// Search-Everywhere-style) when active. The panels underneath keep their
    /// size; matches highlight live inside them around the popup.
    pub global_search: Option<Rect>,
    /// Full-width transient band for the active status/error message (or the
    /// sync spinner), docked directly above the footer. Present only while a
    /// message is showing, so nothing is clipped by the footer pills.
    pub status_message: Option<Rect>,
    pub terminal: Rect,
    pub footer: Rect,
}

/// Rows the global-search popup occupies: a 2-row border around a query line,
/// a per-scope match summary, a scrollable result list (~11 rows), and a
/// key-hint line. Matches also highlight live in the panels around the popup.
const GLOBAL_SEARCH_POPUP_ROWS: u16 = 16;

/// Popup width bounds: ~60% of the terminal, clamped so it neither collapses
/// on medium terminals nor sprawls on ultrawide ones (below the minimum the
/// popup just takes the full width).
const GLOBAL_SEARCH_POPUP_MIN_WIDTH: u16 = 50;
const GLOBAL_SEARCH_POPUP_MAX_WIDTH: u16 = 90;

/// The centered global-search popup rect: horizontally centered, top edge in
/// the upper third (where JetBrains' Search Everywhere sits), floating over
/// the content — computed from the full frame area, independent of the
/// band/column splits, so opening the search never resizes the panels or the
/// session PTYs behind it.
fn global_search_popup(area: Rect, show_status_row: bool) -> Rect {
    let width = ((area.width as u32 * 3 / 5) as u16)
        .clamp(GLOBAL_SEARCH_POPUP_MIN_WIDTH, GLOBAL_SEARCH_POPUP_MAX_WIDTH)
        .min(area.width);
    let y_off = (area.height / 6).min(area.height.saturating_sub(1));
    // Keep the footer — and, when shown, the transient status-message row above
    // it — visible below the popup. Both render *after* the popup (`App::view`),
    // so an overlap would overwrite the popup's bottom border on short
    // terminals; reserving their rows keeps the popup clean instead.
    let reserved = if show_status_row { 2 } else { 1 };
    let height =
        GLOBAL_SEARCH_POPUP_ROWS.min(area.height.saturating_sub(y_off).saturating_sub(reserved));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + y_off,
        width,
        height,
    }
}

/// Max rows (including borders) the automations pane may occupy.
const AUTOMATIONS_PANE_MAX_ROWS: u16 = 10;
/// Minimum rows the automations pane occupies (border + one content row), so it
/// stays visible even with zero automations.
const AUTOMATIONS_PANE_MIN_ROWS: u16 = 3;
/// Minimum rows the session list keeps when the automations pane is shown.
const SESSIONS_MIN_ROWS: u16 = 3;
/// Minimum rows (incl. borders) for the inline info pane; a shorter clamp
/// drops the pane instead of rendering a useless sliver.
const INFO_PANE_MIN_ROWS: u16 = 3;

/// Rows the automations pane occupies in a left column `col_height` rows tall
/// (`0` when the feature is off or the column is too short for both lists).
/// This is the same clamp [`split_left_column`] applies, exposed separately so
/// the `Auto` info-pane fit test in [`compute_layout`] can never disagree with
/// the split.
fn automations_pane_rows(col_height: u16, show: bool, automation_count: usize) -> u16 {
    if !show {
        return 0;
    }
    let desired =
        (automation_count as u16 + 2).clamp(AUTOMATIONS_PANE_MIN_ROWS, AUTOMATIONS_PANE_MAX_ROWS);
    let h = desired.min(col_height.saturating_sub(SESSIONS_MIN_ROWS));
    if h < AUTOMATIONS_PANE_MIN_ROWS {
        0
    } else {
        h
    }
}

/// Split a left-column rect into (sessions, automations, inline info).
/// `auto_rows` comes from [`automations_pane_rows`]; `inline_info_rows` is the
/// info pane's desired height (`0` = not inlined), clamped to what's left once
/// the session list keeps its minimum. The panes are bottom-docked in that
/// order and the session list absorbs any slack.
fn split_left_column(
    col: Rect,
    auto_rows: u16,
    inline_info_rows: u16,
) -> (Rect, Option<Rect>, Option<Rect>) {
    let info_h = inline_info_rows.min(col.height.saturating_sub(SESSIONS_MIN_ROWS + auto_rows));
    let info_h = if info_h < INFO_PANE_MIN_ROWS {
        0
    } else {
        info_h
    };
    if auto_rows == 0 && info_h == 0 {
        // No bottom panes to dock (automations pane absent — feature off or the
        // column too short — and info not inlined here): the session list takes
        // the whole column.
        return (col, None, None);
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(SESSIONS_MIN_ROWS),
            Constraint::Length(auto_rows),
            Constraint::Length(info_h),
        ])
        .split(col);
    (
        rows[0],
        (auto_rows > 0).then(|| rows[1]),
        (info_h > 0).then(|| rows[2]),
    )
}

/// Vertical bands carved from the full area: header, content region, optional
/// status-message row, and footer. (The global-search popup floats over the
/// content instead of occupying a band — see [`global_search_popup`].)
struct VerticalBands {
    header: Rect,
    content: Rect,
    status_message: Option<Rect>,
    footer: Rect,
}

/// Split the full area into header / content / status-message / footer bands.
fn split_vertical(area: Rect, show_status_row: bool) -> VerticalBands {
    // Compact mode: when the terminal is shorter than 20 rows, drop the
    // header line entirely so the content + footer get every row available.
    let header_height = if area.height < 20 { 0 } else { 1 };

    // One transient row for the active status/error message, directly above the
    // footer (keeping the pills pinned to the bottom edge). Carved only while a
    // message is showing, so content shrinks by 1 only transiently.
    let status_height = if show_status_row { 1 } else { 0 };

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(status_height),
            Constraint::Length(1),
        ])
        .split(area);

    VerticalBands {
        header: vertical[0],
        content: vertical[1],
        status_message: (status_height > 0).then_some(vertical[2]),
        footer: vertical[3],
    }
}

/// Build the wide (≥ three_panel_min_cols) layout with optional info / tasks /
/// file-viewer columns. Column order: list | info? | terminal | tasks? |
/// file_viewer?. `show_info_column` and `inline_info_rows` are mutually
/// exclusive — [`compute_layout`] resolves the info-pane placement first.
fn three_panel_layout(
    bands: &VerticalBands,
    content: Rect,
    show_info_column: bool,
    show_tasks_panel: bool,
    show_file_viewer: bool,
    auto_rows: u16,
    inline_info_rows: u16,
) -> PanelAreas {
    let mut constraints: Vec<Constraint> = vec![Constraint::Percentage(18)];
    if show_info_column {
        constraints.push(Constraint::Percentage(15));
    }
    // terminal takes the remainder
    let terminal_idx = constraints.len();
    constraints.push(Constraint::Min(0));
    if show_tasks_panel {
        constraints.push(Constraint::Percentage(20));
    }
    if show_file_viewer {
        constraints.push(Constraint::Percentage(20));
    }

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(content);

    let info_column = show_info_column.then(|| horizontal[1]);
    let terminal = horizontal[terminal_idx];
    // Tasks (if shown) immediately follow the terminal; the file viewer
    // follows tasks (or the terminal when tasks are hidden).
    let mut next = terminal_idx + 1;
    let tasks_panel = show_tasks_panel.then(|| {
        let r = horizontal[next];
        next += 1;
        r
    });
    let file_viewer = show_file_viewer.then(|| horizontal[next]);

    let (left_panel, automations_panel, inline_info) =
        split_left_column(horizontal[0], auto_rows, inline_info_rows);
    PanelAreas {
        header: bands.header,
        left_panel: Some(left_panel),
        automations_panel,
        info_panel: info_column.or(inline_info),
        tasks_panel,
        file_viewer,
        global_search: None,
        status_message: bands.status_message,
        terminal,
        footer: bands.footer,
    }
}

/// Build the 2-panel layout: 25% list | 75% terminal.
fn two_panel_layout(
    bands: &VerticalBands,
    content: Rect,
    auto_rows: u16,
    inline_info_rows: u16,
) -> PanelAreas {
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(content);

    let (left_panel, automations_panel, inline_info) =
        split_left_column(horizontal[0], auto_rows, inline_info_rows);
    PanelAreas {
        header: bands.header,
        left_panel: Some(left_panel),
        automations_panel,
        info_panel: inline_info,
        tasks_panel: None,
        file_viewer: None,
        global_search: None,
        status_message: bands.status_message,
        terminal: horizontal[1],
        footer: bands.footer,
    }
}

/// Inputs to [`compute_layout`]: the panel-visibility flags plus the measured
/// row counts that place the info pane.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutParams {
    /// Info panel visibility (F2).
    pub show_info_panel: bool,
    /// Where the info panel docks (settings key `info_panel_position`).
    pub info_position: InfoPanelPosition,
    /// Rows (incl. borders) the info panel's full content needs — measured via
    /// [`super::info_panel::content_rows`]. Sizes the inline pane and drives
    /// the `Auto` fit test; ignored for `Column`.
    pub info_rows: u16,
    /// Rows (incl. borders) the session list needs to show every row (sessions
    /// plus repo-group headers). Drives the `Auto` fit test only.
    pub session_rows: u16,
    pub show_tasks_panel: bool,
    pub show_file_viewer: bool,
    pub show_global_search: bool,
    /// False when the `automations` feature flag is off.
    pub show_automations_pane: bool,
    /// Sizes the automations pane.
    pub automation_count: usize,
    /// Carve the transient status/error row directly above the footer.
    pub show_status_row: bool,
}

/// Compute panel layout areas based on terminal dimensions and
/// [`LayoutParams`].
///
/// At width ≥ 120, the layout becomes
/// `list | info? | terminal | tasks? | file_viewer?` with info (15%), tasks
/// (20%), and file_viewer (20%) appearing only when requested. The tasks panel
/// sits between the terminal and the file viewer (both right-side columns). The
/// left column is further split into a session list, an automations pane
/// beneath it (whenever the column is tall enough and `show_automations_pane`
/// is set — false when the `automations` feature flag is off; `automation_count`
/// only sizes that pane), and — when [`InfoPanelPosition`] resolves to the
/// inline dock — the info pane at the bottom.
///
/// `show_status_row` carves a transient full-width 1-row band directly above the
/// footer for the active status/error message (or the sync spinner), so a long
/// message is never clipped by the right-aligned footer pills. It shrinks the
/// content region by one row while shown.
///
/// `show_global_search` floats the centered popup (`global_search_popup`)
/// over the content; no band is carved and no panel shrinks.
pub fn compute_layout(area: Rect, p: &LayoutParams) -> PanelAreas {
    let mut areas = compute_panel_areas(area, p);
    areas.global_search = p
        .show_global_search
        .then(|| global_search_popup(area, p.show_status_row));
    areas
}

/// The band/column split behind [`compute_layout`] — everything except the
/// floating global-search popup.
fn compute_panel_areas(area: Rect, p: &LayoutParams) -> PanelAreas {
    let bands = split_vertical(area, p.show_status_row);
    let content = bands.content;

    let settings = crate::session::settings::global();
    if area.width < settings.two_panel_min_cols {
        return PanelAreas {
            header: bands.header,
            left_panel: None,
            automations_panel: None,
            info_panel: None,
            tasks_panel: None,
            file_viewer: None,
            global_search: None,
            status_message: bands.status_message,
            terminal: content,
            footer: bands.footer,
        };
    }

    // Both column branches give the left column the full content height, so
    // the automations-pane rows (and the fit test below) are settled here.
    let auto_rows =
        automations_pane_rows(content.height, p.show_automations_pane, p.automation_count);

    // Resolve where a visible info panel docks this frame: `inline_rows > 0`
    // puts it at the bottom of the left column; otherwise a still-visible
    // panel falls back to the dedicated column (three-panel widths only).
    // `Auto` inlines only when the full session list, the automations pane,
    // and the full info content fit the column together.
    let inline_rows = if p.show_info_panel {
        match p.info_position {
            InfoPanelPosition::Column => 0,
            InfoPanelPosition::Inline => p.info_rows,
            InfoPanelPosition::Auto => {
                let needed = p
                    .session_rows
                    .saturating_add(auto_rows)
                    .saturating_add(p.info_rows);
                if p.info_rows > 0 && needed <= content.height {
                    p.info_rows
                } else {
                    0
                }
            }
        }
    } else {
        0
    };
    let show_info_column =
        p.show_info_panel && inline_rows == 0 && p.info_position != InfoPanelPosition::Inline;

    // At width ≥ three_panel_min_cols (default 120), support optional info /
    // tasks / file-viewer columns.
    if area.width >= settings.three_panel_min_cols
        && (show_info_column || p.show_tasks_panel || p.show_file_viewer)
    {
        return three_panel_layout(
            &bands,
            content,
            show_info_column,
            p.show_tasks_panel,
            p.show_file_viewer,
            auto_rows,
            inline_rows,
        );
    }

    two_panel_layout(&bands, content, auto_rows, inline_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    /// Positional wrapper pinning the classic `Column` position, so the
    /// long-standing behavioral tests below read unchanged and keep guarding
    /// the pre-inline layout exactly.
    #[allow(clippy::too_many_arguments)]
    fn layout(
        area: Rect,
        show_info_panel: bool,
        show_tasks_panel: bool,
        show_file_viewer: bool,
        show_global_search: bool,
        show_automations_pane: bool,
        automation_count: usize,
        show_status_row: bool,
    ) -> PanelAreas {
        compute_layout(
            area,
            &LayoutParams {
                show_info_panel,
                info_position: InfoPanelPosition::Column,
                show_tasks_panel,
                show_file_viewer,
                show_global_search,
                show_automations_pane,
                automation_count,
                show_status_row,
                ..Default::default()
            },
        )
    }

    /// Params for the inline/auto placement tests: info panel shown with the
    /// automations pane on, everything else off.
    fn inline_params(
        position: InfoPanelPosition,
        info_rows: u16,
        session_rows: u16,
    ) -> LayoutParams {
        LayoutParams {
            show_info_panel: true,
            info_position: position,
            info_rows,
            session_rows,
            show_automations_pane: true,
            ..Default::default()
        }
    }

    #[test]
    fn narrow_terminal_hides_left_panel() {
        let areas = layout(area(79, 24), false, false, false, false, true, 0, false);
        assert!(areas.left_panel.is_none());
        assert!(areas.info_panel.is_none());
        assert!(areas.file_viewer.is_none());
    }

    #[test]
    fn normal_width_shows_two_panels() {
        let areas = layout(area(100, 24), false, false, false, false, true, 0, false);
        assert!(areas.left_panel.is_some());
        assert!(areas.info_panel.is_none());
        assert!(areas.file_viewer.is_none());
    }

    #[test]
    fn wide_terminal_with_info_panel_shows_three_panels() {
        let areas = layout(area(120, 24), true, false, false, false, true, 0, false);
        assert!(areas.left_panel.is_some());
        assert!(areas.info_panel.is_some());
        assert!(areas.file_viewer.is_none());
    }

    #[test]
    fn wide_terminal_without_info_panel_shows_two_panels() {
        let areas = layout(area(120, 24), false, false, false, false, true, 0, false);
        assert!(areas.left_panel.is_some());
        assert!(areas.info_panel.is_none());
        assert!(areas.file_viewer.is_none());
    }

    #[test]
    fn wide_terminal_with_file_viewer_only() {
        let areas = layout(area(160, 24), false, false, true, false, true, 0, false);
        assert!(areas.left_panel.is_some());
        assert!(areas.info_panel.is_none());
        assert!(areas.file_viewer.is_some());
    }

    #[test]
    fn wide_terminal_with_info_and_file_viewer() {
        let areas = layout(area(160, 24), true, false, true, false, true, 0, false);
        assert!(areas.left_panel.is_some());
        assert!(areas.info_panel.is_some());
        assert!(areas.file_viewer.is_some());
        let term = areas.terminal;
        let fv = areas.file_viewer.unwrap();
        assert!(fv.x >= term.x + term.width);
    }

    #[test]
    fn wide_terminal_with_tasks_panel_only() {
        let areas = layout(area(160, 24), false, true, false, false, true, 0, false);
        assert!(areas.left_panel.is_some());
        assert!(areas.info_panel.is_none());
        assert!(areas.tasks_panel.is_some());
        assert!(areas.file_viewer.is_none());
        let term = areas.terminal;
        let tp = areas.tasks_panel.unwrap();
        assert!(tp.x >= term.x + term.width);
    }

    #[test]
    fn tasks_panel_sits_left_of_file_viewer() {
        let areas = layout(area(180, 24), false, true, true, false, true, 0, false);
        let term = areas.terminal;
        let tp = areas.tasks_panel.expect("tasks panel shown");
        let fv = areas.file_viewer.expect("file viewer shown");
        assert!(tp.x >= term.x + term.width, "tasks right of terminal");
        assert!(fv.x >= tp.x + tp.width, "file viewer right of tasks");
    }

    #[test]
    fn tasks_panel_ignored_below_120_cols() {
        let areas = layout(area(119, 24), false, true, false, false, true, 0, false);
        assert!(areas.tasks_panel.is_none());
    }

    #[test]
    fn global_search_popup_absent_by_default() {
        let areas = layout(area(120, 40), false, false, false, false, true, 0, false);
        assert!(areas.global_search.is_none());
    }

    #[test]
    fn global_search_popup_is_centered_in_the_upper_third() {
        let areas = layout(area(120, 40), false, false, false, true, true, 0, false);
        let popup = areas.global_search.expect("popup shown when active");
        // 120 cols → 60% = 72, within the [50, 90] clamp; centered.
        assert_eq!(popup.width, 72);
        assert_eq!(popup.x, (120 - 72) / 2);
        // Top edge in the upper third, JetBrains-style.
        assert_eq!(popup.y, 40 / 6);
        assert_eq!(popup.height, GLOBAL_SEARCH_POPUP_ROWS);
        // Floats over the content: never touches the footer row.
        assert!(popup.y + popup.height < areas.footer.y);
    }

    #[test]
    fn global_search_popup_does_not_shrink_content() {
        let without = layout(area(120, 40), false, false, false, false, true, 0, false).terminal;
        let with = layout(area(120, 40), false, false, false, true, true, 0, false).terminal;
        // The popup floats: every panel keeps its size (and the session PTYs
        // behind it never resize when the search opens).
        assert_eq!(without, with);
    }

    #[test]
    fn global_search_popup_width_is_clamped() {
        // Ultrawide: capped at the max width, still centered.
        let wide = layout(area(300, 40), false, false, false, true, true, 0, false)
            .global_search
            .unwrap();
        assert_eq!(wide.width, GLOBAL_SEARCH_POPUP_MAX_WIDTH);
        assert_eq!(wide.x, (300 - GLOBAL_SEARCH_POPUP_MAX_WIDTH) / 2);
        // Narrow: the min-width clamp caps at the full terminal width.
        let narrow = layout(area(45, 40), false, false, false, true, true, 0, false)
            .global_search
            .unwrap();
        assert_eq!(narrow.width, 45);
        assert_eq!(narrow.x, 0);
    }

    #[test]
    fn global_search_popup_clamps_to_short_terminals() {
        let areas = layout(area(120, 12), false, false, false, true, true, 0, false);
        let popup = areas.global_search.expect("popup shown");
        // Shorter than the full popup: clamp the height, keep the footer row.
        assert!(popup.height < GLOBAL_SEARCH_POPUP_ROWS);
        assert!(popup.y + popup.height < 12);
    }

    #[test]
    fn global_search_popup_clears_the_status_row_when_both_show() {
        // Short terminal + an active status message: the status row renders
        // over the popup, so the popup must reserve it (footer + status) and
        // never extend onto the status row's line.
        let areas = layout(area(120, 14), false, false, false, true, true, 0, true);
        let popup = areas.global_search.expect("popup shown");
        let status = areas.status_message.expect("status row shown");
        assert!(
            popup.y + popup.height <= status.y,
            "popup {popup:?} must end at or above the status row {status:?}"
        );
    }

    #[test]
    fn status_row_absent_by_default() {
        let areas = layout(area(120, 40), false, false, false, false, true, 0, false);
        assert!(areas.status_message.is_none());
        assert_eq!(areas.footer.height, 1);
    }

    #[test]
    fn status_row_present_when_active() {
        let areas = layout(area(120, 40), false, false, false, false, true, 0, true);
        let row = areas
            .status_message
            .expect("row shown when a message is active");
        // Full width, one row, docked directly above the footer.
        assert_eq!(row.width, 120);
        assert_eq!(row.x, 0);
        assert_eq!(row.height, 1);
        assert_eq!(row.y + row.height, areas.footer.y);
    }

    #[test]
    fn status_row_shrinks_content_by_one() {
        let without = layout(area(120, 40), false, false, false, false, true, 0, false).terminal;
        let with = layout(area(120, 40), false, false, false, false, true, 0, true).terminal;
        assert_eq!(without.height - with.height, 1);
    }

    #[test]
    fn status_row_unaffected_by_global_search_popup() {
        // Popup active + status message showing: the floating popup leaves the
        // status row pinned above the footer, exactly where it is without it.
        let with_popup = layout(area(120, 40), false, false, false, true, true, 0, true);
        let without_popup = layout(area(120, 40), false, false, false, false, true, 0, true);
        let sm = with_popup.status_message.expect("status row shown");
        assert_eq!(sm, without_popup.status_message.unwrap());
        assert_eq!(
            sm.y + sm.height,
            with_popup.footer.y,
            "status row sits above the footer"
        );
    }

    #[test]
    fn header_and_footer_are_one_line() {
        let areas = layout(area(100, 24), false, false, false, false, true, 0, false);
        assert_eq!(areas.header.height, 1);
        assert_eq!(areas.footer.height, 1);
    }

    #[test]
    fn compact_mode_hides_header_below_20_rows() {
        let areas = layout(area(100, 19), false, false, false, false, true, 0, false);
        assert_eq!(areas.header.height, 0);
        assert_eq!(areas.footer.height, 1);
        assert!(areas.left_panel.is_some());
    }

    #[test]
    fn header_returns_at_20_rows() {
        let areas = layout(area(100, 20), false, false, false, false, true, 0, false);
        assert_eq!(areas.header.height, 1);
    }

    #[test]
    fn info_panel_ignored_below_120_cols() {
        let areas = layout(area(119, 24), true, false, false, false, true, 0, false);
        assert!(areas.info_panel.is_none());
    }

    #[test]
    fn file_viewer_ignored_below_120_cols() {
        let areas = layout(area(119, 24), false, false, true, false, true, 0, false);
        assert!(areas.file_viewer.is_none());
    }

    fn terminal_inner(width: u16, height: u16, show_info: bool) -> (u16, u16) {
        use ratatui::widgets::{Block, Borders};
        let terminal = layout(
            area(width, height),
            show_info,
            false,
            false,
            false,
            true,
            0,
            false,
        )
        .terminal;
        let inner = Block::default().borders(Borders::ALL).inner(terminal);
        (inner.height, inner.width)
    }

    #[test]
    fn two_panel_terminal_width_at_160_cols() {
        let (rows, cols) = terminal_inner(160, 40, false);
        assert_eq!(cols, 118);
        assert_eq!(rows, 36);
    }

    #[test]
    fn two_panel_terminal_width_at_80_cols() {
        let (rows, cols) = terminal_inner(80, 24, false);
        assert_eq!(cols, 58);
        assert_eq!(rows, 20);
    }

    #[test]
    fn three_panel_terminal_width_at_160_cols() {
        // 160 cols, list(18%)+info(15%)=33% reserved, terminal ≈ 67% (107) - 2 borders
        let (rows, cols) = terminal_inner(160, 40, true);
        assert!((100..=110).contains(&cols));
        assert_eq!(rows, 36);
    }

    #[test]
    fn narrow_terminal_uses_full_width() {
        let (rows, cols) = terminal_inner(60, 24, false);
        assert_eq!(cols, 58);
        assert_eq!(rows, 20);
    }

    #[test]
    fn automations_pane_present_even_when_empty() {
        // Zero automations still get a minimum-height pane (so it's discoverable).
        let areas = layout(area(100, 24), false, false, false, false, true, 0, false);
        assert!(areas.left_panel.is_some());
        let autos = areas.automations_panel.expect("empty pane still shown");
        assert_eq!(autos.height, AUTOMATIONS_PANE_MIN_ROWS);
    }

    #[test]
    fn automations_pane_appears_below_sessions() {
        let areas = layout(area(100, 30), false, false, false, false, true, 2, false);
        let sessions = areas.left_panel.unwrap();
        let autos = areas.automations_panel.expect("automations pane shown");
        assert_eq!(sessions.x, autos.x);
        assert_eq!(sessions.width, autos.width);
        assert_eq!(autos.y, sessions.y + sessions.height);
        // 2 automations + 2 border rows = 4 rows tall.
        assert_eq!(autos.height, 4);
        assert!(sessions.height >= SESSIONS_MIN_ROWS);
    }

    #[test]
    fn automations_pane_height_is_capped() {
        let areas = layout(area(100, 60), false, false, false, false, true, 50, false);
        assert_eq!(
            areas.automations_panel.unwrap().height,
            AUTOMATIONS_PANE_MAX_ROWS
        );
    }

    #[test]
    fn automations_pane_hidden_when_feature_disabled() {
        let with = layout(area(100, 30), false, false, false, false, true, 2, false);
        let without = layout(area(100, 30), false, false, false, false, false, 2, false);
        assert!(without.automations_panel.is_none());
        // The session list absorbs the whole left column.
        let full = without.left_panel.unwrap();
        let split = with.left_panel.unwrap();
        assert_eq!(
            full.height,
            split.height + with.automations_panel.unwrap().height
        );
    }

    #[test]
    fn automations_pane_hidden_when_column_too_short() {
        // Content height ≈ 4 rows leaves no room for both lists.
        let areas = layout(area(100, 6), false, false, false, false, true, 3, false);
        assert!(areas.left_panel.is_some());
        assert!(areas.automations_panel.is_none());
    }

    // ── info-pane placement (`info_panel_position`) ──

    #[test]
    fn auto_inlines_info_under_sessions_when_it_fits() {
        // 100×40: no dedicated column below 120 cols, but the left column
        // (38 content rows) holds sessions (6) + automations (3) + info (12).
        let areas = compute_layout(
            area(100, 40),
            &inline_params(InfoPanelPosition::Auto, 12, 6),
        );
        let sessions = areas.left_panel.unwrap();
        let autos = areas.automations_panel.expect("automations pane shown");
        let info = areas.info_panel.expect("info pane inlined");
        assert_eq!(info.x, sessions.x);
        assert_eq!(info.width, sessions.width);
        assert_eq!(info.height, 12);
        // Bottom-docked: sessions | automations | info.
        assert_eq!(autos.y, sessions.y + sessions.height);
        assert_eq!(info.y, autos.y + autos.height);
        // The terminal keeps the full remaining width — no info column carved.
        assert_eq!(sessions.width + areas.terminal.width, 100);
    }

    #[test]
    fn auto_falls_back_to_column_when_too_short() {
        // 22 content rows can't hold sessions (10) + automations (3) + info
        // (12), so at three-panel widths the classic column returns.
        let areas = compute_layout(
            area(160, 24),
            &inline_params(InfoPanelPosition::Auto, 12, 10),
        );
        let sessions = areas.left_panel.unwrap();
        let info = areas.info_panel.expect("column shown");
        assert!(
            info.x >= sessions.x + sessions.width,
            "info is its own column"
        );
        assert_eq!(info.y, sessions.y, "column spans the full content height");
    }

    #[test]
    fn auto_hides_info_when_neither_dock_fits() {
        // Too narrow for the column and too short to inline.
        let areas = compute_layout(
            area(100, 12),
            &inline_params(InfoPanelPosition::Auto, 12, 10),
        );
        assert!(areas.info_panel.is_none());
    }

    #[test]
    fn auto_fit_accounts_for_the_automations_pane() {
        // sessions (8) + automations (3) + info (9) = 20 > 18 content rows →
        // column; with the automations pane off the same inputs fit → inline.
        let mut p = inline_params(InfoPanelPosition::Auto, 9, 8);
        let areas = compute_layout(area(160, 20), &p);
        let sessions = areas.left_panel.unwrap();
        assert!(areas.info_panel.unwrap().x >= sessions.x + sessions.width);

        p.show_automations_pane = false;
        let areas = compute_layout(area(160, 20), &p);
        let sessions = areas.left_panel.unwrap();
        assert_eq!(areas.info_panel.unwrap().x, sessions.x);
    }

    #[test]
    fn inline_position_squeezes_sessions_to_minimum() {
        // Forced inline on a short column: the info pane keeps its rows even
        // though the session list needs more than what's left.
        let areas = compute_layout(
            area(100, 20),
            &inline_params(InfoPanelPosition::Inline, 10, 12),
        );
        let sessions = areas.left_panel.unwrap();
        let info = areas.info_panel.expect("inline pane forced");
        assert_eq!(info.height, 10);
        assert!(sessions.height < 12, "session list gave up rows");
        assert!(sessions.height >= SESSIONS_MIN_ROWS);
    }

    #[test]
    fn inline_position_never_uses_the_column() {
        // Even at three-panel widths with too little room for the full
        // content, `inline` clamps into the left column instead of falling
        // back to the dedicated column.
        let areas = compute_layout(
            area(160, 24),
            &inline_params(InfoPanelPosition::Inline, 30, 10),
        );
        let sessions = areas.left_panel.unwrap();
        let info = areas.info_panel.expect("clamped inline pane");
        assert_eq!(info.x, sessions.x);
        assert!(info.height < 30, "pane clamped to the column");
    }

    #[test]
    fn inline_pane_dropped_below_minimum_rows() {
        // The clamp leaves under INFO_PANE_MIN_ROWS → no useless sliver.
        let areas = compute_layout(
            area(100, 8),
            &inline_params(InfoPanelPosition::Inline, 10, 4),
        );
        assert!(areas.info_panel.is_none());
    }

    #[test]
    fn inline_info_coexists_with_right_columns() {
        // Inline dock in the left column while the tasks column is open.
        let p = LayoutParams {
            show_tasks_panel: true,
            ..inline_params(InfoPanelPosition::Auto, 10, 5)
        };
        let areas = compute_layout(area(160, 40), &p);
        let sessions = areas.left_panel.unwrap();
        let info = areas.info_panel.expect("inlined");
        assert_eq!(info.x, sessions.x);
        assert!(areas.tasks_panel.is_some());
    }
}
