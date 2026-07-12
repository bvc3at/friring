//! Import an existing Claude Code conversation as a new session.
//!
//! Friring normally tracks only conversations it started (it pins
//! `agent_session_id` at spawn). This flow closes the gap for conversations
//! started *outside* Friring: it scans every top-level transcript under
//! `~/.claude/projects/*/<uuid>.jsonl` off the UI thread, lets the user pick
//! one from a fuzzy-searchable list, choose the launch directory (default: the
//! conversation's original cwd), and spawns a session whose config pins both
//! `agent_session_id` *and* `resume_session_id` to the conversation id — so the
//! agent relaunches with `--resume <id>` and every id-keyed feature (status
//! hooks, F9 activity, restart) works as if Friring had started it.
//!
//! `claude --resume <id>` only finds transcripts under the slug dir of its
//! *current* directory (verified against v2.1.206), so importing into a
//! different directory first **stages** the transcript: copy the newest
//! `<id>.jsonl` into `projects/<slug-of-destination>/` (see
//! [`stage_transcript_for_resume`]). The original file is never touched.
//!
//! Pure line parsing lives in [`crate::session::cc_activity`]
//! (`parse_conversation_head`); this module is the filesystem glue, the picker
//! modal state, and its key handling. Local sessions only: the scan walks the
//! local `~/.claude`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::session::cc_activity::parse_conversation_head;

use super::{background, modals, App};

/// How much of a transcript's head the scan reads for identity metadata. The
/// cwd/branch/title all appear within the first real entries; the only large
/// early line observed is a `file-history-snapshot`, whose head instance is
/// small (backups accumulate later). Bounding the read keeps a scan over
/// hundreds of multi-megabyte transcripts cheap.
const CONVO_HEAD_BYTES: u64 = 256 * 1024;

/// One importable conversation found on disk. When the same session id exists
/// under several project dirs (a previously staged import), the scan keeps only
/// the newest copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcConversation {
    /// The Claude Code session id (the `<uuid>` of `<uuid>.jsonl`).
    pub id: String,
    /// The transcript file the metadata was read from (the newest copy).
    pub path: PathBuf,
    /// The conversation's original working directory, when recorded.
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    /// Best-available title: Claude Code's own summary line, else the first
    /// typed user prompt.
    pub title: Option<String>,
    /// Transcript file mtime (ms since epoch) — the "last active" shown in the
    /// picker and the sort key.
    pub mtime_ms: u64,
}

impl CcConversation {
    /// The single-line text the picker both fuzzy-filters and renders (the
    /// match positions must map onto what is displayed, so it is one string):
    /// a first-line, length-capped title plus the tilde-shortened directory.
    pub fn display_text(&self) -> String {
        let title = self
            .title
            .as_deref()
            .map(first_line_capped)
            .unwrap_or_else(|| "(no prompt)".to_string());
        match &self.cwd {
            Some(cwd) => format!("{title} — {}", crate::paths::display_path(cwd)),
            None => title,
        }
    }
}

/// First line of a title, whitespace-normalized and capped so the directory
/// stays visible next to it in a picker row.
fn first_line_capped(s: &str) -> String {
    let line = s
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// Which section of the conversation picker is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConversationPickerFocus {
    /// The conversation list.
    #[default]
    List,
    /// The fuzzy search filter input.
    Search,
    /// The working-directory input (the second step, after a conversation is
    /// chosen).
    Dir,
}

/// State of the conversation-import picker modal: the scanned entries, the
/// fuzzy filter over them, and the directory step's input.
#[derive(Debug, Clone, Default)]
pub struct ConversationPickerModal {
    /// True while the off-thread scan is still running (the list shows a
    /// scanning placeholder).
    pub loading: bool,
    pub entries: Vec<CcConversation>,
    /// Cursor index into `filtered_indices`.
    pub list_index: usize,
    pub search_input: modals::TextInput,
    /// Indices into `entries` matching the current search query.
    pub filtered_indices: Vec<usize>,
    pub focus: ConversationPickerFocus,
    /// Index into `entries` of the conversation confirmed with Enter; `Some`
    /// means the modal is in its directory step.
    pub chosen: Option<usize>,
    pub dir_input: modals::TextInput,
    /// Fish-style completion suffix for the directory input.
    pub dir_suggestion: Option<String>,
    /// Registry agent the import will relaunch with (resolved at open; the
    /// first agent whose `resume_args` take an id).
    pub agent: String,
}

impl ConversationPickerModal {
    /// Rebuild `filtered_indices` from the current search query, keeping
    /// `list_index` in range. Mirrors the repo picker's filter.
    pub fn recompute_filter(&mut self) {
        let query = self.search_input.value().to_string();
        self.filtered_indices = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                query.is_empty() || crate::fuzzy::fuzzy_match(&query, &e.display_text()).is_some()
            })
            .map(|(i, _)| i)
            .collect();
        if self.list_index >= self.filtered_indices.len() {
            self.list_index = self.filtered_indices.len().saturating_sub(1);
        }
    }

    /// The entry under the list cursor, if any.
    fn entry_under_cursor(&self) -> Option<usize> {
        self.filtered_indices.get(self.list_index).copied()
    }
}

/// Scan `projects/*/<uuid>.jsonl` for importable conversations, newest first.
/// `exclude` drops conversations Friring already tracks (a live session's
/// `agent_session_id`) — importing one would race the running agent on the same
/// transcript. Duplicated ids (earlier stagings) keep only the newest copy.
fn scan_conversations(projects: &Path, exclude: &HashSet<String>) -> Vec<CcConversation> {
    let mut by_id: HashMap<String, CcConversation> = HashMap::new();
    let slug_dirs = std::fs::read_dir(projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir());
    for dir in slug_dirs {
        let files = std::fs::read_dir(&dir).into_iter().flatten().flatten();
        for entry in files {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Only session transcripts are `<uuid>.jsonl`; anything else in a
            // project dir is not a conversation.
            if uuid::Uuid::parse_str(id).is_err() || exclude.contains(id) {
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            if md.len() == 0 {
                continue;
            }
            let mtime_ms = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if by_id.get(id).is_some_and(|prev| prev.mtime_ms >= mtime_ms) {
                continue;
            }
            let meta = parse_conversation_head(&read_head(&path));
            // No cwd *and* no prompt = an empty shell (opened and abandoned)
            // or not a conversation at all — nothing worth resuming.
            if meta.cwd.is_none() && meta.title.is_none() {
                continue;
            }
            by_id.insert(
                id.to_string(),
                CcConversation {
                    id: id.to_string(),
                    path,
                    cwd: meta.cwd.map(PathBuf::from),
                    git_branch: meta.git_branch,
                    title: meta.title,
                    mtime_ms,
                },
            );
        }
    }
    let mut out: Vec<CcConversation> = by_id.into_values().collect();
    out.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Read up to [`CONVO_HEAD_BYTES`] of a transcript, lossily decoded. A cut-off
/// final line is skipped by the parser like any malformed line.
fn read_head(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = file.take(CONVO_HEAD_BYTES).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Make `--resume <id>` work from `canonical_dest_cwd`: ensure the transcript
/// exists under that directory's `projects/<slug>/` (Claude Code only looks
/// there — see the module doc). Copies the picked (newest) transcript in;
/// an existing same-or-newer destination copy is kept, so re-importing never
/// rolls a continued conversation back.
fn stage_transcript_for_resume(
    projects: &Path,
    src: &Path,
    id: &str,
    canonical_dest_cwd: &Path,
) -> std::io::Result<()> {
    let dest_dir = projects.join(crate::paths::claude_project_slug(canonical_dest_cwd));
    let dest = dest_dir.join(format!("{id}.jsonl"));
    if dest == src {
        return Ok(());
    }
    if let (Ok(dm), Ok(sm)) = (dest.metadata(), src.metadata()) {
        if let (Ok(d), Ok(s)) = (dm.modified(), sm.modified()) {
            if d >= s {
                return Ok(());
            }
        }
    }
    std::fs::create_dir_all(&dest_dir)?;
    std::fs::copy(src, &dest)?;
    Ok(())
}

/// A session name suggested from the conversation title (editable in the name
/// modal). Short enough for the session list; empty when there is no title.
fn suggest_session_name(convo: &CcConversation) -> String {
    let Some(title) = convo.title.as_deref() else {
        return String::new();
    };
    let line = title
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    line.chars()
        .take(30)
        .collect::<String>()
        .trim_end()
        .to_string()
}

impl App {
    /// Open the conversation-import picker (`i` in the session list): resolve
    /// the relaunch agent, kick the off-thread scan, and show the modal in its
    /// loading state. [`Self::poll_conversation_import`] fills it in.
    pub(super) fn start_conversation_import(&mut self) {
        let Some(agent) = self.resume_capable_agent() else {
            self.set_error(
                "No agent in agents.toml can resume by id (needs resume_args with {id})",
            );
            return;
        };
        let Some(projects) = crate::paths::claude_projects_dir(None) else {
            self.set_error("Cannot resolve the Claude Code projects directory");
            return;
        };
        // An import is always local and parentless — clear anything a
        // cancelled wizard/fork/task flow left behind (mirrors `act_new_session`).
        self.new_session.backend = None;
        self.new_session.parent_session_id = None;
        self.task_ui.pending_task_prompt = None;

        self.modal = modals::Modal::ConversationPicker(ConversationPickerModal {
            loading: true,
            agent,
            ..Default::default()
        });

        // A still-running scan (modal closed and reopened) will fill the fresh
        // modal from `poll_conversation_import`; don't double-start.
        if self.conversation_scan.in_progress() {
            return;
        }
        let exclude: HashSet<String> = self
            .sessions
            .iter()
            .filter_map(|s| s.info.agent_session_id.clone())
            .collect();
        let tx = self.conversation_scan.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(scan_conversations(&projects, &exclude));
        });
    }

    /// The registry agent an import relaunches with: the default agent when it
    /// resumes by id, else the first agent that does (in practice `claude` —
    /// the scanned on-disk layout is Claude Code's).
    fn resume_capable_agent(&self) -> Option<String> {
        self.agents
            .default_agent()
            .filter(|a| a.resumes_by_id())
            .or_else(|| self.agents.agents.iter().find(|a| a.resumes_by_id()))
            .map(|a| a.name.clone())
    }

    /// Apply a finished conversation scan to the picker (dropped when the
    /// modal was closed in the meantime).
    pub(super) fn poll_conversation_import(&mut self) {
        let entries = match self.conversation_scan.poll() {
            background::TaskPoll::Pending => return,
            background::TaskPoll::Died => {
                if let modals::Modal::ConversationPicker(ref mut cp) = self.modal {
                    cp.loading = false;
                }
                self.set_error("Conversation scan failed (worker died)");
                return;
            }
            background::TaskPoll::Done(entries) => entries,
        };
        if let modals::Modal::ConversationPicker(ref mut cp) = self.modal {
            cp.loading = false;
            cp.entries = entries;
            cp.recompute_filter();
        }
    }

    pub(super) fn handle_conversation_picker_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let modals::Modal::ConversationPicker(ref cp) = self.modal else {
            return;
        };
        match cp.focus {
            ConversationPickerFocus::List => self.handle_convo_list_key(code),
            ConversationPickerFocus::Search => self.handle_convo_search_key(code, mods),
            ConversationPickerFocus::Dir => self.handle_convo_dir_key(code, mods),
        }
    }

    fn handle_convo_list_key(&mut self, code: KeyCode) {
        let modals::Modal::ConversationPicker(ref mut cp) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.modal.close();
            }
            KeyCode::Char('/') => {
                cp.search_input.clear();
                cp.list_index = 0;
                cp.recompute_filter();
                cp.focus = ConversationPickerFocus::Search;
            }
            KeyCode::Char('j') | KeyCode::Down if cp.list_index + 1 < cp.filtered_indices.len() => {
                cp.list_index += 1;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                cp.list_index = cp.list_index.saturating_sub(1);
            }
            KeyCode::Enter => self.choose_conversation(),
            _ => {}
        }
    }

    /// `Enter` on the list: advance to the directory step, prefilled with the
    /// conversation's original cwd (tilde-shortened, like it will be typed).
    fn choose_conversation(&mut self) {
        let modals::Modal::ConversationPicker(ref mut cp) = self.modal else {
            return;
        };
        let Some(real_idx) = cp.entry_under_cursor() else {
            return;
        };
        cp.chosen = Some(real_idx);
        // Full path, not `display_path` (basename) — the input's value is what
        // gets expanded on Enter, and a bare basename would resolve relative.
        let prefill = cp.entries[real_idx]
            .cwd
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        cp.dir_input.set(&prefill);
        cp.focus = ConversationPickerFocus::Dir;
        self.update_convo_dir_suggestion();
    }

    fn handle_convo_search_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let modals::Modal::ConversationPicker(ref mut cp) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => {
                cp.search_input.clear();
                cp.list_index = 0;
                cp.recompute_filter();
                cp.focus = ConversationPickerFocus::List;
            }
            KeyCode::Enter => {
                cp.focus = ConversationPickerFocus::List;
            }
            // Cursor moves don't change the filter; edits (incl. Ctrl+W/U) do.
            KeyCode::Left => cp.search_input.move_left(),
            KeyCode::Right => cp.search_input.move_right(),
            KeyCode::Home => cp.search_input.home(),
            KeyCode::End => cp.search_input.end(),
            other => {
                if modals::apply_text_input_key(Some(&mut cp.search_input), other, mods) {
                    cp.recompute_filter();
                }
            }
        }
    }

    fn handle_convo_dir_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let modals::Modal::ConversationPicker(ref mut cp) = self.modal else {
            return;
        };
        match code {
            // Esc steps back to the list (not out of the modal) so a mispick
            // is recoverable without restarting the scan.
            KeyCode::Esc => {
                cp.chosen = None;
                cp.dir_suggestion = None;
                cp.focus = ConversationPickerFocus::List;
                return;
            }
            KeyCode::Tab => {
                if let Some(suggestion) = cp.dir_suggestion.take() {
                    for c in suggestion.chars() {
                        cp.dir_input.insert(c);
                    }
                } else {
                    return;
                }
            }
            KeyCode::Enter => {
                self.confirm_conversation_import();
                return;
            }
            other => {
                if !modals::apply_text_input_key(Some(&mut cp.dir_input), other, mods) {
                    return;
                }
            }
        }
        self.update_convo_dir_suggestion();
    }

    /// Per-keystroke fish-style completion for the directory input (imports
    /// are local-only, so the local filesystem is always the right target).
    fn update_convo_dir_suggestion(&mut self) {
        let modals::Modal::ConversationPicker(ref mut cp) = self.modal else {
            return;
        };
        let value = cp.dir_input.value().to_string();
        let at_end = cp.dir_input.cursor_pos() == value.chars().count();
        cp.dir_suggestion = if at_end && !value.is_empty() {
            crate::paths::complete_directory_path(&value)
        } else {
            None
        };
    }

    /// `Enter` on the directory input: validate the directory, stage the
    /// transcript next to it, and hand off to the session-name step with a
    /// spawn config that pins the conversation id (resume + identity).
    fn confirm_conversation_import(&mut self) {
        let modals::Modal::ConversationPicker(ref cp) = self.modal else {
            return;
        };
        let Some(convo) = cp.chosen.and_then(|i| cp.entries.get(i)).cloned() else {
            return;
        };
        let agent = cp.agent.clone();
        let dir = cp.dir_input.value().trim().to_string();
        if dir.is_empty() {
            self.set_error("Working directory cannot be empty");
            return;
        }
        let expanded = crate::paths::expand_tilde(&dir);
        // Canonicalize for the slug: `claude` resolves its cwd via getcwd
        // (physical path), so a symlinked component would otherwise land the
        // staged transcript under a slug the CLI never looks at.
        let canonical = match std::fs::canonicalize(&expanded) {
            Ok(p) => p,
            Err(e) => {
                self.set_error(format!(
                    "Directory not usable: {} ({e})",
                    expanded.display()
                ));
                return;
            }
        };
        if !canonical.is_dir() {
            self.set_error(format!("Not a directory: {}", expanded.display()));
            return;
        }
        let Some(projects) = crate::paths::claude_projects_dir(None) else {
            self.set_error("Cannot resolve the Claude Code projects directory");
            return;
        };
        if let Err(e) = stage_transcript_for_resume(&projects, &convo.path, &convo.id, &canonical) {
            self.set_error(format!("Failed to stage transcript for resume: {e}"));
            return;
        }
        self.modal.close();

        // Both ids pinned: `resume_session_id` selects the `--resume {id}` arg
        // group, `agent_session_id` is the session's identity everywhere else
        // (FRIRING_SESSION_ID, the F9 activity scan, the DB row, restarts).
        let config = crate::session::SessionConfig {
            agent,
            agent_session_id: Some(convo.id.clone()),
            resume_session_id: Some(convo.id.clone()),
            cwd: Some(expanded),
            ..Default::default()
        };
        self.new_session.additional_dirs.clear();
        self.new_session.spawn_base_branch = None;
        self.new_session.import = true;
        self.new_session.spawn_config = Some(config);
        self.new_session.spawn_worktrees = Vec::new();
        let mut name_modal = modals::SessionNameModal::default();
        name_modal.name.set(&suggest_session_name(&convo));
        self.modal = modals::Modal::SessionName(name_modal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn user_line(cwd: &str, prompt: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{prompt}"}},"cwd":"{cwd}","gitBranch":"main"}}"#
        )
    }

    const ID_A: &str = "11111111-1111-4111-8111-111111111111";
    const ID_B: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn scan_lists_conversations_newest_first_and_skips_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();
        write(
            &projects.join("-repo-a").join(format!("{ID_A}.jsonl")),
            &user_line("/repo/a", "older prompt"),
        );
        write(
            &projects.join("-repo-b").join(format!("{ID_B}.jsonl")),
            &user_line("/repo/b", "newer prompt"),
        );
        // Noise: non-uuid jsonl, an empty transcript, a metadata-less file.
        write(&projects.join("-repo-a/notes.jsonl"), "{}");
        write(
            &projects.join("-repo-a").join(format!("{ID_B}.empty.jsonl")),
            "",
        );
        // Order by mtime: make B strictly newer than A.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        set_mtime(&projects.join("-repo-a").join(format!("{ID_A}.jsonl")), old);

        let found = scan_conversations(projects, &HashSet::new());
        let ids: Vec<&str> = found.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec![ID_B, ID_A]);
        assert_eq!(found[0].cwd.as_deref(), Some(Path::new("/repo/b")));
        assert_eq!(found[0].title.as_deref(), Some("newer prompt"));
        assert_eq!(found[0].git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn scan_excludes_tracked_ids_and_dedupes_to_newest_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();
        // The same conversation staged in two dirs: keep the newest copy only.
        let stale = projects.join("-orig").join(format!("{ID_A}.jsonl"));
        let fresh = projects.join("-staged").join(format!("{ID_A}.jsonl"));
        write(&stale, &user_line("/orig", "stale"));
        write(&fresh, &user_line("/orig", "fresh"));
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        set_mtime(&stale, old);
        write(
            &projects.join("-x").join(format!("{ID_B}.jsonl")),
            &user_line("/x", "tracked"),
        );

        let exclude: HashSet<String> = [ID_B.to_string()].into();
        let found = scan_conversations(projects, &exclude);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, ID_A);
        assert_eq!(found[0].title.as_deref(), Some("fresh"));
        assert_eq!(found[0].path, fresh);
    }

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let f = std::fs::File::options().append(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn stage_copies_into_destination_slug_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let src = projects.join("-orig").join(format!("{ID_A}.jsonl"));
        write(&src, "history");
        let dest_cwd = tmp.path().join("work dir");
        std::fs::create_dir_all(&dest_cwd).unwrap();

        stage_transcript_for_resume(&projects, &src, ID_A, &dest_cwd).unwrap();

        let slug = crate::paths::claude_project_slug(&dest_cwd);
        let staged = projects.join(slug).join(format!("{ID_A}.jsonl"));
        assert_eq!(std::fs::read_to_string(staged).unwrap(), "history");
        // The original stays behind untouched.
        assert_eq!(std::fs::read_to_string(src).unwrap(), "history");
    }

    #[test]
    fn stage_keeps_a_newer_destination_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let src = projects.join("-orig").join(format!("{ID_A}.jsonl"));
        write(&src, "old history");
        let dest_cwd = tmp.path().join("dest");
        std::fs::create_dir_all(&dest_cwd).unwrap();
        let slug = crate::paths::claude_project_slug(&dest_cwd);
        let dest = projects.join(slug).join(format!("{ID_A}.jsonl"));
        write(&dest, "continued here");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        set_mtime(&src, old);

        stage_transcript_for_resume(&projects, &src, ID_A, &dest_cwd).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "continued here");

        // A strictly newer source (the scan's pick) replaces a stale staging.
        set_mtime(&dest, old);
        write(&src, "newest");
        stage_transcript_for_resume(&projects, &src, ID_A, &dest_cwd).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "newest");
    }

    #[test]
    fn filter_matches_title_and_directory() {
        let convo = |id: &str, title: &str, cwd: &str| CcConversation {
            id: id.into(),
            path: PathBuf::from("/p"),
            cwd: Some(PathBuf::from(cwd)),
            git_branch: None,
            title: Some(title.into()),
            mtime_ms: 0,
        };
        let mut cp = ConversationPickerModal {
            entries: vec![
                convo(ID_A, "fix flaky test", "/repo/api"),
                convo(ID_B, "write docs", "/repo/site"),
            ],
            ..Default::default()
        };
        cp.recompute_filter();
        assert_eq!(cp.filtered_indices, vec![0, 1]);
        cp.search_input.set("flaky");
        cp.recompute_filter();
        assert_eq!(cp.filtered_indices, vec![0]);
        cp.search_input.set("site");
        cp.recompute_filter();
        assert_eq!(cp.filtered_indices, vec![1]);
    }

    #[test]
    fn suggested_name_is_first_line_capped() {
        let mut convo = CcConversation {
            id: ID_A.into(),
            path: PathBuf::from("/p"),
            cwd: None,
            git_branch: None,
            title: Some("Fix the flaky   integration tests please\nsecond line".into()),
            mtime_ms: 0,
        };
        // Whitespace collapsed, second line dropped, capped at 30 chars.
        assert_eq!(
            suggest_session_name(&convo),
            "Fix the flaky integration test"
        );
        convo.title = None;
        assert_eq!(suggest_session_name(&convo), "");
    }
}
