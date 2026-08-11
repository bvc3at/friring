// Modal state management for Friring TUI: a single discriminated `Modal` enum
// makes invalid states (two modals open at once) unrepresentable.

use std::collections::HashSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::session::theme_config::ThemeEntry;
use crate::storage::DeletedSessionInfo;

// ── TextInput Helper ────────────────────────────────────────────────────────

/// Feed pasted `text` to `insert` one char at a time, dropping control
/// characters. When `keep_newlines` is set, `\n` is preserved (multi-line
/// fields); every other control char (including `\r`) is always dropped so a
/// single-line field never gains a line break and `\r\n` collapses to one.
fn insert_pasted(text: &str, keep_newlines: bool, mut insert: impl FnMut(char)) {
    for c in text.chars() {
        if (keep_newlines && c == '\n') || !c.is_control() {
            insert(c);
        }
    }
}

/// Simple text input state with cursor tracking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    buffer: String,
    cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, c: char) {
        let byte_pos = self.byte_offset();
        self.buffer.insert(byte_pos, c);
        self.cursor += 1;
    }

    /// Insert pasted text at the cursor. Control characters (newlines, tabs, …)
    /// are dropped so a single-line input never gains a line break from a paste.
    pub fn insert_str(&mut self, s: &str) {
        insert_pasted(s, false, |c| self.insert(c));
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let byte_pos = self.byte_offset();
            self.buffer.remove(byte_pos);
        }
    }

    pub fn delete(&mut self) {
        let byte_pos = self.byte_offset();
        if byte_pos < self.buffer.len() {
            self.buffer.remove(byte_pos);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        let char_count = self.buffer.chars().count();
        if self.cursor < char_count {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.buffer.chars().count();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, value: &str) {
        self.buffer = value.to_string();
        self.cursor = value.chars().count();
    }

    pub fn value(&self) -> &str {
        &self.buffer
    }

    pub fn cursor_pos(&self) -> usize {
        self.cursor
    }

    /// Convert char-based cursor position to byte offset.
    fn byte_offset(&self) -> usize {
        byte_index(&self.buffer, self.cursor)
    }
}

/// Multi-line text input with a char-indexed cursor over a `\n`-delimited
/// buffer. Backs the task editor's description field. Mirrors [`TextInput`] but
/// adds newline insertion and vertical (line) cursor movement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextArea {
    buffer: String,
    /// Cursor as a char index into `buffer` (each `\n` counts as one char).
    cursor: usize,
}

impl TextArea {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, c: char) {
        let byte_pos = self.byte_offset();
        self.buffer.insert(byte_pos, c);
        self.cursor += 1;
    }

    /// Insert a line break at the cursor.
    pub fn insert_newline(&mut self) {
        self.insert('\n');
    }

    /// Insert pasted text at the cursor, preserving newlines (this is a
    /// multi-line field). `\r` is dropped and other control characters are
    /// skipped so a pasted `\r\n` collapses to a single line break.
    pub fn insert_str(&mut self, s: &str) {
        insert_pasted(s, true, |c| self.insert(c));
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let byte_pos = self.byte_offset();
            self.buffer.remove(byte_pos);
        }
    }

    pub fn delete(&mut self) {
        let byte_pos = self.byte_offset();
        if byte_pos < self.buffer.len() {
            self.buffer.remove(byte_pos);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    /// Move up one line, keeping the column (clamped to the shorter line).
    pub fn move_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line > 0 {
            self.cursor = self.cursor_at(line - 1, col);
        }
    }

    /// Move down one line, keeping the column (clamped to the shorter line).
    pub fn move_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line + 1 < self.buffer.split('\n').count() {
            self.cursor = self.cursor_at(line + 1, col);
        }
    }

    /// Move to the start of the current line.
    pub fn home(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.cursor_at(line, 0);
    }

    /// Move to the end of the current line.
    pub fn end(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.cursor_at(line, usize::MAX);
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, value: &str) {
        self.buffer = value.to_string();
        self.cursor = self.char_count();
    }

    pub fn value(&self) -> &str {
        &self.buffer
    }

    /// Cursor as zero-based `(line, col)` in chars, for drawing the block cursor.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        for c in self.buffer.chars().take(self.cursor) {
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Char index of `(line, col)`, clamping `col` to that line's length and
    /// `line` past the end to the buffer end.
    fn cursor_at(&self, line: usize, col: usize) -> usize {
        let mut idx = 0;
        for (l, text) in self.buffer.split('\n').enumerate() {
            let len = text.chars().count();
            if l == line {
                return idx + col.min(len);
            }
            idx += len + 1;
        }
        self.char_count()
    }

    /// Convert the char-based cursor to a byte offset into `buffer`.
    fn byte_offset(&self) -> usize {
        byte_index(&self.buffer, self.cursor)
    }
}

/// Step to the next/previous field in `fields` relative to `current`, wrapping
/// at both ends. `delta` is `+1` (next) or `-1` (previous). Shared by the
/// automation and task editor forms, which navigate different field enums.
fn cycle_field<F: PartialEq + Copy>(fields: &[F], current: F, delta: isize) -> F {
    if fields.is_empty() {
        return current;
    }
    let idx = fields.iter().position(|f| *f == current).unwrap_or(0);
    let len = fields.len() as isize;
    let next = (idx as isize + delta).rem_euclid(len) as usize;
    fields[next]
}

/// Byte offset of char index `idx` in `s` (end of string when out of range).
/// Shared by both text widgets, which index a `char` cursor into a byte buffer.
fn byte_index(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(i, _)| i).unwrap_or(s.len())
}

/// Delete the chars in `[start, cursor)` (char indices) from `buffer` and park
/// `cursor` at `start`. A no-op when `start >= cursor`. Backs every backward
/// line-edit (`Ctrl+W` word, `Ctrl+U` line) of both text widgets.
fn remove_before_cursor(buffer: &mut String, cursor: &mut usize, start: usize) {
    if start >= *cursor {
        return;
    }
    let (s, e) = (byte_index(buffer, start), byte_index(buffer, *cursor));
    buffer.replace_range(s..e, "");
    *cursor = start;
}

/// Delete the chars in `[cursor, end)` (char indices) from `buffer`, leaving
/// `cursor` in place. A no-op when `end <= cursor`. Backs the forward line-edit
/// (`Ctrl+K` kill to line end) of both text widgets.
fn remove_after_cursor(buffer: &mut String, cursor: usize, end: usize) {
    if end <= cursor {
        return;
    }
    let (s, e) = (byte_index(buffer, cursor), byte_index(buffer, end));
    buffer.replace_range(s..e, "");
}

/// Char index of the start of the word ending at `cursor`: skip any trailing
/// whitespace, then the run of non-whitespace before it. Backs the `Ctrl+W`
/// handlers of both text widgets (newlines count as whitespace).
fn word_start_before(chars: &[char], cursor: usize) -> usize {
    let mut start = cursor;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    start
}

/// Readline-style line editing shared by the single-line [`TextInput`] and the
/// multi-line [`TextArea`], so the Ctrl-chord dispatch lives in one place. The
/// cursor-move and single-char delete chords delegate to each widget's existing
/// inherent methods; only the word/line kills carry their own logic.
pub(crate) trait LineEdit {
    /// Delete the word before the cursor (readline `Ctrl+W`).
    fn delete_word_before(&mut self);
    /// Delete from the cursor to the start of the current line (readline `Ctrl+U`).
    fn kill_to_line_start(&mut self);
    /// Delete from the cursor to the end of the current line (readline `Ctrl+K`).
    fn kill_to_line_end(&mut self);
    /// Move the cursor to the start of the current line (readline `Ctrl+A`).
    fn line_start(&mut self);
    /// Move the cursor to the end of the current line (readline `Ctrl+E`).
    fn line_end(&mut self);
    /// Move the cursor one char left (readline `Ctrl+B`).
    fn cursor_left(&mut self);
    /// Move the cursor one char right (readline `Ctrl+F`).
    fn cursor_right(&mut self);
    /// Delete the char under the cursor (readline `Ctrl+D`).
    fn delete_forward(&mut self);
    /// Delete the char before the cursor (readline `Ctrl+H`, like Backspace).
    fn delete_backward(&mut self);
}

/// The `LineEdit` methods identical for both text widgets: the word kill plus
/// the cursor-move / single-char deletes that delegate to each widget's
/// inherent methods. Defined once here so the single-line and multi-line impls
/// share one copy (the line kills, which differ per widget, stay inline). Both
/// widgets expose the `buffer`/`cursor` fields and inherent methods this needs.
macro_rules! line_edit_shared {
    () => {
        fn delete_word_before(&mut self) {
            let chars: Vec<char> = self.buffer.chars().collect();
            let start = word_start_before(&chars, self.cursor);
            remove_before_cursor(&mut self.buffer, &mut self.cursor, start);
        }
        fn line_start(&mut self) {
            self.home();
        }
        fn line_end(&mut self) {
            self.end();
        }
        fn cursor_left(&mut self) {
            self.move_left();
        }
        fn cursor_right(&mut self) {
            self.move_right();
        }
        fn delete_forward(&mut self) {
            self.delete();
        }
        fn delete_backward(&mut self) {
            self.backspace();
        }
    };
}

impl LineEdit for TextInput {
    line_edit_shared!();

    /// For a single-line input this clears everything before the cursor.
    fn kill_to_line_start(&mut self) {
        remove_before_cursor(&mut self.buffer, &mut self.cursor, 0);
    }

    /// For a single-line input this clears everything from the cursor onward.
    fn kill_to_line_end(&mut self) {
        let end = self.buffer.chars().count();
        remove_after_cursor(&mut self.buffer, self.cursor, end);
    }
}

impl LineEdit for TextArea {
    line_edit_shared!();

    fn kill_to_line_start(&mut self) {
        let (line, _) = self.cursor_line_col();
        let line_start = self.cursor_at(line, 0);
        remove_before_cursor(&mut self.buffer, &mut self.cursor, line_start);
    }

    /// Kill to the end of the current line; at the line end (with text below)
    /// this consumes the trailing newline, joining the next line — matching
    /// readline/emacs `Ctrl+K`.
    fn kill_to_line_end(&mut self) {
        let (line, _) = self.cursor_line_col();
        let mut end = self.cursor_at(line, usize::MAX);
        if end == self.cursor && self.cursor < self.char_count() {
            end = self.cursor + 1;
        }
        remove_after_cursor(&mut self.buffer, self.cursor, end);
    }
}

/// Apply a readline `Ctrl`+letter line-edit chord to `field`, giving modal text
/// fields the same emacs-style editing as a terminal:
///
/// - `Ctrl+A` / `Ctrl+E` — move to line start / end
/// - `Ctrl+B` / `Ctrl+F` — move one char left / right
/// - `Ctrl+H` / `Ctrl+D` — delete the char before / under the cursor
/// - `Ctrl+W` — delete the word before the cursor
/// - `Ctrl+U` / `Ctrl+K` — kill to line start / end
///
/// Returns `true` when `code` is a `Ctrl`+letter chord — handled or swallowed
/// (any unmapped letter) — so the caller treats it as consumed and never
/// inserts the bare letter as text. `Ctrl` with a non-letter key (arrows,
/// Home/End) returns `false` so normal cursor handling still applies.
pub(crate) fn apply_ctrl_line_edit(
    field: &mut impl LineEdit,
    code: KeyCode,
    mods: KeyModifiers,
) -> bool {
    if !mods.contains(KeyModifiers::CONTROL) {
        return false;
    }
    let KeyCode::Char(c) = code else {
        return false; // Ctrl+<non-letter>: defer to normal handling
    };
    match c.to_ascii_lowercase() {
        'a' => field.line_start(),
        'e' => field.line_end(),
        'b' => field.cursor_left(),
        'f' => field.cursor_right(),
        'h' => field.delete_backward(),
        'd' => field.delete_forward(),
        'w' => field.delete_word_before(),
        'u' => field.kill_to_line_start(),
        'k' => field.kill_to_line_end(),
        _ => {} // other Ctrl+letter: swallow (never insert)
    }
    true
}

/// Apply a key to a multi-line [`TextArea`] field: `Enter` inserts a newline,
/// `Up`/`Down` move within the text, and the rest edit / move the cursor.
/// Returns `true` when the key was consumed. `Esc` (cancel) and `Tab`/`BackTab`
/// (field navigation) are deliberately *not* handled here — they belong to the
/// owning editor — and `Ctrl` chords should be routed through
/// [`apply_ctrl_line_edit`] first. Shared by the automation `Prompt` and task
/// `Description` fields so their editing behavior can't drift apart.
fn handle_textarea_key(area: &mut TextArea, code: KeyCode) -> bool {
    match code {
        KeyCode::Enter => area.insert_newline(),
        KeyCode::Up => area.move_up(),
        KeyCode::Down => area.move_down(),
        KeyCode::Char(c) => area.insert(c),
        KeyCode::Backspace => area.backspace(),
        KeyCode::Delete => area.delete(),
        KeyCode::Left => area.move_left(),
        KeyCode::Right => area.move_right(),
        KeyCode::Home => area.home(),
        KeyCode::End => area.end(),
        _ => return false,
    }
    true
}

/// Apply a text-editing key (insert/backspace/delete/cursor move + readline
/// `Ctrl+W`/`Ctrl+U`) to the currently focused field, if any. Returns `true`
/// when `code`+`mods` was a text-editing key (whether or not a field was
/// focused), so editor key handlers can share one implementation across the
/// automation and task forms. A `Ctrl`+letter chord is consumed here (W/U edit,
/// the rest swallowed) so no literal control-letter leaks into the field; a
/// `Ctrl`+<non-letter> chord (arrows, Home/End) falls through to normal handling.
pub(super) fn apply_text_input_key(
    field: Option<&mut TextInput>,
    code: KeyCode,
    mods: KeyModifiers,
) -> bool {
    if mods.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(_) = code {
            if let Some(f) = field {
                apply_ctrl_line_edit(f, code, mods);
            }
            return true;
        }
        // Ctrl+<non-letter> (arrows, Home/End): fall through to normal handling.
    }
    if !is_text_input_key(code) {
        return false;
    }
    if let Some(f) = field {
        apply_text_edit_op(f, code);
    }
    true
}

/// Whether `code` is a text-editing key handled by [`apply_text_input_key`]
/// (insert/backspace/delete/cursor move).
fn is_text_input_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char(_)
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
    )
}

/// Apply a single text-editing key to a focused field. `code` must be a key for
/// which [`is_text_input_key`] returns `true`; anything else is a no-op.
fn apply_text_edit_op(f: &mut TextInput, code: KeyCode) {
    match code {
        KeyCode::Char(c) => f.insert(c),
        KeyCode::Backspace => f.backspace(),
        KeyCode::Delete => f.delete(),
        KeyCode::Left => f.move_left(),
        KeyCode::Right => f.move_right(),
        KeyCode::Home => f.home(),
        KeyCode::End => f.end(),
        _ => {}
    }
}

// ── Modal State Structs ────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct BranchSelectorModal {
    /// Selection cursor in `filter`'s *filtered* row space, not directly into
    /// `branches` (the two coincide while no query is typed).
    pub index: usize,
    pub branches: Vec<String>,
    /// Type-to-filter query over `branches` (printable keys edit it).
    pub(crate) filter: crate::fuzzy::FuzzyFilter,
    /// The branch list is still being read off-thread (ADR-P12): the modal
    /// opened instantly with a placeholder row and `Enter` is inert until the
    /// background load delivers.
    pub loading: bool,
}

/// Picker shown when a Ctrl+S sync targets a repo with more than one remote:
/// choose which remote to rebase onto. One picker per multi-remote repo; the
/// queue of repos still awaiting a choice rides on the parked sync run
/// ([`PendingSyncRun`](super::sync_state::PendingSyncRun)), not the modal.
#[derive(Debug, Clone, Default)]
pub struct SyncBasePickerModal {
    /// Display name of the repo the choice applies to (shown in the title).
    pub repo_name: String,
    pub remotes: Vec<String>,
    pub index: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WorktreeNameModal {
    pub name: TextInput,
}

#[derive(Debug, Clone, Default)]
pub struct SessionNameModal {
    pub name: TextInput,
    /// Optional workspace-dir field for multi-repo local spawns: `None` =
    /// hidden (the default id-derived workspace applies). `Ctrl+O` shows /
    /// hides it, `Tab` moves focus between the two fields.
    pub workspace_dir: Option<TextInput>,
    /// Whether the (shown) workspace-dir field has keyboard focus.
    pub workspace_focused: bool,
}

#[derive(Debug, Clone)]
pub struct ThemePickerModal {
    /// Selection cursor in *filtered* space — an index into
    /// [`ThemePickerModal::matches`], not into the full entry list. Every
    /// consumer resolves it through `matches` so a typed filter can never
    /// apply the wrong theme.
    pub index: usize,
    /// Palette active when the picker opened. The picker live-previews by
    /// mutating the global palette as the selection moves, so cancelling
    /// (`Esc`) restores this snapshot; only confirming (`Enter`) persists.
    pub original: crate::session::ThemePalette,
    /// The open filter sub-mode, entered with `/` — `None` in normal
    /// navigation mode, where `j`/`k` select like every other picker.
    ///
    /// With 36 built-ins plus custom themes the list is far taller than any
    /// terminal, so a query narrows it; but it sits behind `/` (mirroring the
    /// file viewer's and code review's find) rather than swallowing every
    /// letter, so the picker's keys stay consistent with its siblings. While
    /// `Some`, letters append to the query and `Esc` closes the sub-mode.
    pub filter: Option<TextInput>,
    /// Indices into the full entry list that match `filter`, in list order.
    /// Recomputed by [`ThemePickerModal::refilter`] on every query change.
    pub matches: Vec<usize>,
}

impl ThemePickerModal {
    /// The active query, or `""` when the filter sub-mode is closed.
    pub fn filter_query(&self) -> &str {
        self.filter.as_ref().map_or("", |f| f.value())
    }

    /// Case-insensitive substring match over a theme's display name and its
    /// stable id, so both "rose" and "rose-pine-dawn" find the same entry.
    /// `needle` must already be lowercased. Both sides are lowercased: built-in
    /// ids are all lowercase, but a custom theme may name itself anything.
    fn is_match(entry: &ThemeEntry, needle: &str) -> bool {
        entry.display_name.to_lowercase().contains(needle)
            || entry.name.to_lowercase().contains(needle)
    }

    /// All entry indices matching the current query (every index when empty).
    pub fn compute_matches(entries: &[ThemeEntry], filter: &str) -> Vec<usize> {
        let needle = filter.trim().to_lowercase();
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| needle.is_empty() || Self::is_match(e, &needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Recompute `matches` after a query edit, keeping the cursor on the
    /// previously selected *entry* when it survives the filter — so refining a
    /// query never silently previews a different theme. Falls back to the first
    /// match (the caller then previews it).
    pub fn refilter(&mut self, entries: &[ThemeEntry]) {
        let previous = self.selected_entry();
        self.matches = Self::compute_matches(entries, self.filter_query());
        self.index = previous
            .and_then(|entry| self.matches.iter().position(|&i| i == entry))
            .unwrap_or(0);
    }

    /// Open the filter sub-mode (`/`). Idempotent — re-pressing `/` while it
    /// is already open keeps the query rather than clearing it.
    pub fn open_filter(&mut self) {
        self.filter.get_or_insert_with(TextInput::new);
    }

    /// Close the filter sub-mode and restore the full list, keeping the cursor
    /// on the theme it was on so `Esc` never jumps the selection elsewhere.
    pub fn close_filter(&mut self, entries: &[ThemeEntry]) {
        self.filter = None;
        self.refilter(entries);
    }

    /// The full-list entry index under the cursor, or `None` when the filter
    /// matches nothing.
    pub fn selected_entry(&self) -> Option<usize> {
        self.matches.get(self.index).copied()
    }
}

/// What a hard delete would destroy: uncommitted changes and/or unmerged
/// commits across a session's worktree(s). `unknown` means the state could not
/// be determined (a remote-host session or a failed git query), in which case
/// we confirm anyway. Drives whether the confirmation prompt is shown at all
/// and what it lists. See [`DeleteRisk::from_stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteRisk {
    pub dirty: bool,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub untracked: usize,
    pub ahead: usize,
    pub unknown: bool,
}

impl DeleteRisk {
    /// Risk could not be determined (remote session / git error) → confirm.
    pub fn unknown() -> Self {
        DeleteRisk {
            unknown: true,
            ..Default::default()
        }
    }

    /// Reduce per-worktree git stats into a delete risk. `stats[i]` is `None`
    /// when worktree `i` could not be inspected (not a git worktree / git
    /// failed) — any such entry forces `unknown`. Returns `None` (delete
    /// silently) only when every worktree is known-clean: no dirty tree, no
    /// untracked files, no commits ahead, nothing unknown.
    pub fn from_stats(stats: &[Option<crate::session::GitStats>]) -> Option<DeleteRisk> {
        let mut risk = DeleteRisk::default();
        for entry in stats {
            match entry {
                None => risk.unknown = true,
                Some(s) => {
                    risk.dirty |= s.dirty;
                    risk.files_changed += s.files_changed;
                    risk.insertions += s.insertions;
                    risk.deletions += s.deletions;
                    risk.untracked += s.untracked;
                    risk.ahead += s.ahead;
                }
            }
        }
        if risk.unknown || risk.dirty || risk.untracked > 0 || risk.ahead > 0 {
            Some(risk)
        } else {
            None
        }
    }
}

/// Confirmation prompt for a destructive (hard) session delete, shown only when
/// the `soft_delete` feature flag is off **and** the session has work at risk
/// (see [`DeleteRisk`]). Carries the target session so the confirm handler can
/// tear it down without re-resolving the active index.
#[derive(Debug, Clone)]
pub struct ConfirmDeleteModal {
    pub session_id: crate::session::SessionId,
    pub session_name: String,
    pub risk: DeleteRisk,
}

/// Confirmation prompt for a **best-effort** restore of a force-deleted session.
/// Force-delete tore down the worktree directory + tmux window (and any
/// uncommitted work), but the git branch — and its committed history — survives,
/// so restore can reattach it. This modal warns that only committed state is
/// recovered, then restores the carried session on confirm.
#[derive(Debug, Clone)]
pub struct ConfirmRestoreModal {
    pub deleted: DeletedSessionInfo,
}

// ── RestoreSessionsModal ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct RestoreSessionsModal {
    pub list: Vec<DeletedSessionInfo>,
    pub index: usize,
}

// ── AutomationEditorModal ───────────────────────────────────────────────

/// What action a triggered automation performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutomationActionKind {
    /// Paste the prompt into an existing session.
    #[default]
    Send,
    /// Spawn a new session and prompt it.
    Spawn,
    /// Run a shell command headlessly (no session/agent).
    Exec,
}

/// How an automation's schedule is entered in the editor. Cycled with the
/// arrow keys so users never type a cron expression or magic trigger string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriggerKind {
    /// Fire once after a relative delay (e.g. `30m`).
    Once,
    /// Every hour at a chosen minute.
    Hourly,
    /// Every day at a chosen time.
    #[default]
    Daily,
    /// Mon–Fri at a chosen time.
    Weekdays,
    /// A chosen weekday at a chosen time.
    Weekly,
    /// Raw cron expression (power users).
    Cron,
}

impl TriggerKind {
    /// All kinds in cycle order.
    const ALL: [TriggerKind; 6] = [
        TriggerKind::Once,
        TriggerKind::Hourly,
        TriggerKind::Daily,
        TriggerKind::Weekdays,
        TriggerKind::Weekly,
        TriggerKind::Cron,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TriggerKind::Once => "once",
            TriggerKind::Hourly => "hourly",
            TriggerKind::Daily => "daily",
            TriggerKind::Weekdays => "weekdays",
            TriggerKind::Weekly => "weekly",
            TriggerKind::Cron => "cron",
        }
    }

    fn step(self, delta: i32) -> Self {
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0) as i32;
        let len = Self::ALL.len() as i32;
        Self::ALL[(idx + delta).rem_euclid(len) as usize]
    }
}

/// Focusable field in the automation editor. The set shown depends on the
/// current [`TriggerKind`] and action (see `AutomationEditorModal::visible_fields`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutomationField {
    #[default]
    Name,
    /// Trigger-kind selector (cycled with ←/→).
    Trigger,
    /// Relative delay text (Once).
    Delay,
    /// Weekday stepper (Weekly).
    Weekday,
    /// Hour-of-day stepper.
    Hour,
    /// Minute stepper.
    Minute,
    /// Raw cron expression text (Cron).
    CronExpr,
    Timezone,
    Action,
    /// Send action: target-session selector (cycled with ←/→).
    Target,
    Repo,
    Worktree,
    /// Spawn action: base branch a new worktree forks from.
    BaseBranch,
    /// Spawn action: agent selector over the registry (cycled with ←/→).
    Agent,
    /// Spawn action: host selector over `hosts.toml` (cycled with ←/→).
    Host,
    /// Spawn action: reuse one session or spawn a fresh one per fire.
    SessionMode,
    /// Spawn action: comma-separated extra repos (`path[@base]`), each on its
    /// own worktree.
    ExtraRepos,
    /// Spawn action: comma-separated extra directories, attached as-is.
    ExtraDirs,
    /// Exec action: shell command text.
    Command,
    /// Exec action: seconds before the command is killed.
    Timeout,
    /// Which prompt step the `Prompt` field is editing (add/remove/reorder).
    Step,
    /// Settle delay after the current step, before the next one is pasted.
    StepDelay,
    Prompt,
}

/// One prompt step being edited. The `Prompt` field edits
/// [`text`](Self::text) for the currently selected step; `delay` overrides the
/// settle time before the *next* step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptStepDraft {
    pub text: TextArea,
    /// Settle delay in milliseconds, as typed. Empty = the default.
    pub delay: TextInput,
}

impl PromptStepDraft {
    /// A draft holding `text` on the default delay.
    fn from_text(text: &str) -> Self {
        let mut draft = Self::default();
        draft.text.set(text);
        draft
    }
}

/// Editor form for creating or editing an automation.
#[derive(Debug, Clone)]
pub struct AutomationEditorModal {
    /// `Some` when editing an existing automation.
    pub editing_id: Option<i64>,
    pub name: TextInput,
    /// How the schedule is specified.
    pub trigger_kind: TriggerKind,
    /// Relative delay text for `Once` (e.g. `30m`, `2h`, `1h30m`).
    pub delay: TextInput,
    /// Weekday for `Weekly`: 0 = Sunday … 6 = Saturday.
    pub weekday: u32,
    /// Hour-of-day 0–23 for daily/weekdays/weekly.
    pub hour: u32,
    /// Minute 0–59 for hourly/daily/weekdays/weekly.
    pub minute: u32,
    /// Raw cron expression for `Cron`.
    pub cron_expr: TextInput,
    /// Optional IANA timezone.
    pub timezone: TextInput,
    pub action: AutomationActionKind,
    /// Spawn action: repository path.
    pub repo: TextInput,
    /// Spawn action: optional worktree branch.
    pub worktree: TextInput,
    /// Spawn action: base branch a new worktree forks from (empty = `main`).
    pub base_branch: TextInput,
    /// Spawn action: comma-separated extra repos (`path[@base]`), each on its
    /// own worktree off the shared branch.
    pub extra_repos: TextInput,
    /// Spawn action: comma-separated extra directories, attached as-is.
    pub extra_dirs: TextInput,
    /// Spawn action: reuse one session per automation, or spawn a fresh one.
    pub session_mode: crate::session::SpawnSessionMode,
    /// Exec action: the shell command to run.
    pub command: TextInput,
    /// Exec action: kill deadline in seconds, as typed. Empty = the default.
    pub timeout: TextInput,
    /// The ordered prompt steps (Send/Spawn). Always at least one.
    pub steps: Vec<PromptStepDraft>,
    /// Index into `steps` of the step the `Prompt`/`StepDelay` fields edit.
    pub step_index: usize,
    pub enabled: bool,
    pub field: AutomationField,
    /// Send action: the running sessions available as targets (id + display
    /// name), captured at open and cycled with the `Target` field.
    pub sessions: Vec<(crate::session::SessionId, String)>,
    /// Index into `sessions` of the selected Send target.
    pub target_index: usize,
    /// Send action: the target the automation was loaded with. A name target —
    /// or an id whose session isn't running — has no entry in `sessions`, so the
    /// selector falls back to the first session; keeping the original lets an
    /// unrelated edit save without silently retargeting the automation.
    pub original_target: Option<crate::session::SendTarget>,
    /// Whether the user actually moved the `Target` selector. Only then does the
    /// selection win over [`original_target`](Self::original_target).
    pub target_dirty: bool,
    /// Spawn action: selectable agent names, `""` first for "registry default".
    /// Populated by the caller (the registry lives on `App`).
    pub agents: Vec<String>,
    /// Index into `agents` of the selected agent.
    pub agent_index: usize,
    /// Spawn action: selectable host names, `""` first for "local".
    pub hosts: Vec<String>,
    /// Index into `hosts` of the selected host.
    pub host_index: usize,
}

/// Result of feeding a key to the automation editor — lets the caller decide
/// what "save"/"cancel" mean (close an overlay vs. return focus to the pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorOutcome {
    /// Key was consumed; stay in the editor.
    Continue,
    /// `Enter` — the caller should validate + persist the automation.
    Save,
    /// `Esc` — the caller should discard and leave the editor.
    Cancel,
}

impl Default for AutomationEditorModal {
    fn default() -> Self {
        Self {
            editing_id: None,
            name: TextInput::default(),
            trigger_kind: TriggerKind::default(),
            delay: {
                let mut t = TextInput::default();
                t.set("30m");
                t
            },
            weekday: 1, // Monday
            hour: 9,
            minute: 0,
            cron_expr: TextInput::default(),
            timezone: TextInput::default(),
            action: AutomationActionKind::default(),
            repo: TextInput::default(),
            worktree: TextInput::default(),
            base_branch: TextInput::default(),
            extra_repos: TextInput::default(),
            extra_dirs: TextInput::default(),
            session_mode: crate::session::SpawnSessionMode::default(),
            command: TextInput::default(),
            timeout: TextInput::default(),
            steps: vec![PromptStepDraft::default()],
            step_index: 0,
            enabled: true,
            field: AutomationField::default(),
            sessions: Vec::new(),
            target_index: 0,
            original_target: None,
            target_dirty: false,
            // `""` = the registry default; the caller replaces this with the
            // real registry via `set_agents`.
            agents: vec![String::new()],
            agent_index: 0,
            hosts: vec![String::new()],
            host_index: 0,
        }
    }
}

impl AutomationEditorModal {
    /// The fields shown for the current trigger kind + action, in display and
    /// navigation order.
    pub fn visible_fields(&self) -> Vec<AutomationField> {
        use AutomationField::*;
        let mut fields = vec![Name, Trigger];
        match self.trigger_kind {
            TriggerKind::Once => fields.push(Delay),
            TriggerKind::Hourly => fields.push(Minute),
            TriggerKind::Daily | TriggerKind::Weekdays => fields.extend([Hour, Minute]),
            TriggerKind::Weekly => fields.extend([Weekday, Hour, Minute]),
            TriggerKind::Cron => fields.push(CronExpr),
        }
        // Timezone only matters for wall-clock (cron) schedules, not a relative
        // one-shot delay.
        if self.trigger_kind != TriggerKind::Once {
            fields.push(Timezone);
        }
        fields.push(Action);
        match self.action {
            AutomationActionKind::Send => fields.push(Target),
            AutomationActionKind::Spawn => fields.extend([
                Repo,
                Worktree,
                BaseBranch,
                Agent,
                Host,
                SessionMode,
                ExtraRepos,
                ExtraDirs,
            ]),
            AutomationActionKind::Exec => fields.extend([Command, Timeout]),
        }
        // Exec has no prompt (it runs a command, not an agent turn).
        if self.action != AutomationActionKind::Exec {
            fields.push(Step);
            // The settle delay only exists between steps.
            if self.steps.len() > 1 {
                fields.push(StepDelay);
            }
            fields.push(Prompt);
        }
        fields
    }

    /// The prompt step the `Prompt`/`StepDelay` fields currently edit.
    /// `steps` is never empty, so this always resolves.
    pub fn current_step(&self) -> &PromptStepDraft {
        let idx = self.step_index.min(self.steps.len().saturating_sub(1));
        &self.steps[idx]
    }

    fn current_step_mut(&mut self) -> &mut PromptStepDraft {
        let idx = self.step_index.min(self.steps.len().saturating_sub(1));
        &mut self.steps[idx]
    }

    /// The prompt text of the selected step (what the `Prompt` field shows).
    pub fn prompt(&self) -> &TextArea {
        &self.current_step().text
    }

    /// Mutable access to the selected step's prompt text.
    pub fn prompt_mut(&mut self) -> &mut TextArea {
        &mut self.current_step_mut().text
    }

    /// Insert a blank step after the selected one and move to it.
    pub fn add_step(&mut self) {
        let at = (self.step_index + 1).min(self.steps.len());
        self.steps.insert(at, PromptStepDraft::default());
        self.step_index = at;
    }

    /// Remove the selected step. The last remaining step is cleared instead of
    /// removed — an automation always has at least one prompt.
    pub fn remove_step(&mut self) {
        if self.steps.len() == 1 {
            self.steps[0] = PromptStepDraft::default();
            return;
        }
        self.steps.remove(self.step_index);
        self.step_index = self.step_index.min(self.steps.len() - 1);
    }

    /// Move the selected step one position earlier (`-1`) or later (`+1`),
    /// keeping the selection on it. A no-op at the ends.
    pub fn move_step(&mut self, delta: i32) {
        let target = self.step_index as i32 + delta;
        if target < 0 || target as usize >= self.steps.len() {
            return;
        }
        let target = target as usize;
        self.steps.swap(self.step_index, target);
        self.step_index = target;
    }

    /// The prompt steps as they would be persisted: trimmed text, parsed
    /// per-step delays, and blank trailing steps dropped. Returns a user-facing
    /// error when nothing survives (an all-blank prompt) or a delay was typed
    /// but doesn't parse — a mistyped delay must not silently become "default".
    pub fn build_steps(&self) -> Result<Vec<crate::session::PromptStep>, String> {
        let mut steps: Vec<crate::session::PromptStep> = Vec::new();
        for (i, s) in self.steps.iter().enumerate() {
            if s.text.value().trim().is_empty() {
                continue;
            }
            let delay = s.delay.value().trim();
            let delay_ms = if delay.is_empty() {
                None
            } else {
                Some(delay.parse::<u64>().map_err(|_| {
                    format!(
                        "Step {} delay must be a whole number of milliseconds",
                        i + 1
                    )
                })?)
            };
            steps.push(crate::session::PromptStep {
                text: s.text.value().trim().to_string(),
                delay_ms,
            });
        }
        if steps.is_empty() {
            return Err("Prompt cannot be empty".to_string());
        }
        Ok(steps)
    }

    /// Move focus to the next visible field (wraps).
    pub fn next_field(&mut self) {
        self.field = cycle_field(&self.visible_fields(), self.field, 1);
    }

    /// Move focus to the previous visible field (wraps).
    pub fn prev_field(&mut self) {
        self.field = cycle_field(&self.visible_fields(), self.field, -1);
    }

    /// The focused text field, or `None` for selector/stepper fields (which are
    /// adjusted with ←/→ instead — see [`is_adjustable`](Self::is_adjustable)).
    pub fn active_field_mut(&mut self) -> Option<&mut TextInput> {
        use AutomationField::*;
        Some(match self.field {
            Name => &mut self.name,
            Delay => &mut self.delay,
            CronExpr => &mut self.cron_expr,
            Timezone => &mut self.timezone,
            Repo => &mut self.repo,
            Worktree => &mut self.worktree,
            BaseBranch => &mut self.base_branch,
            ExtraRepos => &mut self.extra_repos,
            ExtraDirs => &mut self.extra_dirs,
            Command => &mut self.command,
            Timeout => &mut self.timeout,
            StepDelay => &mut self.current_step_mut().delay,
            // Prompt is a multi-line `TextArea`, handled explicitly in
            // `handle_key`, not as a single-line `TextInput`.
            Prompt | Trigger | Weekday | Hour | Minute | Action | Target | Agent | Host
            | SessionMode | Step => return None,
        })
    }

    /// Whether the focused field is a selector/stepper adjusted with ←/→/Space
    /// rather than edited as text.
    pub fn is_adjustable(&self) -> bool {
        use AutomationField::*;
        matches!(
            self.field,
            Trigger | Weekday | Hour | Minute | Action | Target | Agent | Host | SessionMode | Step
        )
    }

    /// Adjust the focused selector/stepper by `delta` (−1 for ←, +1 for →/Space).
    pub fn adjust(&mut self, delta: i32) {
        use AutomationField::*;
        match self.field {
            Trigger => self.trigger_kind = self.trigger_kind.step(delta),
            Action => self.toggle_action(),
            Weekday => self.weekday = wrap_add(self.weekday, delta, 7),
            Hour => self.hour = wrap_add(self.hour, delta, 24),
            Minute => self.minute = wrap_add(self.minute, delta, 60),
            Target => {
                self.target_index = wrap_index(self.target_index, delta, self.sessions.len());
                self.target_dirty = true;
            }
            Agent => self.agent_index = wrap_index(self.agent_index, delta, self.agents.len()),
            Host => self.host_index = wrap_index(self.host_index, delta, self.hosts.len()),
            SessionMode => {
                self.session_mode = match self.session_mode {
                    crate::session::SpawnSessionMode::Reuse => {
                        crate::session::SpawnSessionMode::Fresh
                    }
                    crate::session::SpawnSessionMode::Fresh => {
                        crate::session::SpawnSessionMode::Reuse
                    }
                }
            }
            // ←/→ walk the step list; adding/removing/reordering is on the
            // letter chords in `handle_step_key`.
            Step => self.step_index = wrap_index(self.step_index, delta, self.steps.len()),
            _ => {}
        }
    }

    /// Step chords on the `Step` field: `n` adds a step after the current one,
    /// `d` deletes it, `[`/`]` move it earlier/later. Returns whether the key was
    /// consumed. Safe to bind letters here — `Step` is a selector, so nothing
    /// types into it.
    fn handle_step_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('n') => self.add_step(),
            KeyCode::Char('d') => self.remove_step(),
            KeyCode::Char('[') => self.move_step(-1),
            KeyCode::Char(']') => self.move_step(1),
            _ => return false,
        }
        true
    }

    /// Feed a key to the editor, mutating field state. Returns whether the caller
    /// should save (`Ctrl+S`, or `Enter` on any non-prompt field), cancel
    /// (`Esc`), or keep editing. On the multi-line `Prompt` field `Enter` inserts
    /// a newline instead of saving. Shared by the centered overlay (Ctrl+P) and
    /// the in-pane editor so both behave identically.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> EditorOutcome {
        // Ctrl+S is the universal save: the multi-line Prompt field needs a save
        // path that isn't Enter (Enter inserts a newline there).
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            return EditorOutcome::Save;
        }
        // Ctrl+E toggles enabled from any field. Lifted above the Prompt branch
        // (which swallows every Ctrl chord) so it still works while editing it.
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('e') | KeyCode::Char('E'))
        {
            self.enabled = !self.enabled;
            return EditorOutcome::Continue;
        }

        // The Prompt is a multi-line `TextArea`: Enter inserts a newline, Up/Down
        // move within the text, and Ctrl+letter chords edit (or are swallowed)
        // rather than leaking literal letters. Mirrors the task editor's
        // description field.
        if self.field == AutomationField::Prompt {
            if apply_ctrl_line_edit(&mut self.current_step_mut().text, code, mods) {
                return EditorOutcome::Continue;
            }
            match code {
                KeyCode::Esc => return EditorOutcome::Cancel,
                KeyCode::Tab => self.next_field(),
                KeyCode::BackTab => self.prev_field(),
                _ => {
                    let step = self.current_step_mut();
                    handle_textarea_key(&mut step.text, code);
                }
            }
            return EditorOutcome::Continue;
        }

        // Selector/stepper fields (trigger, weekday, hour, minute, action,
        // target, agent, host, session mode, step) are adjusted with ←/→/Space;
        // text fields edit as usual.
        let adjustable = self.is_adjustable();
        match code {
            KeyCode::Esc => return EditorOutcome::Cancel,
            KeyCode::Enter => return EditorOutcome::Save,
            KeyCode::Tab | KeyCode::Down => self.next_field(),
            KeyCode::BackTab | KeyCode::Up => self.prev_field(),
            KeyCode::Left if adjustable => self.adjust(-1),
            KeyCode::Right | KeyCode::Char(' ') if adjustable => self.adjust(1),
            other if self.field == AutomationField::Step && self.handle_step_key(other) => {}
            other => {
                apply_text_input_key(self.active_field_mut(), other, mods);
            }
        }
        EditorOutcome::Continue
    }

    /// Populate the available Send targets and select `selected` (falling back to
    /// the first session when it isn't present).
    pub fn set_target_sessions(
        &mut self,
        sessions: Vec<(crate::session::SessionId, String)>,
        selected: Option<crate::session::SessionId>,
    ) {
        self.target_index = selected
            .and_then(|id| sessions.iter().position(|(sid, _)| *sid == id))
            .unwrap_or(0);
        self.sessions = sessions;
    }

    /// The currently selected Send target (id + display name), if any sessions
    /// are available.
    pub fn selected_target(&self) -> Option<&(crate::session::SessionId, String)> {
        self.sessions.get(self.target_index)
    }

    /// Populate the agent selector from the registry and select `selected`.
    ///
    /// A leading `""` entry is the "registry default" choice. An agent the row
    /// still names but the registry no longer has is appended rather than
    /// silently reset, so opening the editor never rewrites the stored value
    /// behind the user's back — saving is what rejects it.
    pub fn set_agents(&mut self, mut names: Vec<String>, selected: Option<&str>) {
        names.insert(0, String::new());
        if let Some(sel) = selected.filter(|s| !s.is_empty()) {
            if !names.iter().any(|n| n == sel) {
                names.push(sel.to_string());
            }
        }
        self.agent_index = selected
            .and_then(|sel| names.iter().position(|n| n == sel))
            .unwrap_or(0);
        self.agents = names;
    }

    /// The selected agent name, or `None` for the registry default.
    pub fn selected_agent(&self) -> Option<&str> {
        self.agents
            .get(self.agent_index)
            .map(String::as_str)
            .filter(|n| !n.is_empty())
    }

    /// Populate the host selector from `hosts.toml` and select `selected`. A
    /// leading `""` entry is "local"; an unknown stored host is kept the same
    /// way [`set_agents`](Self::set_agents) keeps an unknown agent.
    pub fn set_hosts(&mut self, mut names: Vec<String>, selected: Option<&str>) {
        names.insert(0, String::new());
        if let Some(sel) = selected.filter(|s| !s.is_empty()) {
            if !names.iter().any(|n| n == sel) {
                names.push(sel.to_string());
            }
        }
        self.host_index = selected
            .and_then(|sel| names.iter().position(|n| n == sel))
            .unwrap_or(0);
        self.hosts = names;
    }

    /// The selected host name, or `None` for a local spawn.
    pub fn selected_host(&self) -> Option<&str> {
        self.hosts
            .get(self.host_index)
            .map(String::as_str)
            .filter(|n| !n.is_empty())
    }

    /// Cycle through Send → Spawn → Exec actions.
    pub fn toggle_action(&mut self) {
        self.action = match self.action {
            AutomationActionKind::Send => AutomationActionKind::Spawn,
            AutomationActionKind::Spawn => AutomationActionKind::Exec,
            AutomationActionKind::Exec => AutomationActionKind::Send,
        };
    }

    /// The IANA timezone the user entered, if any.
    pub fn timezone(&self) -> Option<String> {
        let tz = self.timezone.value().trim();
        (!tz.is_empty()).then(|| tz.to_string())
    }

    /// Build the [`AutomationSchedule`] described by the current fields, relative
    /// to `now` (used for the `Once` delay). Returns a user-facing error string
    /// for invalid input.
    pub fn build_schedule(&self, now: u64) -> Result<crate::session::AutomationSchedule, String> {
        use crate::session::automation::{parse_duration, preset_to_cron, SchedulePreset};
        use crate::session::AutomationSchedule;
        Ok(match self.trigger_kind {
            TriggerKind::Once => {
                let ms = parse_duration(self.delay.value().trim())
                    .ok_or("Delay must look like 30m, 2h, 1h30m, or 1d")?;
                AutomationSchedule::Once {
                    at: now.saturating_add(ms),
                }
            }
            TriggerKind::Hourly => AutomationSchedule::Cron {
                expr: preset_to_cron(SchedulePreset::Hourly, self.hour, self.minute, self.weekday),
            },
            TriggerKind::Daily => AutomationSchedule::Cron {
                expr: preset_to_cron(SchedulePreset::Daily, self.hour, self.minute, self.weekday),
            },
            TriggerKind::Weekdays => AutomationSchedule::Cron {
                expr: preset_to_cron(
                    SchedulePreset::Weekdays,
                    self.hour,
                    self.minute,
                    self.weekday,
                ),
            },
            TriggerKind::Weekly => AutomationSchedule::Cron {
                expr: preset_to_cron(SchedulePreset::Weekly, self.hour, self.minute, self.weekday),
            },
            TriggerKind::Cron => {
                let expr = self.cron_expr.value().trim();
                if expr.is_empty() {
                    return Err("Cron expression cannot be empty".into());
                }
                AutomationSchedule::Cron {
                    expr: expr.to_string(),
                }
            }
        })
    }

    /// Build an editor pre-filled from an existing automation, reverse-mapping
    /// the schedule back into structured fields where possible. The caller is
    /// responsible for populating the Send target list via
    /// [`set_target_sessions`](Self::set_target_sessions) afterwards.
    pub fn from_automation(auto: &crate::session::Automation) -> Self {
        use crate::session::{AutomationAction, AutomationSchedule};
        let mut m = Self {
            editing_id: Some(auto.id),
            enabled: auto.enabled,
            ..Self::default()
        };
        m.name.set(&auto.name);
        match &auto.schedule {
            AutomationSchedule::Once { at } => {
                m.trigger_kind = TriggerKind::Once;
                let remaining = at.saturating_sub(crate::sync::current_time_millis());
                m.delay.set(&format_duration_short(remaining));
            }
            AutomationSchedule::Cron { expr } => match recognize_cron(expr) {
                Some((kind, hour, minute, weekday)) => {
                    m.trigger_kind = kind;
                    m.hour = hour;
                    m.minute = minute;
                    m.weekday = weekday;
                }
                None => {
                    m.trigger_kind = TriggerKind::Cron;
                    m.cron_expr.set(expr);
                }
            },
        }
        if let Some(tz) = &auto.timezone {
            m.timezone.set(tz);
        }
        m.steps = auto
            .steps()
            .iter()
            .map(|step| {
                let mut draft = PromptStepDraft::from_text(&step.text);
                if let Some(ms) = step.delay_ms {
                    draft.delay.set(&ms.to_string());
                }
                draft
            })
            .collect();
        match &auto.action {
            AutomationAction::Send { target } => {
                m.action = AutomationActionKind::Send;
                m.original_target = Some(target.clone());
                // The target list + selected index are filled in by the caller
                // via `set_target_sessions` (it has the running-session list).
            }
            AutomationAction::Spawn {
                repo_path,
                worktree_branch,
                base_branch,
                extra_repos,
                session_mode,
                ..
            } => {
                m.action = AutomationActionKind::Spawn;
                m.repo.set(&repo_path.to_string_lossy());
                if let Some(w) = worktree_branch {
                    m.worktree.set(w);
                }
                if let Some(b) = base_branch {
                    m.base_branch.set(b);
                }
                m.session_mode = *session_mode;
                m.extra_repos.set(&format_extra_repos(extra_repos, true));
                m.extra_dirs.set(&format_extra_repos(extra_repos, false));
                // The agent + host selectors are filled in by the caller via
                // `set_agents` / `set_hosts` (the registries live on `App`).
            }
            AutomationAction::Exec {
                command,
                timeout_secs,
            } => {
                m.action = AutomationActionKind::Exec;
                m.command.set(command);
                if let Some(secs) = timeout_secs {
                    m.timeout.set(&secs.to_string());
                }
            }
        }
        m
    }
}

/// Render the worktree (`worktree = true`) or attached-dir half of an extra-repo
/// list as the comma-separated text the editor's `ExtraRepos`/`ExtraDirs` fields
/// use. A worktree extra with its own base renders as `path@base`.
fn format_extra_repos(extras: &[crate::session::ExtraRepo], worktree: bool) -> String {
    extras
        .iter()
        .filter(|e| e.worktree == worktree)
        .map(|e| match (&e.base_branch, worktree) {
            (Some(base), true) => format!("{}@{base}", e.repo_path.display()),
            _ => e.repo_path.display().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse the editor's comma-separated extra-repo / extra-dir fields into the
/// persisted list. Mirrors the CLI's `--add-repo path[@base]` / `--add-dir path`
/// grammar so both authoring paths agree.
pub fn parse_extra_repo_fields(repos: &str, dirs: &str) -> Vec<crate::session::ExtraRepo> {
    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut out: Vec<crate::session::ExtraRepo> = split(repos)
        .into_iter()
        .map(|entry| {
            let (path, base) = match entry.rsplit_once('@') {
                Some((p, b)) if !p.is_empty() && !b.is_empty() => (p.to_string(), Some(b.into())),
                _ => (entry, None),
            };
            crate::session::ExtraRepo {
                repo_path: crate::paths::expand_tilde(&path),
                worktree: true,
                base_branch: base,
            }
        })
        .collect();
    out.extend(
        split(dirs)
            .into_iter()
            .map(|path| crate::session::ExtraRepo {
                repo_path: crate::paths::expand_tilde(&path),
                worktree: false,
                base_branch: None,
            }),
    );
    out
}

/// Add `delta` to `v` modulo `modulus`, wrapping (e.g. hour 23 +1 → 0).
fn wrap_add(v: u32, delta: i32, modulus: u32) -> u32 {
    (v as i32 + delta).rem_euclid(modulus as i32) as u32
}

/// Step a selector index by `delta`, wrapping within `len`. An empty list keeps
/// index 0 (nothing to select).
fn wrap_index(index: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (index as i32 + delta).rem_euclid(len as i32) as usize
}

/// Format a millisecond duration as a compact, re-enterable string like
/// `1h30m` (matching [`crate::session::automation::parse_duration`]).
fn format_duration_short(ms: u64) -> String {
    let total_secs = ms / 1000;
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let mins = (total_secs % 3_600) / 60;
    let mut out = String::new();
    if days > 0 {
        out.push_str(&format!("{days}d"));
    }
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if mins > 0 || out.is_empty() {
        out.push_str(&format!("{mins}m"));
    }
    out
}

/// Best-effort reverse mapping of a cron expression generated by this editor
/// back into `(TriggerKind, hour, minute, weekday)`. Returns `None` for
/// expressions that don't match a known preset shape (those stay raw `Cron`).
fn recognize_cron(expr: &str) -> Option<(TriggerKind, u32, u32, u32)> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        return None;
    }
    let (min, hour, dom, mon, dow) = (f[0], f[1], f[2], f[3], f[4]);
    if dom != "*" || mon != "*" {
        return None;
    }
    let minute: u32 = min.parse().ok()?;
    if minute >= 60 {
        return None;
    }
    // Hourly: `m * * * *`.
    if hour == "*" && dow == "*" {
        return Some((TriggerKind::Hourly, 0, minute, 1));
    }
    let h: u32 = hour.parse().ok()?;
    if h >= 24 {
        return None;
    }
    match dow {
        "*" => Some((TriggerKind::Daily, h, minute, 1)),
        "1-5" => Some((TriggerKind::Weekdays, h, minute, 1)),
        single => {
            let d: u32 = single.parse().ok()?;
            (d <= 6).then_some((TriggerKind::Weekly, h, minute, d))
        }
    }
}

/// Render a cron expression as a short, human-readable schedule
/// (e.g. `daily 09:00`, `hourly :05`, `weekdays 09:00`, `Mondays 09:00`).
/// Returns `None` for expressions that don't match a known preset shape, so the
/// caller can fall back to the raw cron string for power-user expressions.
pub(crate) fn humanize_cron(expr: &str) -> Option<String> {
    let (kind, hour, minute, dow) = recognize_cron(expr)?;
    let hhmm = format!("{hour:02}:{minute:02}");
    Some(match kind {
        TriggerKind::Hourly => format!("hourly :{minute:02}"),
        TriggerKind::Daily => format!("daily {hhmm}"),
        TriggerKind::Weekdays => format!("weekdays {hhmm}"),
        TriggerKind::Weekly => format!("{} {hhmm}", weekday_plural(dow)),
        // `recognize_cron` never yields Once/Cron, but stay total.
        TriggerKind::Once | TriggerKind::Cron => return None,
    })
}

/// Pluralised weekday name for a Unix cron day-of-week number (0 or 7 = Sunday).
fn weekday_plural(dow: u32) -> &'static str {
    match dow % 7 {
        0 => "Sundays",
        1 => "Mondays",
        2 => "Tuesdays",
        3 => "Wednesdays",
        4 => "Thursdays",
        5 => "Fridays",
        6 => "Saturdays",
        _ => "weekly",
    }
}

/// The dry-run overlay's payload: an automation's name plus the resolved
/// `(label, value)` plan rows, snapshotted when the overlay opens.
#[derive(Debug, Clone, Default)]
pub struct AutomationDryRunModal {
    pub name: String,
    pub rows: Vec<(String, String)>,
}

// ── AutomationsListModal ────────────────────────────────────────────────

/// An entry in the automations list modal.
#[derive(Debug, Clone)]
pub struct AutomationListEntry {
    pub id: i64,
    pub name: String,
    pub summary: String,
    pub enabled: bool,
}

/// Modal state for listing and managing automations.
#[derive(Debug, Clone, Default)]
pub struct AutomationsListModal {
    pub index: usize,
    pub entries: Vec<AutomationListEntry>,
}

// ── RepoPickerModal ─────────────────────────────────────────────────────

/// What a repo-picker row is. Selection and worktree flags do NOT live on the
/// row — they live in path-keyed sets on [`RepoPickerModal`], because rows are
/// rebuilt on every parent re-scan and the user's picks must survive that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoRowKind {
    /// Parent-folder header (non-selectable group title; folds its children).
    Header,
    /// A selectable repo; `child` nests it under the preceding header.
    Repo { child: bool },
    /// First-run helper: "import repos from `path`" — shown (local only) while
    /// there are no bookmark rows at all. Activating it imports the folder as a
    /// parent bookmark and re-scans.
    ImportSuggestion,
    /// Pinned last row: start the session without a repo (`~` locally, the
    /// host's default directory remotely). Makes the no-repo session an
    /// explicit choice instead of a silent Enter fallthrough; `path` is unused.
    StartHere,
}

/// One row of the repo-picker list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    pub path: PathBuf,
    pub kind: RepoRowKind,
}

impl RepoRow {
    pub fn is_header(&self) -> bool {
        matches!(self.kind, RepoRowKind::Header)
    }

    pub fn is_child(&self) -> bool {
        matches!(self.kind, RepoRowKind::Repo { child: true })
    }

    pub fn is_repo(&self) -> bool {
        matches!(self.kind, RepoRowKind::Repo { .. })
    }
}

/// Whether the palette input currently holds a bookmark filter or a
/// filesystem path — decided by shape, so the user controls it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoInputMode {
    /// Fuzzy-filter the bookmark rows.
    Filter,
    /// A path being typed/completed (`~`, `/`, `./`, `../` prefix).
    Path,
}

/// One live directory-completion candidate shown while the palette input is
/// in path mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCandidate {
    /// Entry name as listed in the parent directory.
    pub name: String,
    /// The candidate's full path (tilde-expanded parent joined with `name`).
    pub full: PathBuf,
    /// Whether the directory is a git repo. Local only — probing a remote
    /// candidate would cost one ssh round-trip each, so remote candidates are
    /// always `false` and Enter drills in instead of opening.
    pub is_repo: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RepoPickerModal {
    /// Bookmark rows in recency order (a parent header followed by its
    /// scanned children, or a standalone repo), plus the pinned helper rows
    /// (import suggestions, "start here") at the end.
    pub rows: Vec<RepoRow>,
    /// Checked repos, keyed by path so rebuilds and filtering can't lose them.
    pub selected: HashSet<PathBuf>,
    /// Repos flagged for worktree mode, keyed by path. Toggling the flag on
    /// also checks the repo (see [`Self::toggle_worktree`]).
    pub worktree: HashSet<PathBuf>,
    /// Parent folders whose child tree is currently collapsed (keyed by path).
    /// Survives row rebuilds so collapsing state is kept across re-scans.
    pub collapsed: HashSet<PathBuf>,
    /// Cursor index in the bookmark list (indexes into `filtered_indices`).
    pub list_index: usize,
    /// The single always-focused palette input: filter text or a path.
    pub input: TextInput,
    /// Fish-style ghost completion for the input (path mode; derived from
    /// `candidates` locally, from the explicit Tab listing remotely).
    pub path_suggestion: Option<String>,
    /// Path mode: live directory candidates under the typed prefix (local:
    /// refreshed per keystroke; remote: filled by an explicit Tab).
    pub candidates: Vec<PathCandidate>,
    /// Path-mode highlight; `None` = act on the typed path itself.
    pub candidate_index: Option<usize>,
    /// Indices into `rows` that match the current filter.
    /// When the filter is empty, contains `0..rows.len()`.
    pub filtered_indices: Vec<usize>,
    /// Whether the wizard targets a remote host (drives the "start here"
    /// label and suppresses local-only rows on rebuild).
    pub remote: bool,
}

impl RepoPickerModal {
    /// Append a row.
    pub fn push_row(&mut self, path: PathBuf, kind: RepoRowKind) {
        self.rows.push(RepoRow { path, kind });
    }

    /// Whether the row at `idx` is a parent header (bounds-safe).
    pub fn is_header_row(&self, idx: usize) -> bool {
        self.rows.get(idx).is_some_and(RepoRow::is_header)
    }

    /// Whether the row at `idx` is a child repo under a parent (bounds-safe).
    pub fn is_child_row(&self, idx: usize) -> bool {
        self.rows.get(idx).is_some_and(RepoRow::is_child)
    }

    /// How the input is currently interpreted (see [`RepoInputMode`]).
    pub fn input_mode(&self) -> RepoInputMode {
        let v = self.input.value();
        // Path-shaped: a `~` home prefix, an explicit relative lead (`./`/`../`,
        // and their `\` forms on Windows), or any absolute path. `is_absolute`
        // is what makes a Windows drive path (`C:\repos`, `C:/repos`) — and a
        // Unix `/abs` — path mode; a bare filter query like `friring` is not
        // absolute, so it stays a fuzzy filter.
        let path_like = v.starts_with('~')
            || v.starts_with('/')
            || v.starts_with("./")
            || v.starts_with("../")
            || (cfg!(windows) && (v.starts_with(".\\") || v.starts_with("..\\")))
            || std::path::Path::new(v).is_absolute();
        if path_like {
            RepoInputMode::Path
        } else {
            RepoInputMode::Filter
        }
    }

    /// Number of checked repos that are actually present as rows (a stale
    /// selection whose bookmark was deleted doesn't count).
    pub fn picked_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.is_repo() && self.selected.contains(&r.path))
            .count()
    }

    /// Toggle whether the repo at `path` is checked. Callers guard against
    /// non-repo rows (headers and pinned rows have no selectable identity).
    pub fn toggle_selected(&mut self, path: &std::path::Path) {
        if !self.selected.remove(path) {
            self.selected.insert(path.to_path_buf());
        }
    }

    /// Toggle the worktree flag of the repo at `path`, checking the repo when
    /// the flag turns on — a worktree mark on an unchecked repo would be
    /// silently ignored at submit.
    pub fn toggle_worktree(&mut self, path: &std::path::Path) {
        if self.worktree.remove(path) {
            return;
        }
        self.worktree.insert(path.to_path_buf());
        self.selected.insert(path.to_path_buf());
    }

    /// Toggle the collapsed state of the parent header at `real_idx` (a no-op on
    /// non-header rows) and recompute the visible rows.
    pub fn toggle_collapsed(&mut self, real_idx: usize) {
        if !self.is_header_row(real_idx) {
            return;
        }
        let path = self.rows[real_idx].path.clone();
        if !self.collapsed.insert(path.clone()) {
            self.collapsed.remove(&path);
        }
        self.recompute_filter();
    }

    /// Rebuild `filtered_indices` from the current input and collapse state.
    /// Only filter-mode text filters (a typed path is not a query). Header rows
    /// are always visible; a child is hidden when its parent is collapsed
    /// (unless a filter is active, which expands all so matches are findable);
    /// the pinned helper rows hide while filtering. Keeps `list_index` in range.
    pub fn recompute_filter(&mut self) {
        let query = match self.input_mode() {
            RepoInputMode::Filter => self.input.value().to_string(),
            RepoInputMode::Path => String::new(),
        };
        let searching = !query.is_empty();
        let matches = |path: &std::path::Path| {
            !searching || crate::fuzzy::fuzzy_match(&query, &path.display().to_string()).is_some()
        };

        let mut indices = Vec::new();
        let mut current_collapsed = false;
        for (i, row) in self.rows.iter().enumerate() {
            let visible = match row.kind {
                RepoRowKind::Header => {
                    current_collapsed = self.collapsed.contains(&row.path);
                    true
                }
                RepoRowKind::Repo { child: true } => {
                    let hidden = current_collapsed && !searching;
                    !hidden && matches(&row.path)
                }
                RepoRowKind::Repo { child: false } => matches(&row.path),
                RepoRowKind::ImportSuggestion | RepoRowKind::StartHere => !searching,
            };
            if visible {
                indices.push(i);
            }
        }
        self.filtered_indices = indices;
        if self.list_index >= self.filtered_indices.len() {
            self.list_index = self.filtered_indices.len().saturating_sub(1);
        }
    }
}

// ── TaskEditorModal ─────────────────────────────────────────────────────

/// Focusable field in the task editor. A task is just title + description +
/// status; the agent action is chosen at trigger time (`r`), not authored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskField {
    #[default]
    Title,
    /// Multi-line markdown description (Enter inserts a newline; Ctrl+S saves).
    Description,
    /// Status selector (cycled with ←/→).
    Status,
}

/// The editor fields, in navigation order. Constant — no longer action-driven.
const TASK_FIELDS: [TaskField; 3] = [TaskField::Title, TaskField::Description, TaskField::Status];

/// Editor form for creating or editing a task.
#[derive(Debug, Clone)]
pub struct TaskEditorModal {
    /// `Some` when editing an existing task.
    pub editing_id: Option<i64>,
    pub title: TextInput,
    /// Multi-line markdown description.
    pub description: TextArea,
    pub status: crate::session::TaskStatus,
    pub field: TaskField,
}

impl TaskEditorModal {
    /// A blank editor for a new task.
    pub fn new() -> Self {
        Self {
            editing_id: None,
            title: TextInput::default(),
            description: TextArea::default(),
            status: crate::session::TaskStatus::Todo,
            field: TaskField::default(),
        }
    }

    /// Build an editor pre-filled from an existing task.
    pub fn from_task(task: &crate::session::Task) -> Self {
        let mut m = Self::new();
        m.editing_id = Some(task.id);
        m.title.set(&task.title);
        if let Some(d) = &task.description {
            m.description.set(d);
        }
        m.status = task.status;
        m
    }

    /// The fields shown, in navigation order.
    pub fn visible_fields(&self) -> Vec<TaskField> {
        TASK_FIELDS.to_vec()
    }

    /// Move focus to the next visible field (wraps).
    pub fn next_field(&mut self) {
        self.field = cycle_field(&TASK_FIELDS, self.field, 1);
    }

    /// Move focus to the previous visible field (wraps).
    pub fn prev_field(&mut self) {
        self.field = cycle_field(&TASK_FIELDS, self.field, -1);
    }

    /// The focused text field, or `None` for the description / status selector.
    pub fn active_field(&self) -> Option<&TextInput> {
        match self.field {
            TaskField::Title => Some(&self.title),
            // Description is a `TextArea` (handled explicitly); Status is a
            // selector adjusted with ←/→.
            TaskField::Description | TaskField::Status => None,
        }
    }

    /// The focused text field, or `None` for the description / status selector.
    pub fn active_field_mut(&mut self) -> Option<&mut TextInput> {
        match self.field {
            TaskField::Title => Some(&mut self.title),
            TaskField::Description | TaskField::Status => None,
        }
    }

    /// Whether the focused field is a selector adjusted with ←/→/Space.
    pub fn is_adjustable(&self) -> bool {
        matches!(self.field, TaskField::Status)
    }

    /// Adjust the focused selector by `delta` (−1 for ←, +1 for →/Space).
    pub fn adjust(&mut self, delta: i32) {
        if self.field == TaskField::Status {
            // Cycle in either direction (cycle() only goes forward, so step
            // backward via two forward cycles).
            self.status = if delta < 0 {
                self.status.cycle().cycle()
            } else {
                self.status.cycle()
            };
        }
    }

    /// Feed a key to the editor. Returns whether the caller should save, cancel
    /// (`Esc`), or keep editing.
    ///
    /// `Ctrl+S` saves from any field. On most fields `Enter` saves; on the
    /// multi-line `Description` field `Enter` inserts a newline and `Up`/`Down`
    /// move within the text (field navigation there is `Tab`/`BackTab` only).
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> EditorOutcome {
        // Ctrl+S is the universal save (the description field needs a save path
        // that isn't Enter).
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            return EditorOutcome::Save;
        }

        if self.field == TaskField::Description {
            // Readline Ctrl+W / Ctrl+U editing (and swallow other Ctrl chords so
            // they never insert a literal letter).
            if apply_ctrl_line_edit(&mut self.description, code, mods) {
                return EditorOutcome::Continue;
            }
            match code {
                KeyCode::Esc => return EditorOutcome::Cancel,
                KeyCode::Tab => self.next_field(),
                KeyCode::BackTab => self.prev_field(),
                _ => {
                    handle_textarea_key(&mut self.description, code);
                }
            }
            return EditorOutcome::Continue;
        }

        let adjustable = self.is_adjustable();
        match code {
            KeyCode::Esc => return EditorOutcome::Cancel,
            KeyCode::Enter => return EditorOutcome::Save,
            KeyCode::Tab | KeyCode::Down => self.next_field(),
            KeyCode::BackTab | KeyCode::Up => self.prev_field(),
            KeyCode::Left if adjustable => self.adjust(-1),
            KeyCode::Right | KeyCode::Char(' ') if adjustable => self.adjust(1),
            other => {
                apply_text_input_key(self.active_field_mut(), other, mods);
            }
        }
        EditorOutcome::Continue
    }
}

impl Default for TaskEditorModal {
    fn default() -> Self {
        Self::new()
    }
}

// ── Help / Keybinding Editor Modal ──────────────────────────────────────────

/// State for the interactive F1 keybinding editor.
///
/// `selected` indexes into [`Action::rebindable_in_order`], which matches the
/// row order rendered by `render_help_overlay`. When `capturing` is set, the
/// next keypress is captured as the new chord for the selected action rather
/// than being interpreted normally.
#[derive(Debug, Clone, Default)]
pub struct HelpModal {
    pub selected: usize,
    pub capturing: bool,
}

// ── Main Modal Enum ────────────────────────────────────────────────────────

/// One editable row in the [`SettingsModal`]. Section headers are render-only,
/// so this enum lists only the focusable fields, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    // ── [features] ──────────────────────────────────────────────────────
    FeatTasks,
    FeatAutomations,
    FeatFileViewer,
    FeatGlobalSearch,
    FeatDoubleShiftSearch,
    FeatInfoPanel,
    FeatShellPane,
    FeatCodeReview,
    FeatCcActivity,
    FeatPerfHud,
    FeatMouse,
    FeatNotifications,
    FeatSoftDelete,
    FeatVersionCheck,
    FeatAutoUpdate,
    // ── [notifications] ─────────────────────────────────────────────────
    NotifAlsoOnWaiting,
    NotifSuppressForActive,
    NotifSound,
    NotifMinInterval,
    // ── top-level scalars ───────────────────────────────────────────────
    ScrollbackLines,
    LazyRestore,
    TwoPanelMinCols,
    ThreePanelMinCols,
    InfoPanelPosition,
    AuditRetentionDays,
}

impl SettingsField {
    /// Field nav order — also the render order (headers are interleaved by the
    /// renderer). Used by [`cycle_field`] and the scroll-windowing logic.
    pub const ORDER: [SettingsField; 25] = [
        SettingsField::FeatTasks,
        SettingsField::FeatAutomations,
        SettingsField::FeatFileViewer,
        SettingsField::FeatGlobalSearch,
        SettingsField::FeatDoubleShiftSearch,
        SettingsField::FeatInfoPanel,
        SettingsField::FeatShellPane,
        SettingsField::FeatCodeReview,
        SettingsField::FeatCcActivity,
        SettingsField::FeatPerfHud,
        SettingsField::FeatMouse,
        SettingsField::FeatNotifications,
        SettingsField::FeatSoftDelete,
        SettingsField::FeatVersionCheck,
        SettingsField::FeatAutoUpdate,
        SettingsField::NotifAlsoOnWaiting,
        SettingsField::NotifSuppressForActive,
        SettingsField::NotifSound,
        SettingsField::NotifMinInterval,
        SettingsField::ScrollbackLines,
        SettingsField::LazyRestore,
        SettingsField::TwoPanelMinCols,
        SettingsField::ThreePanelMinCols,
        SettingsField::InfoPanelPosition,
        SettingsField::AuditRetentionDays,
    ];

    /// The field's `settings.toml` key, a short scannable keyword, and one-line
    /// help, in a single table so the three parallel lookups never drift (and
    /// stay one match, not three). Both keyword and help avoid naming key chords
    /// — those are user-configurable, so a hard-coded hint would drift from the
    /// actual binding.
    fn meta(self) -> (&'static str, &'static str, &'static str) {
        use SettingsField::*;
        match self {
            FeatTasks => ("tasks", "Tasks", "Tasks panel and task search"),
            FeatAutomations => (
                "automations",
                "Automations",
                "Automations pane and schedule firing",
            ),
            FeatFileViewer => ("file_viewer", "File viewer", "File viewer column"),
            FeatGlobalSearch => ("global_search", "Global search", "Global search popup"),
            FeatDoubleShiftSearch => (
                "double_shift_search",
                "Double Shift",
                "Double-Shift opens the search (kitty-protocol terminals)",
            ),
            FeatInfoPanel => ("info_panel", "Info panel", "Info panel column"),
            FeatShellPane => ("shell_pane", "Shell pane", "Per-session shell pane"),
            FeatCodeReview => (
                "code_review",
                "Code review",
                "Native code-review view (diff + comments)",
            ),
            FeatCcActivity => (
                "cc_activity",
                "Agent activity",
                "Per-session agent activity retrospective (F9)",
            ),
            FeatPerfHud => (
                "perf_hud",
                "Perf HUD",
                "Live perf counters + frame/tick timing overlay",
            ),
            FeatMouse => ("mouse", "Mouse", "Mouse: clicks, wheel, drag-select, hover"),
            FeatNotifications => (
                "notifications",
                "Notifications",
                "OS desktop notifications on attention",
            ),
            FeatSoftDelete => (
                "soft_delete",
                "Soft delete",
                "Soft-delete sessions with an undo window",
            ),
            FeatVersionCheck => (
                "version_check",
                "Version check",
                "Check GitHub for updates (network call)",
            ),
            FeatAutoUpdate => (
                "auto_update",
                "Auto-update",
                "Silently self-update on launch (network call)",
            ),
            NotifAlsoOnWaiting => (
                "also_on_waiting",
                "Notify done",
                "Also notify when a session finishes (Done)",
            ),
            NotifSuppressForActive => (
                "suppress_for_active",
                "Skip active",
                "Skip the session you're already viewing",
            ),
            NotifSound => ("sound", "Sound", "Play the OS notification sound"),
            NotifMinInterval => (
                "min_interval_secs",
                "Min interval",
                "Min seconds between notifications per session",
            ),
            ScrollbackLines => (
                "scrollback_lines",
                "Scrollback",
                "Terminal history lines kept per session",
            ),
            LazyRestore => (
                "lazy_session_restore",
                "Lazy restore",
                "Restore dead sessions as greyed ghosts, not respawns",
            ),
            TwoPanelMinCols => (
                "two_panel_min_cols",
                "2-panel width",
                "Min width (cols) to show the 2nd panel",
            ),
            ThreePanelMinCols => (
                "three_panel_min_cols",
                "3-panel width",
                "Min width (cols) to show the 3rd panel",
            ),
            InfoPanelPosition => (
                "info_panel_position",
                "Info position",
                "Info pane dock: under sessions or own column",
            ),
            AuditRetentionDays => (
                "audit_retention_days",
                "Audit days",
                "Days of audit-log history kept",
            ),
        }
    }

    /// The field's `settings.toml` key (e.g. `tasks`).
    pub fn label(self) -> &'static str {
        self.meta().0
    }

    /// A short, scannable keyword shown as the bold left column of each row.
    pub fn keyword(self) -> &'static str {
        self.meta().1
    }

    /// One-line help shown next to the keyword (and for the selected field in
    /// the panel footer context).
    pub fn description(self) -> &'static str {
        self.meta().2
    }

    /// Whether the field holds a boolean (toggled with Space/Enter) vs. a
    /// stepped value (←/→): the numeric scalars plus the info-pane position,
    /// which cycles its variants through the same stepper.
    pub fn is_scalar(self) -> bool {
        use SettingsField::*;
        matches!(
            self,
            NotifMinInterval
                | ScrollbackLines
                | TwoPanelMinCols
                | ThreePanelMinCols
                | InfoPanelPosition
                | AuditRetentionDays
        )
    }

    /// Whether changing this field takes effect only after a restart. Feature
    /// flags that gate UI panels (read from `App.features` every frame) apply
    /// live; everything else is read once at startup from `settings::global()`,
    /// which is a write-once value that can't be re-applied in-process.
    ///
    /// Derived from the canonical
    /// [`crate::session::settings::Settings::restart_only_differs`] rather than
    /// a second hand-maintained list: flip just this field on a default draft
    /// and ask whether that single change registers as restart-only. So the UI
    /// `⟳` marker can never disagree with the toast/reload partition.
    pub fn restart_required(self) -> bool {
        let mut modal = SettingsModal::new(crate::session::settings::Settings::default());
        modal.field = self;
        if self.is_scalar() {
            modal.adjust(1);
        } else {
            modal.toggle();
        }
        modal.restart_required_changed()
    }
}

/// Settings panel: a centered modal that edits a working copy of [`Settings`]
/// (`draft`) and writes it back to `settings.toml` on save. Feature flags that
/// gate UI panels apply live; the rest take effect after a restart. No live
/// preview — edits apply only on save, so `Esc` is a clean discard.
#[derive(Debug, Clone)]
pub struct SettingsModal {
    pub draft: crate::session::settings::Settings,
    pub original: crate::session::settings::Settings,
    pub field: SettingsField,
}

impl SettingsModal {
    pub fn new(draft: crate::session::settings::Settings) -> Self {
        Self {
            original: draft.clone(),
            draft,
            field: SettingsField::FeatTasks,
        }
    }

    pub fn next_field(&mut self) {
        self.field = cycle_field(&SettingsField::ORDER, self.field, 1);
    }

    pub fn prev_field(&mut self) {
        self.field = cycle_field(&SettingsField::ORDER, self.field, -1);
    }

    /// Toggle the boolean the current field maps to (no-op on scalar fields).
    pub fn toggle(&mut self) {
        use SettingsField::*;
        let f = &mut self.draft.features;
        let n = &mut self.draft.notifications;
        match self.field {
            FeatTasks => f.tasks = !f.tasks,
            FeatAutomations => f.automations = !f.automations,
            FeatFileViewer => f.file_viewer = !f.file_viewer,
            FeatGlobalSearch => f.global_search = !f.global_search,
            FeatDoubleShiftSearch => f.double_shift_search = !f.double_shift_search,
            FeatInfoPanel => f.info_panel = !f.info_panel,
            FeatShellPane => f.shell_pane = !f.shell_pane,
            FeatCodeReview => f.code_review = !f.code_review,
            FeatCcActivity => f.cc_activity = !f.cc_activity,
            FeatPerfHud => f.perf_hud = !f.perf_hud,
            FeatMouse => f.mouse = !f.mouse,
            FeatNotifications => f.notifications = !f.notifications,
            FeatSoftDelete => f.soft_delete = !f.soft_delete,
            FeatVersionCheck => f.version_check = !f.version_check,
            FeatAutoUpdate => f.auto_update = !f.auto_update,
            NotifAlsoOnWaiting => n.also_on_waiting = !n.also_on_waiting,
            NotifSuppressForActive => n.suppress_for_active = !n.suppress_for_active,
            NotifSound => n.sound = !n.sound,
            LazyRestore => self.draft.lazy_session_restore = !self.draft.lazy_session_restore,
            NotifMinInterval | ScrollbackLines | TwoPanelMinCols | ThreePanelMinCols
            | InfoPanelPosition | AuditRetentionDays => {}
        }
    }

    /// Step a scalar field by `delta` (±1), with field-specific step sizes and
    /// clamping (no-op on boolean fields).
    pub fn adjust(&mut self, delta: i32) {
        use SettingsField::*;
        let d = &mut self.draft;
        match self.field {
            ScrollbackLines => {
                d.scrollback_lines =
                    step_clamp(d.scrollback_lines as i64, delta, 500, 100, 200_000) as usize;
            }
            TwoPanelMinCols => {
                d.two_panel_min_cols =
                    step_clamp(i64::from(d.two_panel_min_cols), delta, 5, 40, 400) as u16;
            }
            ThreePanelMinCols => {
                d.three_panel_min_cols =
                    step_clamp(i64::from(d.three_panel_min_cols), delta, 5, 40, 400) as u16;
            }
            AuditRetentionDays => {
                d.audit_retention_days =
                    step_clamp(d.audit_retention_days as i64, delta, 5, 1, 3650) as u64;
            }
            NotifMinInterval => {
                d.notifications.min_interval_secs =
                    step_clamp(d.notifications.min_interval_secs as i64, delta, 5, 0, 3600) as u64;
            }
            InfoPanelPosition => {
                // Cycle the position variants (wrapping) through the stepper.
                let all = crate::session::settings::InfoPanelPosition::ALL;
                let pos = all
                    .iter()
                    .position(|p| *p == d.info_panel_position)
                    .unwrap_or(0);
                let next = (pos as i32 + delta).rem_euclid(all.len() as i32) as usize;
                d.info_panel_position = all[next];
            }
            _ => {}
        }
    }

    /// String value rendered for the current field.
    pub fn value_string(&self, field: SettingsField) -> String {
        use SettingsField::*;
        let f = &self.draft.features;
        let n = &self.draft.notifications;
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        match field {
            FeatTasks => on(f.tasks),
            FeatAutomations => on(f.automations),
            FeatFileViewer => on(f.file_viewer),
            FeatGlobalSearch => on(f.global_search),
            FeatDoubleShiftSearch => on(f.double_shift_search),
            FeatInfoPanel => on(f.info_panel),
            FeatShellPane => on(f.shell_pane),
            FeatCodeReview => on(f.code_review),
            FeatCcActivity => on(f.cc_activity),
            FeatPerfHud => on(f.perf_hud),
            FeatMouse => on(f.mouse),
            FeatNotifications => on(f.notifications),
            FeatSoftDelete => on(f.soft_delete),
            FeatVersionCheck => on(f.version_check),
            FeatAutoUpdate => on(f.auto_update),
            NotifAlsoOnWaiting => on(n.also_on_waiting),
            NotifSuppressForActive => on(n.suppress_for_active),
            NotifSound => on(n.sound),
            NotifMinInterval => n.min_interval_secs.to_string(),
            ScrollbackLines => self.draft.scrollback_lines.to_string(),
            LazyRestore => on(self.draft.lazy_session_restore),
            TwoPanelMinCols => self.draft.two_panel_min_cols.to_string(),
            ThreePanelMinCols => self.draft.three_panel_min_cols.to_string(),
            InfoPanelPosition => self.draft.info_panel_position.as_str().to_string(),
            AuditRetentionDays => self.draft.audit_retention_days.to_string(),
        }
    }

    /// True once any restart-required field differs from the opened state.
    /// Drives the "some changes apply after restart" toast and indicator.
    pub fn restart_required_changed(&self) -> bool {
        self.draft.restart_only_differs(&self.original)
    }

    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> EditorOutcome {
        let scalar = self.field.is_scalar();
        match code {
            KeyCode::Esc => EditorOutcome::Cancel,
            // Explicit save only (Ctrl+S) — keeps Enter free to toggle a bool.
            KeyCode::Char('s') | KeyCode::Char('S') if mods.contains(KeyModifiers::CONTROL) => {
                EditorOutcome::Save
            }
            KeyCode::Tab | KeyCode::Down => {
                self.next_field();
                EditorOutcome::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.prev_field();
                EditorOutcome::Continue
            }
            KeyCode::Left if scalar => {
                self.adjust(-1);
                EditorOutcome::Continue
            }
            KeyCode::Right if scalar => {
                self.adjust(1);
                EditorOutcome::Continue
            }
            KeyCode::Char(' ') if scalar => {
                self.adjust(1);
                EditorOutcome::Continue
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                self.toggle();
                EditorOutcome::Continue
            }
            _ => EditorOutcome::Continue,
        }
    }
}

/// Step `current` by `delta * step`, clamped to `[min, max]`.
fn step_clamp(current: i64, delta: i32, step: i64, min: i64, max: i64) -> i64 {
    (current + i64::from(delta) * step).clamp(min, max)
}

// ── Sandbox profiles ────────────────────────────────────────────────────

/// Modal state for the sandbox-profile list (`docs/SANDBOX.md` §UI).
///
/// The rows are the renderer's own view type: the list owns nothing but the
/// cursor, so a second app-side copy of every column would only be a way for
/// the two to disagree.
#[derive(Debug, Clone, Default)]
pub struct SandboxListModal {
    pub index: usize,
    pub entries: Vec<crate::ui::sandbox_list_modal::SandboxProfileRow>,
}

impl SandboxListModal {
    /// The selected row, or `None` while the list is empty.
    pub fn selected(&self) -> Option<&crate::ui::sandbox_list_modal::SandboxProfileRow> {
        self.entries.get(self.index)
    }

    /// The selected profile's name — what edit and delete key on, the name
    /// being the storage identity.
    pub fn selected_name(&self) -> Option<&str> {
        self.selected().map(|e| e.name.as_str())
    }
}

/// Focusable field in the sandbox-profile editor.
///
/// The set shown depends only on whether the two sub-lists have entries (see
/// [`SandboxEditorModal::visible_fields`]). A field the chosen backend cannot
/// honour stays *visible but inert* — see [`sandbox_field_available`] — so an
/// unavailable capability is shown as unavailable rather than silently missing
/// (`docs/SANDBOX.md` §Failure modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxField {
    #[default]
    Name,
    /// Isolation technology (cycled with ←/→); `auto` also shows what it
    /// resolved to on this host.
    Backend,
    /// Path sub-list anchor: ←/→ walk it, `n`/`d` add/remove, `[`/`]` reorder.
    Paths,
    /// The selected path, as typed.
    PathText,
    /// The selected path's `‹ ro | rw ›`.
    PathMode,
    Network,
    /// Allowed-domain sub-list anchor: ←/→ walk it, `n`/`d` add/remove.
    Domains,
    /// The selected allowed domain, as typed.
    DomainText,
    /// Ask on first use of an unlisted domain (Space toggles).
    PromptDomains,
    ReadScope,
    /// Place backends only: memory cap in MB.
    Memory,
    /// Place backends only: CPU cap.
    Cpus,
    /// Place backends only: image reference.
    Image,
    /// Place backends only: build source, an alternative to the image.
    Containerfile,
    /// Whether the agent may run a command outside the boundary (Space toggles).
    Fallback,
}

/// Whether `field` can be edited against a backend of `shape` (`None` = an
/// unresolved `auto`, whose shape is only known after a host probe) with
/// `network` selected.
///
/// The single rule behind both halves of the unavailable-capability treatment:
/// the editor drops input to an unavailable field, and
/// [`crate::ui::sandbox_editor_modal`] renders the reason in place of its
/// value. An unresolved `auto` blocks nothing, mirroring
/// [`SandboxProfile::validate`](crate::session::SandboxProfile::validate),
/// which exempts it for the same reason.
pub fn sandbox_field_available(
    field: SandboxField,
    shape: Option<crate::session::SandboxShape>,
    network: crate::session::NetworkMode,
) -> bool {
    match field {
        SandboxField::Memory | SandboxField::Cpus => {
            shape.map_or(true, crate::session::SandboxShape::supports_limits)
        }
        SandboxField::Image | SandboxField::Containerfile => {
            shape.map_or(true, crate::session::SandboxShape::supports_image)
        }
        SandboxField::ReadScope => {
            shape.map_or(true, crate::session::SandboxShape::supports_read_scope)
        }
        // Nothing is unlisted under `full`, and nothing leaves at all under
        // `none`, so there is no first use to ask about either way.
        SandboxField::PromptDomains => {
            matches!(network, crate::session::NetworkMode::Allowlist)
        }
        _ => true,
    }
}

/// One path row being edited: the text exactly as typed (`~` kept, expanded
/// only at launch) plus its read/write intent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPathDraft {
    pub text: TextInput,
    pub mode: crate::session::PathMode,
}

/// Editor form for creating or editing a sandbox profile.
///
/// Mirrors [`AutomationEditorModal`]: one `visible_fields` projection drives
/// both render order and Tab navigation, selector fields are adjusted with
/// ←/→, and `Enter`/`Ctrl+S` hand a validated value back to the caller.
#[derive(Debug, Clone)]
pub struct SandboxEditorModal {
    /// The name the profile was loaded under; `None` for a new profile.
    ///
    /// The name is the storage key, so renaming is a delete-plus-insert that
    /// also rewrites the sessions referencing it — this is what tells the
    /// caller the name moved.
    pub editing: Option<String>,
    pub name: TextInput,
    pub backend: crate::session::SandboxBackendKind,
    /// What `auto` resolves to on this host, from the sandbox module's probe.
    /// `None` until probed, which keeps every capability editable — the same
    /// exemption the profile validator makes for `auto`.
    pub resolved: Option<crate::session::SandboxBackendKind>,
    /// Why the backend this profile would run on is not usable here, from the
    /// probe — the actionable half of the same question `resolved` answers.
    /// `None` when it is usable, and while an `auto` ladder is unresolved.
    pub backend_unavailable: Option<String>,
    pub paths: Vec<SandboxPathDraft>,
    /// Index into `paths` of the row `PathText`/`PathMode` edit.
    pub path_index: usize,
    pub network_mode: crate::session::NetworkMode,
    /// Allowed `host[:port]` entries, as typed.
    pub domains: Vec<TextInput>,
    /// Index into `domains` of the row `DomainText` edits.
    pub domain_index: usize,
    /// Deny entries, carried through untouched: they beat allows in every mode
    /// but have no editor of their own yet, and dropping them on save would
    /// silently widen the profile.
    pub deny: Vec<String>,
    pub prompt_new_domains: bool,
    pub read_scope: crate::session::ReadScope,
    /// Memory cap in MB, as typed. Empty = uncapped.
    pub memory: TextInput,
    /// CPU cap, as typed. Empty = uncapped.
    pub cpus: TextInput,
    pub image: TextInput,
    pub containerfile: TextInput,
    pub allow_unsandboxed_fallback: bool,
    pub field: SandboxField,
    /// Carried from the stored profile so saving an edit doesn't reset it.
    pub created_at: u64,
    /// `column = 'value'` for every column of the stored row friring could not
    /// decode (`crate::storage::sandboxes::UndecodedColumn`). Empty for a
    /// healthy profile and for a new one.
    ///
    /// The form already holds the narrow values storage substituted, so saving
    /// **is** the repair — this is what tells the user there is something to
    /// repair, and which of the values in front of them are friring's rather
    /// than theirs. Filled in by the caller that loaded the row; a form has no
    /// database.
    pub undecoded: Vec<String>,
}

impl Default for SandboxEditorModal {
    /// The blank new-profile form, seeded from
    /// [`SandboxProfile::default`](crate::session::SandboxProfile::default) so
    /// the editor's starting knobs are the profile's own defaults rather than a
    /// second copy of them that could drift.
    fn default() -> Self {
        Self::from_profile(&crate::session::SandboxProfile::default())
    }
}

impl SandboxEditorModal {
    /// A form pre-filled from `profile`. A profile with a name is treated as an
    /// existing one (see [`editing`](Self::editing)); the blank default
    /// profile, whose name is empty, opens as a new one.
    pub fn from_profile(profile: &crate::session::SandboxProfile) -> Self {
        let mut name = TextInput::default();
        name.set(&profile.name);
        let mut memory = TextInput::default();
        if let Some(mb) = profile.memory_mb {
            memory.set(&mb.to_string());
        }
        let mut cpus = TextInput::default();
        if let Some(c) = profile.cpus {
            cpus.set(&c.to_string());
        }
        let mut image = TextInput::default();
        image.set(profile.image.as_deref().unwrap_or_default());
        let mut containerfile = TextInput::default();
        containerfile.set(profile.containerfile.as_deref().unwrap_or_default());
        Self {
            editing: (!profile.name.trim().is_empty()).then(|| profile.name.clone()),
            name,
            backend: profile.backend,
            resolved: None,
            backend_unavailable: None,
            paths: profile
                .paths
                .iter()
                .map(|p| {
                    let mut text = TextInput::default();
                    text.set(&p.path);
                    SandboxPathDraft { text, mode: p.mode }
                })
                .collect(),
            path_index: 0,
            network_mode: profile.network_mode,
            domains: profile
                .network_allow
                .iter()
                .map(|d| {
                    let mut text = TextInput::default();
                    text.set(d);
                    text
                })
                .collect(),
            domain_index: 0,
            deny: profile.network_deny.clone(),
            prompt_new_domains: profile.prompt_new_domains,
            read_scope: profile.read_scope,
            memory,
            cpus,
            image,
            containerfile,
            allow_unsandboxed_fallback: profile.allow_unsandboxed_fallback,
            field: SandboxField::default(),
            created_at: profile.created_at,
            undecoded: Vec::new(),
        }
    }

    /// The fields shown, in display and navigation order. Only the two
    /// sub-lists change the set: with nothing in a list there is no entry to
    /// edit, so its text (and mode) rows would point at nothing.
    pub fn visible_fields(&self) -> Vec<SandboxField> {
        use SandboxField::*;
        let mut fields = vec![Name, Backend, Paths];
        if !self.paths.is_empty() {
            fields.extend([PathText, PathMode]);
        }
        fields.extend([Network, Domains]);
        if !self.domains.is_empty() {
            fields.push(DomainText);
        }
        fields.extend([
            PromptDomains,
            ReadScope,
            Memory,
            Cpus,
            Image,
            Containerfile,
            Fallback,
        ]);
        fields
    }

    /// The backend that will actually run: the chosen one, or what `auto`
    /// resolved to once a host has been probed.
    pub fn effective_backend(&self) -> crate::session::SandboxBackendKind {
        match self.backend {
            crate::session::SandboxBackendKind::Auto => self.resolved.unwrap_or(self.backend),
            explicit => explicit,
        }
    }

    /// [`effective_backend`](Self::effective_backend)'s shape, or `None` while
    /// an `auto` backend is still unresolved.
    pub fn effective_shape(&self) -> Option<crate::session::SandboxShape> {
        self.effective_backend().shape()
    }

    /// Whether `field` accepts input right now (see
    /// [`sandbox_field_available`]).
    pub fn field_available(&self, field: SandboxField) -> bool {
        sandbox_field_available(field, self.effective_shape(), self.network_mode)
    }

    /// The path row `PathText`/`PathMode` edit.
    pub fn selected_path(&self) -> Option<&SandboxPathDraft> {
        self.paths.get(self.path_index)
    }

    fn selected_path_mut(&mut self) -> Option<&mut SandboxPathDraft> {
        self.paths.get_mut(self.path_index)
    }

    /// The domain row `DomainText` edits.
    pub fn selected_domain(&self) -> Option<&TextInput> {
        self.domains.get(self.domain_index)
    }

    /// Insert a blank read-only path after the selected row and select it.
    /// Read-only because widening a boundary should be a deliberate keystroke —
    /// the same reason [`PathMode`](crate::session::PathMode) defaults that way.
    pub fn add_path(&mut self) {
        let at = sublist_insert_index(self.path_index, self.paths.len());
        self.paths.insert(at, SandboxPathDraft::default());
        self.path_index = at;
    }

    /// Remove the selected path. Unlike the automation editor's steps, an empty
    /// list is representable here: the blank editor starts with none, and
    /// saving without one is what the profile validator rejects.
    pub fn remove_path(&mut self) {
        if self.path_index >= self.paths.len() {
            return;
        }
        self.paths.remove(self.path_index);
        self.path_index = self.path_index.min(self.paths.len().saturating_sub(1));
    }

    /// Move the selected path one row earlier (`-1`) or later (`+1`), keeping
    /// the selection on it. A no-op at the ends.
    pub fn move_path(&mut self, delta: i32) {
        let Some(target) = sublist_swap_target(self.path_index, delta, self.paths.len()) else {
            return;
        };
        self.paths.swap(self.path_index, target);
        self.path_index = target;
    }

    /// Insert a blank allowed domain after the selected row and select it.
    pub fn add_domain(&mut self) {
        let at = sublist_insert_index(self.domain_index, self.domains.len());
        self.domains.insert(at, TextInput::default());
        self.domain_index = at;
    }

    /// Remove the selected allowed domain.
    pub fn remove_domain(&mut self) {
        if self.domain_index >= self.domains.len() {
            return;
        }
        self.domains.remove(self.domain_index);
        self.domain_index = self.domain_index.min(self.domains.len().saturating_sub(1));
    }

    /// Move focus to the next visible field (wraps).
    pub fn next_field(&mut self) {
        self.field = cycle_field(&self.visible_fields(), self.field, 1);
    }

    /// Move focus to the previous visible field (wraps).
    pub fn prev_field(&mut self) {
        self.field = cycle_field(&self.visible_fields(), self.field, -1);
    }

    /// Whether the focused field is adjusted with ←/→/Space rather than typed
    /// into. The sub-list anchors count: their arrows walk the list.
    pub fn is_adjustable(&self) -> bool {
        use SandboxField::*;
        matches!(
            self.field,
            Backend | Paths | PathMode | Network | Domains | PromptDomains | ReadScope | Fallback
        )
    }

    /// Adjust the focused selector/toggle by `delta` (−1 for ←, +1 for →/Space).
    /// Input to an unavailable field is dropped, so a capability the backend
    /// cannot honour cannot be edited into a meaningless state.
    pub fn adjust(&mut self, delta: i32) {
        use crate::session::{NetworkMode, PathMode, ReadScope, SandboxBackendKind};
        use SandboxField::*;
        if !self.field_available(self.field) {
            return;
        }
        match self.field {
            Backend => self.backend = cycle_value(SandboxBackendKind::ALL, self.backend, delta),
            Paths => self.path_index = wrap_index(self.path_index, delta, self.paths.len()),
            PathMode => {
                if let Some(p) = self.selected_path_mut() {
                    p.mode = cycle_value(PathMode::ALL, p.mode, delta);
                }
            }
            Network => self.network_mode = cycle_value(NetworkMode::ALL, self.network_mode, delta),
            Domains => self.domain_index = wrap_index(self.domain_index, delta, self.domains.len()),
            PromptDomains => self.prompt_new_domains = !self.prompt_new_domains,
            ReadScope => self.read_scope = cycle_value(ReadScope::ALL, self.read_scope, delta),
            Fallback => self.allow_unsandboxed_fallback = !self.allow_unsandboxed_fallback,
            _ => {}
        }
    }

    /// The focused text field, or `None` for a selector, a toggle, an empty
    /// sub-list, or a field the backend cannot honour (whose row renders the
    /// reason instead of a value).
    pub fn active_field_mut(&mut self) -> Option<&mut TextInput> {
        use SandboxField::*;
        if !self.field_available(self.field) {
            return None;
        }
        Some(match self.field {
            Name => &mut self.name,
            PathText => return self.selected_path_mut().map(|p| &mut p.text),
            DomainText => return self.domains.get_mut(self.domain_index),
            Memory => &mut self.memory,
            Cpus => &mut self.cpus,
            Image => &mut self.image,
            Containerfile => &mut self.containerfile,
            Backend | Paths | PathMode | Network | Domains | PromptDomains | ReadScope
            | Fallback => return None,
        })
    }

    /// The caret within the focused text field — where the renderer draws the
    /// block cursor. `0` for a selector, a toggle or an inert field, none of
    /// which draw one.
    pub fn active_cursor(&self) -> usize {
        use SandboxField::*;
        match self.field {
            Name => self.name.cursor_pos(),
            PathText => self.selected_path().map_or(0, |p| p.text.cursor_pos()),
            DomainText => self.selected_domain().map_or(0, TextInput::cursor_pos),
            Memory => self.memory.cursor_pos(),
            Cpus => self.cpus.cursor_pos(),
            Image => self.image.cursor_pos(),
            Containerfile => self.containerfile.cursor_pos(),
            Backend | Paths | PathMode | Network | Domains | PromptDomains | ReadScope
            | Fallback => 0,
        }
    }

    /// Sub-list chords on the `Paths`/`Domains` anchors: `n` adds a row after
    /// the current one, `d` deletes it, `[`/`]` reorder (paths only — the allow
    /// list's order carries no meaning). Returns whether the key was consumed.
    /// Safe to bind letters here: both anchors are selectors, so nothing types
    /// into them.
    fn handle_sublist_key(&mut self, code: KeyCode) -> bool {
        match (self.field, code) {
            (SandboxField::Paths, KeyCode::Char('n')) => self.add_path(),
            (SandboxField::Paths, KeyCode::Char('d')) => self.remove_path(),
            (SandboxField::Paths, KeyCode::Char('[')) => self.move_path(-1),
            (SandboxField::Paths, KeyCode::Char(']')) => self.move_path(1),
            (SandboxField::Domains, KeyCode::Char('n')) => self.add_domain(),
            (SandboxField::Domains, KeyCode::Char('d')) => self.remove_domain(),
            _ => return false,
        }
        true
    }

    /// Feed a key to the editor, mutating field state. Returns whether the
    /// caller should save (`Enter` or `Ctrl+S`), cancel (`Esc`), or keep
    /// editing. Every field is single-line, so `Enter` saves from anywhere.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> EditorOutcome {
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            return EditorOutcome::Save;
        }
        let adjustable = self.is_adjustable();
        match code {
            KeyCode::Esc => return EditorOutcome::Cancel,
            KeyCode::Enter => return EditorOutcome::Save,
            KeyCode::Tab | KeyCode::Down => self.next_field(),
            KeyCode::BackTab | KeyCode::Up => self.prev_field(),
            KeyCode::Left if adjustable => self.adjust(-1),
            KeyCode::Right | KeyCode::Char(' ') if adjustable => self.adjust(1),
            other if self.handle_sublist_key(other) => {}
            other => {
                apply_text_input_key(self.active_field_mut(), other, mods);
            }
        }
        EditorOutcome::Continue
    }

    /// The profile these fields describe, without the uniqueness check —
    /// [`validated_profile`](Self::validated_profile) is what a save calls.
    ///
    /// Blank rows are dropped (an added-then-abandoned row is not a path), and
    /// a capability the backend cannot honour is left out rather than saved
    /// into a profile the validator would reject: the row renders as
    /// unavailable, so a value the user cannot see must not decide the save.
    /// The typed text stays in the form, so choosing a backend that supports it
    /// again brings it back.
    pub fn build_profile(&self) -> Result<crate::session::SandboxProfile, String> {
        use crate::session::{SandboxPath, SandboxProfile};
        let limits = self.field_available(SandboxField::Memory);
        let image = self.field_available(SandboxField::Image);
        Ok(SandboxProfile {
            name: self.name.value().trim().to_string(),
            backend: self.backend,
            paths: self
                .paths
                .iter()
                .filter(|p| !p.text.value().trim().is_empty())
                .map(|p| SandboxPath {
                    path: p.text.value().trim().to_string(),
                    mode: p.mode,
                })
                .collect(),
            network_mode: self.network_mode,
            network_allow: non_empty_values(&self.domains),
            network_deny: self.deny.clone(),
            prompt_new_domains: self.prompt_new_domains,
            read_scope: self.read_scope,
            memory_mb: limits
                .then(|| parse_limit(self.memory.value(), "Memory limit", "megabytes"))
                .transpose()?
                .flatten(),
            cpus: limits
                .then(|| parse_limit(self.cpus.value(), "CPU limit", "CPUs"))
                .transpose()?
                .flatten(),
            image: image.then(|| trimmed_option(self.image.value())).flatten(),
            containerfile: image
                .then(|| trimmed_option(self.containerfile.value()))
                .flatten(),
            allow_unsandboxed_fallback: self.allow_unsandboxed_fallback,
            created_at: self.created_at,
            // Storage stamps the save; a form has no clock.
            updated_at: 0,
        })
    }

    /// The profile these fields describe, validated against `existing_names`
    /// (every stored profile's name, the edited one included — it is filtered
    /// out here so re-saving under its own name is not a collision) and against
    /// the host locations no sandbox may be handed.
    ///
    /// The error is one sentence for the footer toast: the editor has no inline
    /// form-error widget.
    pub fn validated_profile(
        &self,
        existing_names: &[String],
    ) -> Result<crate::session::SandboxProfile, String> {
        let profile = self.build_profile()?;
        let editing = self.editing.as_deref();
        let others: Vec<String> = existing_names
            .iter()
            .filter(|n| !matches!(editing, Some(e) if n.trim().eq_ignore_ascii_case(e.trim())))
            .cloned()
            .collect();
        profile.validate_unique(&others)?;
        writable_roots_refusal(&profile).map_or(Ok(profile), Err)
    }
}

/// Why this profile's paths may not be stored, or `None` when they are
/// grantable.
///
/// The same refusals
/// [`SandboxLaunch::validate`](crate::sandbox::SandboxLaunch::validate) makes,
/// moved forward to the save: a *read-write* path enclosing friring's data
/// directory reaches the database (ADR-29) or drives the host's own multiplexer
/// through a tmux socket directory, and a path in **either** mode may reach
/// neither friring's own sandbox state — the other profiles' logins, the
/// generated policies, the other sessions' sockets — nor a container engine's
/// control socket, both of which are taken by being *readable*. Catching it here
/// turns "a stored profile that refuses every session picking it" into "a form
/// that will not save", with the same sentence.
///
/// It cannot live in
/// [`SandboxProfile::validate`](crate::session::SandboxProfile::validate): the
/// check needs the data directory and the database path, and `session` may not
/// reference [`crate::paths`]. A home or database path that is not valid UTF-8
/// simply narrows what there is to compare — the launch refuses that outright,
/// and that is where the user gets the whole sentence.
fn writable_roots_refusal(profile: &crate::session::SandboxProfile) -> Option<String> {
    let home = crate::paths::home_dir()
        .as_deref()
        .and_then(std::path::Path::to_str)
        .unwrap_or_default()
        .to_string();
    let declared: Vec<String> = profile.paths.iter().map(|p| p.expanded(&home)).collect();
    let writable: Vec<String> = profile
        .paths
        .iter()
        .filter(|p| p.mode.is_writable())
        .map(|p| p.expanded(&home))
        .collect();
    let db = crate::paths::database_file();
    crate::sandbox::check_writable_roots(&writable, db.as_deref().and_then(std::path::Path::to_str))
        .err()
        .or_else(|| crate::sandbox::check_declared_paths(&declared).err())
        .or_else(|| crate::sandbox::check_engine_socket_paths(&declared, Some(&home)).err())
}

/// Where an `n`-added sub-list row lands: after the selection, or at the end of
/// an empty list.
fn sublist_insert_index(selected: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (selected + 1).min(len)
    }
}

/// The sub-list row `selected` would swap with for a `delta` reorder, or `None`
/// at the ends of the list.
fn sublist_swap_target(selected: usize, delta: i32, len: usize) -> Option<usize> {
    let target = selected as i32 + delta;
    (target >= 0 && (target as usize) < len && selected < len).then_some(target as usize)
}

/// Step a `Copy` value through `values` by `delta`, wrapping at both ends.
/// The selector counterpart of [`cycle_field`], for the editor's enum rows.
fn cycle_value<T: PartialEq + Copy>(values: &[T], current: T, delta: i32) -> T {
    cycle_field(values, current, delta as isize)
}

/// The non-blank, trimmed values of a list of text rows.
fn non_empty_values(inputs: &[TextInput]) -> Vec<String> {
    inputs
        .iter()
        .map(|t| t.value().trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// `raw` trimmed, or `None` when it is blank.
fn trimmed_option(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Parse an optional whole-number resource cap. A typed value that doesn't
/// parse is an error rather than a silent "uncapped" — the same reason a
/// mistyped automation step delay refuses to become the default.
fn parse_limit(raw: &str, what: &str, unit: &str) -> Result<Option<u32>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("{what} must be a whole number of {unit}"))
}

/// Single, discriminated union replacing boolean flags for modal state.
/// Only one modal can be active at a time, making invalid states unrepresentable.
#[derive(Debug, Clone, Default)]
pub enum Modal {
    #[default]
    None,
    Help(HelpModal),
    BranchSelector(BranchSelectorModal),
    SyncBasePicker(SyncBasePickerModal),
    WorktreeName(WorktreeNameModal),
    AgentPicker(crate::ui::agent_picker_modal::AgentPickerState),
    HostPicker(crate::ui::host_picker_modal::HostPickerState),
    RestoreSessions(RestoreSessionsModal),
    /// Boxed: the editor form is by far the largest modal payload, and
    /// `Modal` is moved around per frame — see `clippy::large_enum_variant`.
    AutomationEditor(Box<AutomationEditorModal>),
    AutomationsList(AutomationsListModal),
    /// Read-only preview of what an automation would do on its next fire.
    AutomationDryRun(AutomationDryRunModal),
    SandboxList(SandboxListModal),
    /// The new-session wizard's sandbox step.
    SandboxPicker(crate::ui::sandbox_picker_modal::SandboxPickerState),
    /// Boxed for the same reason as [`Modal::AutomationEditor`]: an editor form
    /// is a large payload and `Modal` is moved around per frame.
    SandboxEditor(Box<SandboxEditorModal>),
    /// The egress firewall's first-use question: a sandboxed agent reached for
    /// a host its profile does not allow. Raised from the tick rather than by a
    /// keypress — see [`super::egress_prompts`].
    SandboxDomainPrompt(super::egress_prompts::DomainPrompt),
    RepoPicker(RepoPickerModal),
    ConversationPicker(super::cc_import::ConversationPickerModal),
    SessionName(SessionNameModal),
    ThemePicker(ThemePickerModal),
    TaskActionPicker(TaskActionPickerModal),
    ConfirmDelete(ConfirmDeleteModal),
    ConfirmRestore(ConfirmRestoreModal),
    Settings(SettingsModal),
}

impl Modal {
    pub fn close(&mut self) {
        *self = Modal::None;
    }

    pub fn is_open(&self) -> bool {
        !matches!(self, Modal::None)
    }

    /// For a modal with a selectable list, a mutable handle to its selection
    /// cursor plus the key chord a row-click replays to activate that row
    /// (`Enter` for the selectors and the F1 editor, `Ctrl+Space` for the repo
    /// picker — a row's action there is toggle/fold, `Enter` would confirm the
    /// whole modal on a misclick, and plain `Space` would type into the palette
    /// input). This is the **single** match over the selector modals, so the
    /// read path (`App::modal_selected_index`) and the write path
    /// (`App::select_modal_row`) can never drift onto different modal sets — a new
    /// selectable modal is wired into both at once by adding one arm here.
    pub(super) fn list_selection(&mut self) -> Option<(&mut usize, KeyCode, KeyModifiers)> {
        let enter = KeyModifiers::NONE;
        match self {
            Modal::Help(h) => Some((&mut h.selected, KeyCode::Enter, enter)),
            Modal::ThemePicker(tp) => Some((&mut tp.index, KeyCode::Enter, enter)),
            Modal::AgentPicker(ap) => Some((&mut ap.selected_index, KeyCode::Enter, enter)),
            Modal::HostPicker(hp) => Some((&mut hp.selected_index, KeyCode::Enter, enter)),
            Modal::BranchSelector(bs) => Some((&mut bs.index, KeyCode::Enter, enter)),
            Modal::SyncBasePicker(sb) => Some((&mut sb.index, KeyCode::Enter, enter)),
            Modal::TaskActionPicker(p) => Some((&mut p.selected, KeyCode::Enter, enter)),
            Modal::AutomationsList(al) => Some((&mut al.index, KeyCode::Enter, enter)),
            Modal::SandboxList(sl) => Some((&mut sl.index, KeyCode::Enter, enter)),
            Modal::SandboxPicker(sp) => Some((&mut sp.selected_index, KeyCode::Enter, enter)),
            Modal::RestoreSessions(rs) => Some((&mut rs.index, KeyCode::Enter, enter)),
            Modal::RepoPicker(rp) => Some((
                &mut rp.list_index,
                KeyCode::Char(' '),
                KeyModifiers::CONTROL,
            )),
            Modal::ConversationPicker(cp) => Some((&mut cp.list_index, KeyCode::Enter, enter)),
            _ => None,
        }
    }
}

// ── TaskActionPickerModal ────────────────────────────────────────────────

/// A trigger-time action for a task, chosen from the picker. Nothing is stored
/// on the task — the choice runs immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskActionChoice {
    /// Send the task title to an already-running session (id + display name).
    Send(crate::session::SessionId, String),
    /// Spawn a new session via the normal repo→agent flow, seeded with the title.
    SpawnNew,
}

impl TaskActionChoice {
    /// One-line label for the picker list.
    pub fn label(&self) -> String {
        match self {
            TaskActionChoice::Send(_, name) => format!("Send → {name}"),
            TaskActionChoice::SpawnNew => "Spawn new session…".to_string(),
        }
    }
}

/// Picker shown when triggering a task (`r`): pick where to run it.
#[derive(Debug, Clone)]
pub struct TaskActionPickerModal {
    pub task_id: i64,
    pub title: String,
    pub choices: Vec<TaskActionChoice>,
    pub selected: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::GitStats;

    fn stats(dirty: bool, files: usize, ins: usize, dels: usize, ahead: usize) -> GitStats {
        GitStats {
            files_changed: files,
            insertions: ins,
            deletions: dels,
            untracked: 0,
            dirty,
            ahead,
            behind: 0,
        }
    }

    #[test]
    fn delete_risk_clean_known_is_none() {
        // A single known-clean worktree → delete silently.
        assert_eq!(
            DeleteRisk::from_stats(&[Some(stats(false, 0, 0, 0, 0))]),
            None
        );
        // No worktrees at all is also "nothing at risk".
        assert_eq!(DeleteRisk::from_stats(&[]), None);
    }

    #[test]
    fn delete_risk_dirty_triggers() {
        let risk = DeleteRisk::from_stats(&[Some(stats(true, 2, 5, 1, 0))]).unwrap();
        assert!(risk.dirty);
        assert_eq!(risk.files_changed, 2);
        assert!(!risk.unknown);
    }

    #[test]
    fn delete_risk_ahead_triggers_even_when_clean() {
        let risk = DeleteRisk::from_stats(&[Some(stats(false, 0, 0, 0, 3))]).unwrap();
        assert_eq!(risk.ahead, 3);
        assert!(!risk.dirty);
    }

    #[test]
    fn delete_risk_untracked_only_triggers() {
        // Untracked files don't show in `diff HEAD` (files_changed == 0), so
        // exercise the untracked count as the sole trigger (dirty == false).
        let mut s = stats(false, 0, 0, 0, 0);
        s.untracked = 2;
        let risk = DeleteRisk::from_stats(&[Some(s)]).unwrap();
        assert_eq!(risk.untracked, 2);
        assert_eq!(risk.files_changed, 0);
        assert!(!risk.dirty, "untracked alone triggers even without dirty");
    }

    #[test]
    fn delete_risk_none_entry_forces_unknown() {
        // An uninspectable worktree (None) is treated as "can't prove clean".
        let risk = DeleteRisk::from_stats(&[None]).unwrap();
        assert!(risk.unknown);
        assert_eq!(DeleteRisk::unknown(), risk);
    }

    #[test]
    fn delete_risk_accumulates_across_worktrees() {
        let risk = DeleteRisk::from_stats(&[
            Some(stats(true, 1, 10, 2, 1)),
            Some(stats(false, 2, 30, 5, 4)),
        ])
        .unwrap();
        assert!(risk.dirty);
        assert_eq!(risk.files_changed, 3);
        assert_eq!(risk.insertions, 40);
        assert_eq!(risk.deletions, 7);
        assert_eq!(risk.ahead, 5);
        assert!(!risk.unknown);
    }

    #[test]
    fn test_text_input_basic() {
        let mut input = TextInput::new();
        input.insert('a');
        input.insert('b');
        input.insert('c');
        assert_eq!(input.value(), "abc");
        assert_eq!(input.cursor_pos(), 3);
    }

    #[test]
    fn cycle_field_wraps_both_directions() {
        let fields = ['a', 'b', 'c'];
        assert_eq!(cycle_field(&fields, 'a', 1), 'b');
        assert_eq!(cycle_field(&fields, 'c', 1), 'a'); // wrap forward
        assert_eq!(cycle_field(&fields, 'a', -1), 'c'); // wrap backward
        assert_eq!(cycle_field(&fields, 'b', -1), 'a');
        // Unknown current value falls back to index 0, then steps.
        assert_eq!(cycle_field(&fields, 'z', 1), 'b');
        // Empty slice returns the input unchanged.
        assert_eq!(cycle_field::<char>(&[], 'x', 1), 'x');
    }

    #[test]
    fn apply_text_input_key_edits_and_reports_handled() {
        let none = KeyModifiers::NONE;
        let mut input = TextInput::new();
        input.set("ab");
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('c'),
            none
        ));
        assert_eq!(input.value(), "abc");
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Backspace,
            none
        ));
        assert_eq!(input.value(), "ab");
        // Non-text keys are not handled.
        assert!(!apply_text_input_key(
            Some(&mut input),
            KeyCode::Enter,
            none
        ));
        assert!(!apply_text_input_key(Some(&mut input), KeyCode::Tab, none));
        // A text key with no focused field is still "handled" (a no-op).
        assert!(apply_text_input_key(None, KeyCode::Char('x'), none));
    }

    #[test]
    fn ctrl_chords_edit_text_and_never_insert_the_letter() {
        let ctrl = KeyModifiers::CONTROL;
        let mut input = TextInput::new();
        input.set("hello world");

        // Ctrl+W deletes the word before the cursor (and is reported handled).
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('w'),
            ctrl
        ));
        assert_eq!(input.value(), "hello ");

        // Ctrl+U kills to the start of the line.
        input.set("hello world");
        apply_text_input_key(Some(&mut input), KeyCode::Char('u'), ctrl);
        assert_eq!(input.value(), "");

        // Any other Ctrl+letter is swallowed — it must never insert the letter.
        input.set("ab");
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('x'),
            ctrl
        ));
        assert_eq!(input.value(), "ab");

        // A Ctrl+digit is likewise swallowed, never typed as text.
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('1'),
            ctrl
        ));
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn ctrl_non_letter_chords_fall_through_to_cursor_moves() {
        // Ctrl+<non-letter> (arrows, Home/End) is not a line-edit chord: it
        // must behave like the unmodified key so every text field is
        // consistent (a single-line modal moves the cursor just like the
        // repo-search field does).
        let ctrl = KeyModifiers::CONTROL;
        let mut input = TextInput::new();
        input.set("abc"); // cursor parked at the end

        assert!(apply_text_input_key(Some(&mut input), KeyCode::Left, ctrl));
        assert_eq!(input.cursor_pos(), 2);
        assert_eq!(input.value(), "abc", "Ctrl+Left must not delete text");

        assert!(apply_text_input_key(Some(&mut input), KeyCode::Home, ctrl));
        assert_eq!(input.cursor_pos(), 0);
    }

    #[test]
    fn text_input_word_and_line_deletes() {
        let mut input = TextInput::new();
        input.set("foo bar baz");
        input.delete_word_before();
        assert_eq!(input.value(), "foo bar ");
        input.delete_word_before();
        assert_eq!(input.value(), "foo ");
        input.set("trailing   ");
        input.delete_word_before(); // skips trailing spaces, then the word
        assert_eq!(input.value(), "");

        input.set("keep this");
        input.move_left(); // cursor before the final 's'
        input.kill_to_line_start();
        assert_eq!(input.value(), "s");
    }

    #[test]
    fn text_area_word_and_line_deletes_respect_lines() {
        let mut area = TextArea::new();
        area.set("first\nsecond word");
        // Ctrl+U deletes only to the start of the current (second) line.
        area.kill_to_line_start();
        assert_eq!(area.value(), "first\n");
        // Ctrl+W at the start of a line treats the newline as whitespace and
        // deletes back through it plus the previous word (readline behavior).
        area.set("first\nsecond");
        area.home(); // start of "second"
        area.delete_word_before();
        assert_eq!(area.value(), "second");
    }

    #[test]
    fn ctrl_chords_move_cursor_like_a_terminal() {
        // Ctrl+A/E to line ends, Ctrl+B/F by one char — emacs/readline moves
        // that must edit the cursor without ever inserting the bare letter.
        let ctrl = KeyModifiers::CONTROL;
        let mut input = TextInput::new();
        input.set("abcd"); // cursor at end (4)

        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('a'),
            ctrl
        ));
        assert_eq!(input.cursor_pos(), 0);
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('e'),
            ctrl
        ));
        assert_eq!(input.cursor_pos(), 4);
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('b'),
            ctrl
        ));
        assert_eq!(input.cursor_pos(), 3);
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('f'),
            ctrl
        ));
        assert_eq!(input.cursor_pos(), 4);
        // Uppercase (Shift+Ctrl) maps the same way.
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('A'),
            ctrl
        ));
        assert_eq!(input.cursor_pos(), 0);
        assert_eq!(input.value(), "abcd", "cursor chords must not change text");
    }

    #[test]
    fn ctrl_chords_delete_chars_and_kill_to_line_end() {
        let ctrl = KeyModifiers::CONTROL;
        let mut input = TextInput::new();

        // Ctrl+H deletes the char before the cursor (like Backspace).
        input.set("abc"); // cursor at end
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('h'),
            ctrl
        ));
        assert_eq!(input.value(), "ab");

        // Ctrl+D deletes the char under the cursor.
        input.set("abc");
        input.home(); // cursor before 'a'
        assert!(apply_text_input_key(
            Some(&mut input),
            KeyCode::Char('d'),
            ctrl
        ));
        assert_eq!(input.value(), "bc");

        // Ctrl+K kills from the cursor to the end of the line.
        input.set("hello world");
        input.home();
        input.move_right(); // after 'h'
        input.kill_to_line_end();
        assert_eq!(input.value(), "h");
    }

    #[test]
    fn text_area_kill_to_line_end_respects_lines() {
        let mut area = TextArea::new();
        // Ctrl+K mid-line kills only the rest of the current line.
        area.set("first line\nsecond");
        area.home(); // start of line 1 ("second")
        area.move_up(); // start of line 0 ("first line")
        for _ in 0..5 {
            area.move_right(); // park after "first"
        }
        area.kill_to_line_end();
        assert_eq!(area.value(), "first\nsecond");

        // At the end of a line (text below), Ctrl+K joins the next line.
        area.set("first\nsecond");
        area.home(); // start of line 1 ("second")
        area.move_up(); // line 0
        area.end(); // end of "first", before the newline
        area.kill_to_line_end();
        assert_eq!(area.value(), "firstsecond");
    }

    #[test]
    fn test_text_input_backspace() {
        let mut input = TextInput::new();
        input.set("hello");
        input.backspace();
        assert_eq!(input.value(), "hell");
        assert_eq!(input.cursor_pos(), 4);
    }

    #[test]
    fn test_text_input_cursor_movement() {
        let mut input = TextInput::new();
        input.set("hello");
        assert_eq!(input.cursor_pos(), 5);

        input.move_left();
        assert_eq!(input.cursor_pos(), 4);

        input.move_left();
        assert_eq!(input.cursor_pos(), 3);

        input.move_right();
        assert_eq!(input.cursor_pos(), 4);

        input.home();
        assert_eq!(input.cursor_pos(), 0);

        input.end();
        assert_eq!(input.cursor_pos(), 5);
    }

    #[test]
    fn test_modal_default_is_none() {
        let modal = Modal::default();
        assert!(matches!(modal, Modal::None));
    }

    #[test]
    fn test_modal_help_is_open() {
        let modal = Modal::Help(HelpModal::default());
        assert!(!matches!(modal, Modal::None));
    }

    #[test]
    fn test_modal_close() {
        let mut modal = Modal::Help(HelpModal::default());
        assert!(!matches!(modal, Modal::None));
        modal.close();
        assert!(matches!(modal, Modal::None));
    }

    #[test]
    fn test_text_input_with_unicode() {
        let mut input = TextInput::new();
        input.insert('ñ');
        input.insert('é');
        assert_eq!(input.cursor_pos(), 2);
        assert_eq!(input.value().len(), 4); // 2 bytes each for ñ and é
    }

    #[test]
    fn test_text_input_delete_at_cursor() {
        let mut input = TextInput::new();
        input.set("hello");
        input.move_left(); // Now at 'o'
        input.delete();
        assert_eq!(input.value(), "hell");
    }

    #[test]
    fn test_modal_state_transitions() {
        let mut modal = Modal::None;
        assert!(matches!(modal, Modal::None));

        modal = Modal::Help(HelpModal::default());
        assert!(!matches!(modal, Modal::None));

        modal.close();
        assert!(matches!(modal, Modal::None));
    }

    #[test]
    fn test_branch_selector_initial_state() {
        let branch = BranchSelectorModal::default();
        assert_eq!(branch.index, 0);
        assert_eq!(branch.branches.len(), 0);
    }

    #[test]
    fn test_text_input_equality() {
        let input1 = TextInput::new();
        let input2 = TextInput::default();
        assert_eq!(input1, input2);

        let mut input3 = TextInput::new();
        input3.set("test");
        assert_ne!(input1, input3);
    }

    #[test]
    fn test_automation_editor_default() {
        let modal = AutomationEditorModal::default();
        assert_eq!(modal.name.value(), "");
        assert_eq!(modal.field, AutomationField::Name);
        assert_eq!(modal.trigger_kind, TriggerKind::Daily);
        assert_eq!(modal.hour, 9);
        assert_eq!(modal.minute, 0);
        assert_eq!(modal.action, AutomationActionKind::Send);
        assert!(modal.enabled, "new automations default to enabled");
        assert!(modal.editing_id.is_none());
    }

    #[test]
    fn test_automation_editor_active_field() {
        let mut modal = AutomationEditorModal::default();
        modal.active_field_mut().unwrap().insert('x');
        assert_eq!(modal.name.value(), "x");
        // Selector/stepper fields have no text input.
        for f in [
            AutomationField::Trigger,
            AutomationField::Hour,
            AutomationField::Minute,
            AutomationField::Action,
        ] {
            modal.field = f;
            assert!(modal.active_field_mut().is_none(), "{f:?} is not text");
            assert!(modal.is_adjustable());
        }
    }

    #[test]
    fn test_automation_editor_daily_field_order_for_send() {
        let mut modal = AutomationEditorModal::default(); // Daily + Send
        let order: Vec<_> = (0..9)
            .map(|_| {
                let f = modal.field;
                modal.next_field();
                f
            })
            .collect();
        assert_eq!(
            order,
            vec![
                AutomationField::Name,
                AutomationField::Trigger,
                AutomationField::Hour,
                AutomationField::Minute,
                AutomationField::Timezone,
                AutomationField::Action,
                // Send exposes a target-session selector after the action.
                AutomationField::Target,
                // The step selector precedes the prompt it scopes.
                AutomationField::Step,
                AutomationField::Prompt,
            ]
        );
        assert_eq!(modal.field, AutomationField::Name);
    }

    #[test]
    fn test_automation_editor_steppers_wrap() {
        let mut modal = AutomationEditorModal {
            hour: 23,
            field: AutomationField::Hour,
            ..Default::default()
        };
        modal.adjust(1);
        assert_eq!(modal.hour, 0);
        modal.adjust(-1);
        assert_eq!(modal.hour, 23);

        modal.minute = 0;
        modal.field = AutomationField::Minute;
        modal.adjust(-1);
        assert_eq!(modal.minute, 59);

        modal.field = AutomationField::Trigger;
        modal.trigger_kind = TriggerKind::Once;
        modal.adjust(-1);
        assert_eq!(modal.trigger_kind, TriggerKind::Cron, "wraps backward");
    }

    #[test]
    fn test_automation_editor_build_schedule() {
        use crate::session::AutomationSchedule;
        let mut modal = AutomationEditorModal::default(); // Daily 09:00
        assert_eq!(
            modal.build_schedule(0).unwrap(),
            AutomationSchedule::Cron {
                expr: "0 9 * * *".into()
            }
        );

        modal.trigger_kind = TriggerKind::Weekdays;
        assert_eq!(
            modal.build_schedule(0).unwrap(),
            AutomationSchedule::Cron {
                expr: "0 9 * * 1-5".into()
            }
        );

        modal.trigger_kind = TriggerKind::Once;
        modal.delay.set("30m");
        assert_eq!(
            modal.build_schedule(1000).unwrap(),
            AutomationSchedule::Once {
                at: 1_800_000 + 1000
            }
        );

        modal.delay.set("bogus");
        assert!(modal.build_schedule(0).is_err());
    }

    #[test]
    fn test_automation_editor_spawn_shows_extra_fields() {
        let mut modal = AutomationEditorModal {
            action: AutomationActionKind::Spawn,
            ..Default::default()
        };
        assert!(modal.visible_fields().contains(&AutomationField::Repo));
        // The action cycles Send → Spawn → Exec → Send.
        modal.toggle_action();
        assert_eq!(modal.action, AutomationActionKind::Exec);
        assert!(modal.visible_fields().contains(&AutomationField::Command));
        // Exec has no prompt; it runs a command.
        assert!(!modal.visible_fields().contains(&AutomationField::Prompt));
        modal.toggle_action();
        assert_eq!(modal.action, AutomationActionKind::Send);
        assert!(!modal.visible_fields().contains(&AutomationField::Repo));
    }

    #[test]
    fn visible_fields_track_trigger_kind() {
        use AutomationField::*;
        let fields = |trigger| {
            AutomationEditorModal {
                trigger_kind: trigger,
                action: AutomationActionKind::Send,
                ..Default::default()
            }
            .visible_fields()
        };
        // A one-shot delay never shows a wall-clock timezone field.
        assert_eq!(
            fields(TriggerKind::Once),
            vec![Name, Trigger, Delay, Action, Target, Step, Prompt]
        );
        assert!(!fields(TriggerKind::Once).contains(&Timezone));
        // Wall-clock schedules carry a timezone and their time steppers.
        assert_eq!(
            fields(TriggerKind::Hourly),
            vec![Name, Trigger, Minute, Timezone, Action, Target, Step, Prompt]
        );
        assert_eq!(
            fields(TriggerKind::Daily),
            vec![Name, Trigger, Hour, Minute, Timezone, Action, Target, Step, Prompt]
        );
        assert_eq!(
            fields(TriggerKind::Weekdays),
            vec![Name, Trigger, Hour, Minute, Timezone, Action, Target, Step, Prompt]
        );
        assert_eq!(
            fields(TriggerKind::Weekly),
            vec![Name, Trigger, Weekday, Hour, Minute, Timezone, Action, Target, Step, Prompt]
        );
        assert_eq!(
            fields(TriggerKind::Cron),
            vec![Name, Trigger, CronExpr, Timezone, Action, Target, Step, Prompt]
        );
    }

    #[test]
    fn prompt_steps_add_remove_and_reorder() {
        let mut m = AutomationEditorModal::default();
        m.prompt_mut().set("first");
        m.add_step();
        assert_eq!(m.step_index, 1, "adding moves onto the new step");
        m.prompt_mut().set("second");
        assert_eq!(m.steps.len(), 2);

        // Reorder swaps the pair and follows the moved step.
        m.move_step(-1);
        assert_eq!(m.step_index, 0);
        assert_eq!(m.prompt().value(), "second");
        // A move past either end is a no-op, not a panic.
        m.move_step(-1);
        assert_eq!(m.step_index, 0);

        m.remove_step();
        assert_eq!(m.steps.len(), 1);
        assert_eq!(m.prompt().value(), "first");
        // Removing the last step clears it rather than leaving no prompt at all.
        m.remove_step();
        assert_eq!(m.steps.len(), 1);
        assert_eq!(m.prompt().value(), "");
    }

    #[test]
    fn step_field_binds_letters_to_list_edits() {
        let mut m = AutomationEditorModal {
            field: AutomationField::Step,
            ..Default::default()
        };
        m.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(m.steps.len(), 2, "`n` adds a step");
        m.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(m.steps.len(), 1, "`d` removes it");
        // ←/→ still walk the list (the letters don't shadow the selector).
        m.add_step();
        m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(m.step_index, 0);
    }

    #[test]
    fn step_delay_field_only_appears_with_more_than_one_step() {
        let mut m = AutomationEditorModal::default();
        assert!(!m.visible_fields().contains(&AutomationField::StepDelay));
        m.add_step();
        assert!(m.visible_fields().contains(&AutomationField::StepDelay));
    }

    #[test]
    fn build_steps_trims_and_drops_blanks() {
        let mut m = AutomationEditorModal::default();
        m.prompt_mut().set("  /model opus  ");
        m.add_step();
        m.prompt_mut().set("   "); // a blank step the user never filled in
        m.add_step();
        m.prompt_mut().set("go");
        m.current_step_mut().delay.set("500");

        let steps = m.build_steps().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].text, "/model opus");
        assert_eq!(steps[1].text, "go");
        assert_eq!(steps[1].delay_ms, Some(500));

        // An all-blank prompt yields nothing, so the caller can reject the save.
        let mut m = AutomationEditorModal::default();
        m.prompt_mut().set("   ");
        assert!(m.build_steps().is_err());
    }

    #[test]
    fn build_steps_rejects_an_unparsable_delay() {
        let mut m = AutomationEditorModal::default();
        m.prompt_mut().set("go");
        m.current_step_mut().delay.set("20O0"); // a typo, not a number
        let err = m
            .build_steps()
            .expect_err("a bad delay must not be dropped");
        assert!(err.contains("whole number"), "unexpected error: {err}");
    }

    #[test]
    fn spawn_action_shows_the_host_and_extra_repo_fields() {
        use AutomationField::*;
        let m = AutomationEditorModal {
            action: AutomationActionKind::Spawn,
            ..Default::default()
        };
        let fields = m.visible_fields();
        for f in [
            Repo,
            Worktree,
            BaseBranch,
            Agent,
            Host,
            SessionMode,
            ExtraRepos,
            ExtraDirs,
        ] {
            assert!(fields.contains(&f), "{f:?} should be visible for spawn");
        }
        // Exec swaps them for the command + its kill deadline, and no prompt.
        let m = AutomationEditorModal {
            action: AutomationActionKind::Exec,
            ..Default::default()
        };
        let fields = m.visible_fields();
        assert!(fields.contains(&Command));
        assert!(fields.contains(&Timeout));
        assert!(!fields.contains(&Prompt));
        assert!(!fields.contains(&Step));
    }

    #[test]
    fn agent_selector_keeps_an_agent_the_registry_lost() {
        let mut m = AutomationEditorModal::default();
        // Opening an automation whose agent was removed from agents.toml must
        // not silently rewrite it to the default — saving is what rejects it.
        m.set_agents(vec!["claude".into()], Some("retired"));
        assert_eq!(m.selected_agent(), Some("retired"));
        // The default choice is the leading empty entry.
        m.set_agents(vec!["claude".into()], None);
        assert_eq!(m.selected_agent(), None);
    }

    #[test]
    fn host_selector_defaults_to_local() {
        let mut m = AutomationEditorModal::default();
        m.set_hosts(vec!["devbox".into(), "wsl".into()], None);
        assert_eq!(m.selected_host(), None, "no host = local");
        m.set_hosts(vec!["devbox".into()], Some("devbox"));
        assert_eq!(m.selected_host(), Some("devbox"));
    }

    #[test]
    fn extra_repo_fields_round_trip_through_the_cli_grammar() {
        let extras = parse_extra_repo_fields("/a@main, /b", "/docs");
        assert_eq!(extras.len(), 3);
        assert!(extras[0].worktree);
        assert_eq!(extras[0].base_branch.as_deref(), Some("main"));
        assert!(extras[1].worktree);
        assert_eq!(extras[1].base_branch, None);
        assert!(!extras[2].worktree);

        // ...and back out to the two editor fields.
        assert_eq!(format_extra_repos(&extras, true), "/a@main, /b");
        assert_eq!(format_extra_repos(&extras, false), "/docs");
    }

    #[test]
    fn test_automations_list_modal_default() {
        let modal = AutomationsListModal::default();
        assert_eq!(modal.index, 0);
        assert!(modal.entries.is_empty());
    }

    #[test]
    fn repo_palette_typing_filters_and_clearing_restores() {
        let mut rp = RepoPickerModal::default();
        rp.push_row("/alpha".into(), RepoRowKind::Repo { child: false });
        rp.push_row("/beta".into(), RepoRowKind::Repo { child: false });
        rp.push_row("/gamma".into(), RepoRowKind::Repo { child: false });

        rp.input.set("bet");
        rp.recompute_filter();
        assert_eq!(rp.filtered_indices, vec![1]);

        rp.input.clear();
        rp.recompute_filter();
        assert_eq!(rp.filtered_indices, vec![0, 1, 2]);
    }

    #[test]
    fn repo_palette_input_mode_detects_paths_by_prefix() {
        let mut rp = RepoPickerModal::default();
        for (text, mode) in [
            ("", RepoInputMode::Filter),
            ("friring", RepoInputMode::Filter),
            ("with space", RepoInputMode::Filter),
            ("~", RepoInputMode::Path),
            ("~/code", RepoInputMode::Path),
            ("/abs/path", RepoInputMode::Path),
            ("./rel", RepoInputMode::Path),
            ("../up", RepoInputMode::Path),
        ] {
            rp.input.set(text);
            assert_eq!(rp.input_mode(), mode, "input {text:?}");
        }
    }

    /// A Windows drive path (`C:\…`, `C:/…`) is absolute, so it's path mode —
    /// without this, typing a native Windows path in the palette would be
    /// treated as a fuzzy filter and path completion would never engage.
    #[cfg(windows)]
    #[test]
    fn repo_palette_input_mode_detects_windows_paths() {
        let mut rp = RepoPickerModal::default();
        for p in ["C:\\repos", "C:/repos", ".\\rel", "..\\up"] {
            rp.input.set(p);
            assert_eq!(rp.input_mode(), RepoInputMode::Path, "input {p:?}");
        }
        rp.input.set("friring");
        assert_eq!(rp.input_mode(), RepoInputMode::Filter);
    }

    #[test]
    fn repo_palette_path_mode_does_not_filter_rows() {
        let mut rp = RepoPickerModal::default();
        rp.push_row("/alpha".into(), RepoRowKind::Repo { child: false });
        rp.push_row(PathBuf::new(), RepoRowKind::StartHere);
        // "~/zzz" matches nothing as a query — but it's a path, not a query.
        rp.input.set("~/zzz");
        rp.recompute_filter();
        assert_eq!(rp.filtered_indices, vec![0, 1]);
    }

    #[test]
    fn repo_palette_filter_hides_pinned_rows_and_selection_survives() {
        let mut rp = RepoPickerModal::default();
        rp.push_row("/alpha".into(), RepoRowKind::Repo { child: false });
        rp.push_row("/import-me".into(), RepoRowKind::ImportSuggestion);
        rp.push_row(PathBuf::new(), RepoRowKind::StartHere);
        rp.toggle_selected(std::path::Path::new("/alpha"));
        assert_eq!(rp.picked_count(), 1);

        // A filter that excludes /alpha hides it (and the pinned rows), but
        // the pick survives and still counts once visible again.
        rp.input.set("zzz");
        rp.recompute_filter();
        assert!(rp.filtered_indices.is_empty());
        assert_eq!(rp.picked_count(), 1);

        rp.input.clear();
        rp.recompute_filter();
        assert_eq!(rp.filtered_indices, vec![0, 1, 2]);
        assert!(rp.selected.contains(std::path::Path::new("/alpha")));
    }

    #[test]
    fn repo_rows_keep_selection_keyed_by_path() {
        let mut rp = RepoPickerModal::default();
        rp.push_row("/repo".into(), RepoRowKind::Repo { child: false });
        rp.push_row("/parent".into(), RepoRowKind::Header);
        rp.push_row("/parent/child".into(), RepoRowKind::Repo { child: true });

        rp.toggle_selected(std::path::Path::new("/repo"));
        rp.toggle_worktree(std::path::Path::new("/parent/child"));

        assert!(rp.selected.contains(std::path::Path::new("/repo")));
        // The worktree toggle checks the repo too.
        assert!(rp.selected.contains(std::path::Path::new("/parent/child")));
        assert!(rp.worktree.contains(std::path::Path::new("/parent/child")));

        // Rows can be rebuilt (even reshaped) without losing the flags.
        rp.rows.clear();
        rp.push_row("/parent/child".into(), RepoRowKind::Repo { child: false });
        assert!(rp.selected.contains(std::path::Path::new("/parent/child")));
        assert!(rp.worktree.contains(std::path::Path::new("/parent/child")));

        // Toggling worktree off leaves the selection alone.
        rp.toggle_worktree(std::path::Path::new("/parent/child"));
        assert!(!rp.worktree.contains(std::path::Path::new("/parent/child")));
        assert!(rp.selected.contains(std::path::Path::new("/parent/child")));
    }

    #[test]
    fn test_repo_picker_toggle_collapsed_ignores_non_header_rows() {
        let mut rp = RepoPickerModal::default();
        // Standalone repo, not a header.
        rp.push_row("/repo".into(), RepoRowKind::Repo { child: false });
        rp.toggle_collapsed(0);
        assert!(
            rp.collapsed.is_empty(),
            "non-header rows can't be collapsed"
        );
        // An out-of-range index is also a no-op (no panic).
        rp.toggle_collapsed(99);
        assert!(rp.collapsed.is_empty());
    }

    #[test]
    fn test_repo_picker_search_overrides_collapse() {
        let mut rp = RepoPickerModal::default();
        rp.push_row("/parent".into(), RepoRowKind::Header);
        rp.push_row("/parent/foo".into(), RepoRowKind::Repo { child: true });
        rp.push_row("/parent/bar".into(), RepoRowKind::Repo { child: true });
        rp.collapsed.insert("/parent".into());
        rp.recompute_filter();
        // Collapsed: only the header is visible.
        assert_eq!(rp.filtered_indices, vec![0]);

        // An active filter expands all so matching children are findable.
        rp.input.set("foo");
        rp.recompute_filter();
        // Header (always shown) + the matching child `foo`.
        assert_eq!(rp.filtered_indices, vec![0, 1]);
    }

    #[test]
    fn test_repo_picker_default_is_empty_filter_mode() {
        let mut rp = RepoPickerModal::default();
        assert_eq!(rp.input.value(), "");
        assert_eq!(rp.input_mode(), RepoInputMode::Filter);
        rp.recompute_filter();
        assert!(rp.filtered_indices.is_empty());
        assert_eq!(rp.list_index, 0);
    }

    #[test]
    fn test_automation_editor_from_spawn_automation() {
        use crate::session::{Automation, AutomationAction, AutomationSchedule};
        let auto = Automation {
            id: 7,
            name: "nightly".into(),
            enabled: false,
            schedule: AutomationSchedule::Cron {
                expr: "0 9 * * 1-5".into(),
            },
            timezone: Some("UTC".into()),
            action: AutomationAction::Spawn {
                repo_path: "/tmp/repo".into(),
                worktree_branch: Some("feat/x".into()),
                base_branch: None,
                agent: Some("codex".into()),
                extra_repos: Vec::new(),
                host: None,
                session_mode: Default::default(),
            },
            prompt: "triage".into(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            next_run_at: None,
            prompt_steps: Vec::new(),
        };
        let mut modal = AutomationEditorModal::from_automation(&auto);
        // The agent selector is populated by the caller, which owns the
        // registry (`App::populate_editor_registries`).
        modal.set_agents(vec!["claude".into(), "codex".into()], Some("codex"));
        assert_eq!(modal.editing_id, Some(7));
        assert!(!modal.enabled);
        // `0 9 * * 1-5` is recognized as the Weekdays preset at 09:00.
        assert_eq!(modal.trigger_kind, TriggerKind::Weekdays);
        assert_eq!(modal.hour, 9);
        assert_eq!(modal.minute, 0);
        assert_eq!(modal.action, AutomationActionKind::Spawn);
        assert_eq!(modal.repo.value(), "/tmp/repo");
        assert_eq!(modal.worktree.value(), "feat/x");
        assert_eq!(modal.selected_agent().unwrap_or_default(), "codex");
    }

    #[test]
    fn test_recognize_cron_presets_and_raw() {
        assert_eq!(
            recognize_cron("30 * * * *"),
            Some((TriggerKind::Hourly, 0, 30, 1))
        );
        assert_eq!(
            recognize_cron("0 9 * * *"),
            Some((TriggerKind::Daily, 9, 0, 1))
        );
        assert_eq!(
            recognize_cron("0 9 * * 1-5"),
            Some((TriggerKind::Weekdays, 9, 0, 1))
        );
        assert_eq!(
            recognize_cron("15 8 * * 3"),
            Some((TriggerKind::Weekly, 8, 15, 3))
        );
        // Anything irregular stays raw.
        assert_eq!(recognize_cron("0 9 1 * *"), None);
        assert_eq!(recognize_cron("*/5 * * * *"), None);
        assert_eq!(recognize_cron("0 9 * *"), None);
    }

    #[test]
    fn test_humanize_cron_presets_and_raw() {
        assert_eq!(humanize_cron("5 * * * *").as_deref(), Some("hourly :05"));
        assert_eq!(humanize_cron("0 9 * * *").as_deref(), Some("daily 09:00"));
        assert_eq!(
            humanize_cron("30 17 * * 1-5").as_deref(),
            Some("weekdays 17:30")
        );
        assert_eq!(humanize_cron("0 9 * * 1").as_deref(), Some("Mondays 09:00"));
        assert_eq!(
            humanize_cron("15 8 * * 0").as_deref(),
            Some("Sundays 08:15")
        );
        // Unrecognized shapes return None so the caller keeps the raw expression.
        assert_eq!(humanize_cron("*/5 * * * *"), None);
        assert_eq!(humanize_cron("0 9 1 * *"), None);
    }

    #[test]
    fn test_format_duration_short() {
        assert_eq!(format_duration_short(1_800_000), "30m");
        assert_eq!(format_duration_short(5_400_000), "1h30m");
        assert_eq!(format_duration_short(90_000_000), "1d1h");
        assert_eq!(format_duration_short(0), "0m");
    }

    #[test]
    fn task_editor_visible_fields_are_title_description_status() {
        let m = TaskEditorModal::new();
        assert_eq!(
            m.visible_fields(),
            vec![TaskField::Title, TaskField::Description, TaskField::Status]
        );
    }

    #[test]
    fn task_editor_save_and_cancel_outcomes() {
        let mut m = TaskEditorModal::new();
        assert_eq!(
            m.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            EditorOutcome::Save
        );
        assert_eq!(
            m.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            EditorOutcome::Cancel
        );
        // Typing edits the title field.
        m.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
        m.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(m.title.value(), "hi");
    }

    #[test]
    fn task_editor_status_selector_cycles_both_ways() {
        let mut m = TaskEditorModal::new();
        m.field = TaskField::Status;
        assert_eq!(m.status, crate::session::TaskStatus::Todo);
        m.adjust(1);
        assert_eq!(m.status, crate::session::TaskStatus::InProgress);
        m.adjust(-1);
        assert_eq!(m.status, crate::session::TaskStatus::Todo);
    }

    #[test]
    fn task_editor_active_field_tracks_focus_and_caret() {
        let mut m = TaskEditorModal::new();

        // Title is the default focus: typing moves its caret, and active_field()
        // reports that same field so the renderer can draw the cursor in place.
        m.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
        m.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(m.active_field().map(|f| f.value()), Some("hi"));
        assert_eq!(m.active_field().map(|f| f.cursor_pos()), Some(2));
        // Moving left mid-text is reflected by the caret (the rendering bug).
        m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(m.active_field().map(|f| f.cursor_pos()), Some(1));

        // Selector fields are not text inputs: active_field() is None.
        m.field = TaskField::Status;
        assert!(m.active_field().is_none());
    }

    #[test]
    fn textarea_inserts_newlines_and_moves_vertically() {
        let mut ta = TextArea::new();
        for c in "ab".chars() {
            ta.insert(c);
        }
        ta.insert_newline();
        for c in "cde".chars() {
            ta.insert(c);
        }
        assert_eq!(ta.value(), "ab\ncde");
        assert_eq!(ta.cursor_line_col(), (1, 3));

        // Up keeps the column, clamped to the shorter first line ("ab" → col 2).
        ta.move_up();
        assert_eq!(ta.cursor_line_col(), (0, 2));
        // Down returns to the second line at the clamped column.
        ta.move_down();
        assert_eq!(ta.cursor_line_col(), (1, 2));
    }

    #[test]
    fn textarea_backspace_joins_lines() {
        let mut ta = TextArea::new();
        ta.set("ab\ncd");
        // Cursor is at the end; home to start of line 2, then backspace joins.
        ta.home();
        assert_eq!(ta.cursor_line_col(), (1, 0));
        ta.backspace();
        assert_eq!(ta.value(), "abcd");
        assert_eq!(ta.cursor_line_col(), (0, 2));
    }

    #[test]
    fn task_editor_description_enter_inserts_newline_no_save() {
        let mut m = TaskEditorModal::new();
        m.field = TaskField::Description;
        m.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            m.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            EditorOutcome::Continue
        );
        m.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(m.description.value(), "a\nb");
    }

    #[test]
    fn task_editor_ctrl_s_saves_from_any_field() {
        let mut m = TaskEditorModal::new();
        m.field = TaskField::Description;
        assert_eq!(
            m.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            EditorOutcome::Save
        );
    }

    #[test]
    fn task_editor_from_task_populates_description() {
        let task = crate::session::Task {
            id: 1,
            title: "t".into(),
            description: Some("notes".into()),
            status: crate::session::TaskStatus::Todo,
            action: None,
            source: crate::session::SOURCE_LOCAL.into(),
            external_id: None,
            external_url: None,
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        };
        let m = TaskEditorModal::from_task(&task);
        assert_eq!(m.description.value(), "notes");
    }

    // ── TextInput: remaining cursor/edge cases ───────────────────────────

    #[test]
    fn text_input_edits_are_noops_at_buffer_edges() {
        let mut input = TextInput::new();
        input.set("ab");
        input.home();
        input.backspace(); // at start → no-op
        assert_eq!(input.value(), "ab");
        input.end();
        input.delete(); // at end → no-op
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn text_input_clear_resets_buffer_and_cursor() {
        let mut input = TextInput::new();
        input.set("hello");
        input.clear();
        assert_eq!(input.value(), "");
        assert_eq!(input.cursor_pos(), 0);
    }

    #[test]
    fn text_input_backspace_and_delete_multibyte() {
        let mut input = TextInput::new();
        input.set("añé");
        input.backspace();
        assert_eq!(input.value(), "añ");
        input.home();
        input.move_right();
        input.delete(); // removes the multi-byte 'ñ' at the cursor
        assert_eq!(input.value(), "a");
    }

    #[test]
    fn text_input_insert_str_drops_control_chars_and_advances_cursor() {
        let mut input = TextInput::new();
        input.set("a");
        input.insert_str("bc\nd\te"); // newline + tab are dropped
        assert_eq!(input.value(), "abcde");
        assert_eq!(input.cursor_pos(), 5);
    }

    #[test]
    fn textarea_insert_str_keeps_newlines_drops_carriage_returns() {
        let mut ta = TextArea::new();
        ta.insert_str("one\r\ntwo\tthree");
        // `\r` dropped, `\n` kept, `\t` (control) dropped.
        assert_eq!(ta.value(), "one\ntwothree");
    }

    #[test]
    fn text_input_move_right_clamps_at_end_and_insert_mid_string() {
        let mut input = TextInput::new();
        input.set("ab");
        // At the end already: move_right is a no-op (no overrun past len).
        input.move_right();
        assert_eq!(input.cursor_pos(), 2);
        // Insert in the middle keeps later chars intact.
        input.move_left();
        input.insert('X');
        assert_eq!(input.value(), "aXb");
        assert_eq!(input.cursor_pos(), 2);
    }

    // ── TextArea: methods not exercised by the existing happy-path test ──

    #[test]
    fn textarea_delete_removes_char_at_cursor() {
        let mut ta = TextArea::new();
        ta.set("abc");
        ta.home();
        ta.delete();
        assert_eq!(ta.value(), "bc");
        // Delete at end-of-buffer is a no-op.
        ta.end();
        ta.delete();
        assert_eq!(ta.value(), "bc");
    }

    #[test]
    fn textarea_horizontal_cursor_clamps_at_both_ends() {
        let mut ta = TextArea::new();
        ta.set("ab");
        ta.end();
        ta.move_right(); // already at end → no-op
        assert_eq!(ta.cursor_line_col(), (0, 2));
        ta.home();
        ta.move_left(); // already at start → no-op
        assert_eq!(ta.cursor_line_col(), (0, 0));
    }

    #[test]
    fn textarea_vertical_moves_clamp_at_first_and_last_line() {
        let mut ta = TextArea::new();
        ta.set("a\nbb");
        // On the last line: move_down stays put.
        ta.end();
        assert_eq!(ta.cursor_line_col(), (1, 2));
        ta.move_down();
        assert_eq!(ta.cursor_line_col(), (1, 2));
        // On the first line: move_up stays put.
        ta.set("a\nbb");
        ta.home(); // start of line 2
        ta.move_up(); // → line 1
        assert_eq!(ta.cursor_line_col(), (0, 0));
        ta.move_up(); // already top → no-op
        assert_eq!(ta.cursor_line_col(), (0, 0));
    }

    #[test]
    fn textarea_home_and_end_act_per_line() {
        let mut ta = TextArea::new();
        ta.set("abc\nde");
        // Cursor is at the end of line 2.
        ta.home();
        assert_eq!(ta.cursor_line_col(), (1, 0));
        ta.end();
        assert_eq!(ta.cursor_line_col(), (1, 2));
        // home/end stay within the current line, not the whole buffer.
        ta.move_up();
        ta.home();
        assert_eq!(ta.cursor_line_col(), (0, 0));
        ta.end();
        assert_eq!(ta.cursor_line_col(), (0, 3));
    }

    #[test]
    fn textarea_clear_resets_buffer_and_cursor() {
        let mut ta = TextArea::new();
        ta.set("a\nb");
        ta.clear();
        assert_eq!(ta.value(), "");
        assert_eq!(ta.cursor_line_col(), (0, 0));
    }

    // ── AutomationEditorModal::handle_key (the shared key state machine) ──

    #[test]
    fn automation_editor_handle_key_save_cancel_and_text() {
        let mut m = AutomationEditorModal::default();
        // Default field is Name (a text field): chars edit it, no save/cancel.
        assert_eq!(
            m.handle_key(KeyCode::Char('h'), KeyModifiers::NONE),
            EditorOutcome::Continue
        );
        m.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(m.name.value(), "hi");
        assert_eq!(
            m.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            EditorOutcome::Save
        );
        assert_eq!(
            m.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            EditorOutcome::Cancel
        );
    }

    #[test]
    fn automation_editor_handle_key_tab_navigates_and_ctrl_e_toggles_enabled() {
        let mut m = AutomationEditorModal::default(); // Daily + Send, field=Name
        assert!(m.enabled);
        m.handle_key(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(!m.enabled, "Ctrl+E flips enabled");
        // Tab / Down advance; BackTab / Up retreat (wrapping).
        m.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.field, AutomationField::Trigger);
        m.handle_key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(m.field, AutomationField::Name);
        m.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            m.field,
            AutomationField::Prompt,
            "Up from first wraps to last"
        );
    }

    #[test]
    fn automation_editor_prompt_enter_inserts_newline_not_save() {
        let mut m = AutomationEditorModal {
            field: AutomationField::Prompt,
            ..Default::default()
        };
        m.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            m.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            EditorOutcome::Continue,
            "Enter on the prompt inserts a newline, never saves"
        );
        m.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(m.prompt().value(), "a\nb");
    }

    #[test]
    fn automation_editor_ctrl_s_saves_from_prompt() {
        let mut m = AutomationEditorModal {
            field: AutomationField::Prompt,
            ..Default::default()
        };
        assert_eq!(
            m.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            EditorOutcome::Save
        );
    }

    #[test]
    fn automation_editor_prompt_up_down_move_within_text() {
        let mut m = AutomationEditorModal {
            field: AutomationField::Prompt,
            ..Default::default()
        };
        m.prompt_mut().set("a\nbb"); // cursor parks at the end → line 1
        assert_eq!(m.prompt().cursor_line_col().0, 1);
        m.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            m.prompt().cursor_line_col().0,
            0,
            "Up moves to the line above"
        );
        m.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(m.prompt().cursor_line_col().0, 1, "Down moves back");
        // Field navigation is unchanged: still on Prompt (Up/Down didn't nav).
        assert_eq!(m.field, AutomationField::Prompt);
    }

    #[test]
    fn automation_editor_prompt_tab_navigates_fields() {
        let mut m = AutomationEditorModal {
            field: AutomationField::Prompt,
            ..Default::default()
        }; // Daily + Send: Prompt is the last visible field.
        m.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            m.field,
            AutomationField::Name,
            "Tab from the last field wraps"
        );
        m.field = AutomationField::Prompt;
        m.handle_key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(
            m.field,
            AutomationField::Step,
            "BackTab steps to the prior field"
        );
    }

    #[test]
    fn automation_editor_ctrl_e_still_toggles_on_prompt() {
        let mut m = AutomationEditorModal {
            field: AutomationField::Prompt,
            ..Default::default()
        };
        assert!(m.enabled);
        assert_eq!(
            m.handle_key(KeyCode::Char('e'), KeyModifiers::CONTROL),
            EditorOutcome::Continue
        );
        assert!(
            !m.enabled,
            "Ctrl+E toggles enabled even while editing the prompt"
        );
        assert_eq!(m.prompt().value(), "", "Ctrl+E is not inserted as text");
    }

    #[test]
    fn automation_editor_handle_key_adjusts_selector_fields() {
        let mut m = AutomationEditorModal {
            field: AutomationField::Trigger,
            ..Default::default()
        }; // Daily
           // Right / Space step forward, Left steps back.
        m.handle_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(m.trigger_kind, TriggerKind::Weekdays);
        m.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(m.trigger_kind, TriggerKind::Weekly);
        m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(m.trigger_kind, TriggerKind::Weekdays);
        // On a text field, Left/Right move the caret rather than adjusting.
        m.field = AutomationField::Name;
        m.name.set("ab");
        m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(m.name.cursor_pos(), 1);
    }

    #[test]
    fn automation_editor_set_target_sessions_selects_and_falls_back() {
        let a = crate::session::SessionId::default();
        let b = crate::session::SessionId::default();
        let mut m = AutomationEditorModal::default();
        m.set_target_sessions(vec![(a, "A".into()), (b, "B".into())], Some(b));
        assert_eq!(m.target_index, 1);
        assert_eq!(m.selected_target().map(|(id, _)| *id), Some(b));
        // An id that isn't present falls back to the first session.
        let absent = crate::session::SessionId::default();
        m.set_target_sessions(vec![(a, "A".into())], Some(absent));
        assert_eq!(m.target_index, 0);
        // No sessions → no selected target.
        m.set_target_sessions(vec![], None);
        assert!(m.selected_target().is_none());
    }

    #[test]
    fn automation_editor_timezone_trims_and_blanks_to_none() {
        let mut m = AutomationEditorModal::default();
        assert!(m.timezone().is_none(), "empty timezone is None");
        m.timezone.set("  UTC  ");
        assert_eq!(m.timezone().as_deref(), Some("UTC"));
        m.timezone.set("   ");
        assert!(m.timezone().is_none(), "whitespace-only timezone is None");
    }

    // ── TaskEditorModal::handle_key field navigation + description keys ──

    #[test]
    fn task_editor_tab_navigates_fields() {
        let mut m = TaskEditorModal::new(); // field=Title
        m.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.field, TaskField::Description);
        m.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.field, TaskField::Status);
        m.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            m.field,
            TaskField::Title,
            "Tab wraps back to the first field"
        );
        // BackTab retreats and wraps.
        m.handle_key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(m.field, TaskField::Status);
    }

    #[test]
    fn task_editor_description_editing_keys_and_tab_navigation() {
        let mut m = TaskEditorModal::new();
        m.field = TaskField::Description;
        for c in "ac".chars() {
            m.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        // Left then insert 'b' in the middle.
        m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        m.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(m.description.value(), "abc");
        // Home + Delete removes the first char; End repositions to line end.
        m.handle_key(KeyCode::Home, KeyModifiers::NONE);
        m.handle_key(KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(m.description.value(), "bc");
        m.handle_key(KeyCode::End, KeyModifiers::NONE);
        m.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(m.description.value(), "b");
        // Tab still navigates out of the multi-line field (it is not inserted).
        assert_eq!(
            m.handle_key(KeyCode::Tab, KeyModifiers::NONE),
            EditorOutcome::Continue
        );
        assert_eq!(m.field, TaskField::Status);
    }

    #[test]
    fn task_editor_description_up_down_move_within_text() {
        let mut m = TaskEditorModal::new();
        m.field = TaskField::Description;
        for c in "ab".chars() {
            m.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        m.handle_key(KeyCode::Enter, KeyModifiers::NONE); // newline
        m.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(m.description.cursor_line_col(), (1, 1));
        m.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(m.description.cursor_line_col(), (0, 1));
        m.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(m.description.cursor_line_col(), (1, 1));
    }

    #[test]
    fn settings_order_lists_every_field_once() {
        assert_eq!(SettingsField::ORDER.len(), 25);
        for f in SettingsField::ORDER {
            assert_eq!(
                SettingsField::ORDER.iter().filter(|x| **x == f).count(),
                1,
                "{f:?} duplicated"
            );
        }
    }

    #[test]
    fn settings_space_toggles_bool_and_enter_does_too() {
        let mut m = SettingsModal::new(crate::session::settings::Settings::default());
        assert!(m.draft.features.tasks);
        m.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!m.draft.features.tasks);
        m.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(m.draft.features.tasks);
    }

    #[test]
    fn settings_scalar_steps_and_clamps() {
        let mut m = SettingsModal::new(crate::session::settings::Settings::default());
        m.field = SettingsField::ScrollbackLines;
        m.handle_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(m.draft.scrollback_lines, 1500);
        m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(m.draft.scrollback_lines, 1000);
        // Space adjusts scalars (does not toggle).
        m.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(m.draft.scrollback_lines, 1500);
        // Clamp at the floor.
        m.field = SettingsField::TwoPanelMinCols;
        for _ in 0..100 {
            m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        }
        assert_eq!(m.draft.two_panel_min_cols, 40);
    }

    #[test]
    fn settings_save_and_cancel_outcomes() {
        let mut m = SettingsModal::new(crate::session::settings::Settings::default());
        assert_eq!(
            m.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            EditorOutcome::Save
        );
        assert_eq!(
            m.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            EditorOutcome::Cancel
        );
    }

    #[test]
    fn settings_restart_required_tracks_only_restart_fields() {
        let mut m = SettingsModal::new(crate::session::settings::Settings::default());
        // A live flag changing does not arm the restart note.
        m.field = SettingsField::FeatTasks;
        m.toggle();
        assert!(!m.restart_required_changed());
        // A restart-required flag does.
        m.field = SettingsField::FeatMouse;
        m.toggle();
        assert!(m.restart_required_changed());
    }

    #[test]
    fn settings_restart_required_classification() {
        use SettingsField::*;
        for f in [
            FeatTasks,
            FeatFileViewer,
            FeatGlobalSearch,
            FeatDoubleShiftSearch,
            FeatInfoPanel,
            FeatShellPane,
            FeatCodeReview,
            FeatCcActivity,
            FeatPerfHud,
            FeatSoftDelete,
            InfoPanelPosition,
        ] {
            assert!(!f.restart_required(), "{f:?} should be live");
        }
        for f in [
            FeatMouse,
            FeatNotifications,
            FeatAutomations,
            ScrollbackLines,
        ] {
            assert!(f.restart_required(), "{f:?} should need restart");
        }
    }

    /// The per-field `restart_required` flag (UI marker) and
    /// `Settings::restart_only_differs` (the comparison driving the toast) are
    /// two encodings of the same live/restart split — toggling any
    /// restart-required field must register as a restart-only difference, and a
    /// live field must not.
    #[test]
    fn settings_restart_classifications_agree() {
        for field in SettingsField::ORDER {
            let mut m = SettingsModal::new(crate::session::settings::Settings::default());
            m.field = field;
            if field.is_scalar() {
                m.adjust(1);
            } else {
                m.toggle();
            }
            assert_eq!(
                m.draft.restart_only_differs(&m.original),
                field.restart_required(),
                "{field:?}: restart_required and restart_only_differs disagree",
            );
        }
    }

    // ── ThemePickerModal filtering ──────────────────────────────────────────

    fn theme_entry(name: &str, display: &str) -> ThemeEntry {
        ThemeEntry {
            name: name.to_string(),
            display_name: display.to_string(),
            palette: crate::session::ThemePalette::default(),
            is_light: false,
        }
    }

    #[test]
    fn theme_filter_matches_display_name_and_id_case_insensitively() {
        // A custom theme may name itself anything, so the *id* side must be
        // lowercased too — matching on the raw id would make "MyTheme"
        // unfindable by any lowercase query.
        let entries = vec![
            theme_entry("rose-pine-dawn", "Rosé Pine Dawn"),
            theme_entry("MyTheme", "Custom"),
            theme_entry("nord", "Nord"),
        ];

        // By display name, and by id.
        assert_eq!(ThemePickerModal::compute_matches(&entries, "dawn"), vec![0]);
        assert_eq!(
            ThemePickerModal::compute_matches(&entries, "rose-pine"),
            vec![0]
        );
        // Mixed-case id found by a lowercase query, and vice versa.
        assert_eq!(
            ThemePickerModal::compute_matches(&entries, "mytheme"),
            vec![1]
        );
        assert_eq!(ThemePickerModal::compute_matches(&entries, "NORD"), vec![2]);
        // An empty / whitespace query keeps everything.
        assert_eq!(
            ThemePickerModal::compute_matches(&entries, "   "),
            vec![0, 1, 2]
        );
        assert!(ThemePickerModal::compute_matches(&entries, "zzz").is_empty());
    }

    #[test]
    fn theme_refilter_keeps_the_cursor_on_the_same_theme() {
        let entries = vec![
            theme_entry("nord", "Nord"),
            theme_entry("dracula", "Dracula"),
            theme_entry("monokai", "Monokai"),
        ];
        let mut modal = ThemePickerModal {
            index: 2, // Monokai
            original: crate::session::ThemePalette::default(),
            filter: None,
            matches: (0..entries.len()).collect(),
        };
        modal.open_filter();
        let query = |modal: &mut ThemePickerModal, q: &str| {
            modal.filter.as_mut().expect("filter is open").set(q);
            modal.refilter(&entries);
        };

        // Narrowing to a set that still contains Monokai must follow it, not
        // stay on the old ordinal (which would now be a different theme).
        query(&mut modal, "o");
        assert_eq!(
            modal.selected_entry(),
            Some(2),
            "cursor should track the theme, not the index"
        );

        // Narrowing it away falls back to the first match.
        query(&mut modal, "dracula");
        assert_eq!(modal.selected_entry(), Some(1));

        // No match at all: nothing selected, and nothing panics.
        query(&mut modal, "zzz");
        assert_eq!(modal.selected_entry(), None);

        // Closing the filter restores the whole list, keeping the cursor on a
        // real theme rather than stranding it on the empty match set.
        modal.close_filter(&entries);
        assert_eq!(modal.matches.len(), entries.len());
        assert!(modal.selected_entry().is_some());
        assert!(modal.filter.is_none());
    }

    // ── Sandbox profiles ────────────────────────────────────────────────

    /// A minimal valid form: one named profile over one writable path.
    fn sandbox_editor() -> SandboxEditorModal {
        let mut m = SandboxEditorModal::default();
        m.name.set("dev");
        m.add_path();
        m.paths[0].text.set("~/dev/app");
        m.paths[0].mode = crate::session::PathMode::ReadWrite;
        m
    }

    fn sandbox_row(name: &str) -> crate::ui::sandbox_list_modal::SandboxProfileRow {
        crate::ui::sandbox_list_modal::SandboxProfileRow {
            name: name.to_string(),
            backend: crate::session::SandboxBackendKind::Auto,
            resolved: None,
            paths: 1,
            network: crate::session::NetworkMode::Allowlist,
            undecoded: Vec::new(),
            instance: None,
            unavailable: None,
        }
    }

    #[test]
    fn sandbox_list_selection_follows_the_cursor() {
        let mut list = SandboxListModal {
            index: 1,
            entries: vec![sandbox_row("a"), sandbox_row("b")],
        };
        assert_eq!(list.selected_name(), Some("b"));
        // A stale cursor names nothing rather than panicking.
        list.index = 9;
        assert_eq!(list.selected_name(), None);

        let mut modal = Modal::SandboxList(list);
        let (cursor, code, mods) = modal.list_selection().expect("the list is selectable");
        *cursor = 0;
        assert_eq!((code, mods), (KeyCode::Enter, KeyModifiers::NONE));
        let Modal::SandboxList(ref list) = modal else {
            unreachable!()
        };
        assert_eq!(list.selected_name(), Some("a"));
        modal.close();
        assert!(!modal.is_open());
    }

    #[test]
    fn sandbox_visible_fields_gate_on_the_sub_lists() {
        use SandboxField::*;
        // Nothing in either list: no row to point the per-entry fields at.
        let blank = SandboxEditorModal::default().visible_fields();
        assert_eq!(&blank[..3], &[Name, Backend, Paths]);
        for hidden in [PathText, PathMode, DomainText] {
            assert!(!blank.contains(&hidden), "{hidden:?}");
        }
        // Capabilities a backend may not honour stay listed — they render as
        // unavailable rather than vanishing.
        for shown in [
            PromptDomains,
            ReadScope,
            Memory,
            Cpus,
            Image,
            Containerfile,
            Fallback,
        ] {
            assert!(blank.contains(&shown), "{shown:?}");
        }

        let mut m = sandbox_editor();
        assert_eq!(
            &m.visible_fields()[..5],
            &[Name, Backend, Paths, PathText, PathMode]
        );
        assert!(!m.visible_fields().contains(&DomainText));
        m.add_domain();
        assert!(m.visible_fields().contains(&DomainText));
    }

    #[test]
    fn sandbox_tab_walks_the_visible_fields_and_wraps() {
        let mut m = sandbox_editor();
        let fields = m.visible_fields();
        for expected in fields.iter().skip(1) {
            m.next_field();
            assert_eq!(m.field, *expected);
        }
        m.next_field();
        assert_eq!(m.field, fields[0], "forward wrap");
        m.prev_field();
        assert_eq!(m.field, *fields.last().unwrap(), "backward wrap");
    }

    #[test]
    fn sandbox_path_sublist_adds_removes_and_reorders() {
        let mut m = sandbox_editor();
        m.field = SandboxField::Paths;
        m.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(m.paths.len(), 2);
        assert_eq!(m.path_index, 1, "the added row is selected");
        m.paths[1].text.set("/srv/shared");

        m.handle_key(KeyCode::Char('['), KeyModifiers::NONE);
        assert_eq!(m.paths[0].text.value(), "/srv/shared");
        assert_eq!(m.path_index, 0, "the selection rides the moved row");
        m.handle_key(KeyCode::Char('['), KeyModifiers::NONE);
        assert_eq!(m.path_index, 0, "a no-op at the top of the list");
        m.handle_key(KeyCode::Char(']'), KeyModifiers::NONE);
        assert_eq!(m.path_index, 1);

        m.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(m.paths.len(), 1);
        assert_eq!(m.path_index, 0);
        m.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(m.paths.is_empty(), "an empty list is representable here");
        assert!(!m.visible_fields().contains(&SandboxField::PathText));
        m.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(m.paths.is_empty(), "removing from an empty list is a no-op");
        // ← / → walk the list from the anchor.
        m.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        m.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        m.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(m.path_index, 0);
    }

    #[test]
    fn sandbox_domain_sublist_adds_and_removes_without_reordering() {
        let mut m = sandbox_editor();
        m.field = SandboxField::Domains;
        m.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        m.domains[0].set("github.com");
        m.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        m.domains[1].set("api.anthropic.com");
        assert_eq!(m.domain_index, 1);

        // The allow list's order carries no meaning, so `[` is unbound and
        // falls through without touching the list.
        m.handle_key(KeyCode::Char('['), KeyModifiers::NONE);
        assert_eq!(m.domains[0].value(), "github.com");

        m.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(m.domains.len(), 1);
        assert_eq!(m.domains[0].value(), "github.com");
    }

    #[test]
    fn sandbox_selectors_cycle_and_wrap() {
        use crate::session::{NetworkMode, PathMode, ReadScope, SandboxBackendKind};
        let mut m = sandbox_editor();

        m.field = SandboxField::Backend;
        m.adjust(1);
        assert_eq!(m.backend, SandboxBackendKind::Seatbelt);
        m.adjust(-1);
        assert_eq!(m.backend, SandboxBackendKind::Auto);
        m.adjust(-1);
        assert_eq!(m.backend, *SandboxBackendKind::ALL.last().unwrap());
        m.backend = SandboxBackendKind::Auto;

        m.field = SandboxField::Network;
        m.adjust(1);
        assert_eq!(m.network_mode, NetworkMode::Full);
        m.adjust(1);
        assert_eq!(m.network_mode, NetworkMode::None);

        m.field = SandboxField::ReadScope;
        m.adjust(1);
        assert_eq!(m.read_scope, ReadScope::Workspace);

        m.field = SandboxField::PathMode;
        assert_eq!(m.paths[0].mode, PathMode::ReadWrite);
        m.adjust(1);
        assert_eq!(m.paths[0].mode, PathMode::ReadOnly);
    }

    #[test]
    fn sandbox_space_toggles_the_booleans() {
        let mut m = sandbox_editor();
        m.field = SandboxField::PromptDomains;
        assert!(m.prompt_new_domains);
        m.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!m.prompt_new_domains);
        m.field = SandboxField::Fallback;
        m.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(m.allow_unsandboxed_fallback);
    }

    #[test]
    fn sandbox_typing_reaches_the_selected_sub_list_row() {
        let mut m = sandbox_editor();
        m.add_path();
        m.field = SandboxField::PathText;
        for c in "/srv".chars() {
            m.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(m.paths[1].text.value(), "/srv");
        assert_eq!(
            m.paths[0].text.value(),
            "~/dev/app",
            "the other row is untouched"
        );
        assert_eq!(m.active_cursor(), 4);
        // A selector draws no caret.
        m.field = SandboxField::PathMode;
        assert_eq!(m.active_cursor(), 0);
    }

    #[test]
    fn sandbox_unavailable_fields_refuse_input() {
        use crate::session::SandboxBackendKind;
        let mut m = sandbox_editor();

        m.backend = SandboxBackendKind::Seatbelt;
        m.field = SandboxField::Memory;
        assert!(!m.field_available(SandboxField::Memory));
        m.handle_key(KeyCode::Char('4'), KeyModifiers::NONE);
        assert_eq!(m.memory.value(), "");

        // The mirror case: a place backend has no host read scope to widen.
        m.backend = SandboxBackendKind::Docker;
        m.field = SandboxField::ReadScope;
        let before = m.read_scope;
        m.handle_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(m.read_scope, before);

        // An unresolved `auto` rules nothing out; a probe narrows it.
        m.backend = SandboxBackendKind::Auto;
        assert!(m.field_available(SandboxField::Memory));
        m.resolved = Some(SandboxBackendKind::Bwrap);
        assert!(!m.field_available(SandboxField::Memory));
        assert_eq!(m.effective_backend(), SandboxBackendKind::Bwrap);
    }

    #[test]
    fn sandbox_prompt_toggle_needs_the_allowlist() {
        use crate::session::NetworkMode;
        let mut m = sandbox_editor();
        assert!(m.field_available(SandboxField::PromptDomains));
        for mode in [NetworkMode::None, NetworkMode::Full] {
            m.network_mode = mode;
            assert!(!m.field_available(SandboxField::PromptDomains), "{mode}");
        }
    }

    #[test]
    fn sandbox_build_profile_drops_blank_rows_and_trims() {
        let mut m = sandbox_editor();
        m.add_path(); // added, then abandoned
        m.add_domain();
        m.domains[0].set("  github.com:443  ");
        m.add_domain();
        let p = m.build_profile().unwrap();
        assert_eq!(p.paths.len(), 1);
        assert_eq!(p.paths[0].path, "~/dev/app");
        assert_eq!(p.network_allow, ["github.com:443"]);
        p.validate().unwrap();
    }

    #[test]
    fn sandbox_build_profile_omits_what_the_backend_cannot_honour() {
        use crate::session::SandboxBackendKind;
        let mut m = sandbox_editor();
        m.backend = SandboxBackendKind::Docker;
        m.memory.set("2048");
        m.cpus.set("2");
        m.image.set("ghcr.io/example/dev:latest");
        let place = m.build_profile().unwrap();
        assert_eq!(place.memory_mb, Some(2048));
        assert_eq!(place.cpus, Some(2));
        assert_eq!(place.image.as_deref(), Some("ghcr.io/example/dev:latest"));
        place.validate().unwrap();

        // The same form on a policy backend saves a profile the validator
        // accepts, and keeps the typed values for a switch back.
        m.backend = SandboxBackendKind::Seatbelt;
        let policy = m.build_profile().unwrap();
        assert_eq!(policy.memory_mb, None);
        assert_eq!(policy.cpus, None);
        assert_eq!(policy.image, None);
        policy.validate().unwrap();
        assert_eq!(m.memory.value(), "2048");
    }

    #[test]
    fn sandbox_build_profile_rejects_a_mistyped_limit() {
        use crate::session::SandboxBackendKind;
        let mut m = sandbox_editor();
        m.backend = SandboxBackendKind::Docker;
        m.memory.set("2 gigs");
        assert_eq!(
            m.build_profile().unwrap_err(),
            "Memory limit must be a whole number of megabytes"
        );
        m.memory.clear();
        m.cpus.set("half");
        assert_eq!(
            m.build_profile().unwrap_err(),
            "CPU limit must be a whole number of CPUs"
        );
    }

    #[test]
    fn sandbox_build_profile_keeps_deny_entries_it_cannot_edit() {
        let mut m = sandbox_editor();
        m.deny = vec!["gist.github.com".to_string()];
        assert_eq!(m.build_profile().unwrap().network_deny, ["gist.github.com"]);
    }

    #[test]
    fn sandbox_validation_surfaces_one_sentence_for_the_toast() {
        let mut m = SandboxEditorModal::default();
        assert_eq!(
            m.validated_profile(&[]).unwrap_err(),
            "Name cannot be empty"
        );
        m.name.set("dev");
        assert_eq!(
            m.validated_profile(&[]).unwrap_err(),
            "Add at least one path the sandbox can see"
        );
        m.add_path();
        m.paths[0].text.set("~/dev/app");
        m.validated_profile(&[]).unwrap();
        assert!(m
            .validated_profile(&["DEV".to_string()])
            .unwrap_err()
            .contains("already exists"));
    }

    /// A read-write path reaching friring's data directory is refused at
    /// launch; the editor refuses to store it in the first place, so the
    /// profile never becomes a row that lists fine and fails every session
    /// picking it.
    #[test]
    fn a_profile_reaching_the_database_is_refused_by_the_save_not_only_the_launch() {
        let data_dir = crate::paths::log_directory().expect("a test build pins the data directory");
        let ancestor = data_dir
            .parent()
            .expect("the data directory has a parent")
            .to_string_lossy()
            .into_owned();

        let mut m = sandbox_editor();
        m.paths[0].text.set(&ancestor);
        m.paths[0].mode = crate::session::PathMode::ReadWrite;
        let err = m
            .validated_profile(&[])
            .expect_err("a read-write path above the data directory cannot be saved");
        assert!(err.contains("ADR-29"), "{err}");
        assert!(
            err.contains(&data_dir.to_string_lossy().into_owned()),
            "{err}"
        );

        // A path *inside* the data directory encloses nothing above it, so the
        // ADR-29 rule passes it — and it is refused anyway, in **either** mode:
        // `<data>/sandbox` holds the other profiles' sandbox logins, the markers
        // that keep one credential to one boundary, the generated policies and
        // the other sessions' egress sockets, every one of which is taken by
        // being read.
        for path in [
            ancestor.clone(),
            data_dir.join("sandbox").to_string_lossy().into_owned(),
            data_dir
                .join("sandbox/pl/other/home")
                .to_string_lossy()
                .into_owned(),
        ] {
            for mode in [
                crate::session::PathMode::ReadOnly,
                crate::session::PathMode::ReadWrite,
            ] {
                m.paths[0].text.set(&path);
                m.paths[0].mode = mode;
                assert!(
                    m.validated_profile(&[]).is_err(),
                    "'{path}' must not be storable as {mode:?}"
                );
            }
        }

        // A path that is none of friring's own still saves in either mode.
        m.paths[0].text.set("/fabricated/home/dev/app");
        m.paths[0].mode = crate::session::PathMode::ReadWrite;
        m.validated_profile(&[]).unwrap();
    }

    #[test]
    fn sandbox_editor_round_trips_an_existing_profile() {
        let mut p = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        p.network_allow = vec!["github.com:443".to_string()];
        p.created_at = 17;
        let m = SandboxEditorModal::from_profile(&p);
        assert_eq!(m.editing.as_deref(), Some("dev"));
        // Re-saving under its own name is not a collision, and the creation
        // stamp survives.
        let saved = m
            .validated_profile(&["dev".to_string(), "other".to_string()])
            .unwrap();
        assert_eq!(saved, p);
        // The blank form is a *new* profile, not an edit of an unnamed one.
        assert!(SandboxEditorModal::default().editing.is_none());
    }

    #[test]
    fn sandbox_editor_outcomes_match_the_footer_buttons() {
        let mut m = sandbox_editor();
        assert_eq!(
            m.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            EditorOutcome::Save
        );
        assert_eq!(
            m.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            EditorOutcome::Save
        );
        assert_eq!(
            m.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            EditorOutcome::Cancel
        );
        assert_eq!(
            m.handle_key(KeyCode::Tab, KeyModifiers::NONE),
            EditorOutcome::Continue
        );
    }
}
