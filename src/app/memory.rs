//! Per-session process-tree memory — the off-thread scan that prices each
//! running agent onto [`SessionInfo::memory`].
//!
//! Follows the same discipline as the other background scans (ADR-P12, and the
//! `metrics_refresh` / `cc_refresh` jobs): build a cheap input list on the UI
//! thread, hand it to `spawn_blocking`, apply the result by session id. Nothing
//! here runs per frame — a session's pid lookup is a control-mode round-trip
//! and the process-table read is either a few hundred procfs reads or a `ps`
//! fork, neither of which may sit between a keystroke and a repaint.
//!
//! Scope, and what each outcome means (see [`SessionMemory`]):
//!
//! - **Live local pane** → its pid's whole subtree, summed.
//! - **Ghost** → `Unloaded`, with no I/O at all: an unloaded session has no
//!   process by construction, and reporting the measured absence is the point
//!   (it is what an `Alt+U` bought).
//! - **Remote (`ssh:`/`wsl:`)** → nothing. The tree lives on the host, and
//!   summing it would mean shipping a second command down every transport
//!   every few seconds; guessing from the local table would price an unrelated
//!   local pid.
//!
//! The pure arithmetic and the three platform text formats live in
//! [`crate::session::memory`]; this module is the I/O glue.

use std::sync::Arc;

use crate::agent::SessionBackend;
use crate::session::memory::{ProcEntry, ProcTable};
use crate::session::{SessionId, SessionMemory};

use super::{background, App};

/// Result of a background memory scan, delivered via `App::memory_refresh`.
/// Each entry replaces one session's [`SessionInfo::memory`] wholesale, so a
/// session that stops being measurable clears instead of going stale.
pub(super) struct MemoryRefresh {
    updates: Vec<(SessionId, Option<SessionMemory>)>,
}

/// One session's scan input, resolved on the UI thread.
enum MemoryInput {
    /// A live local pane. The backend handle rides along so the pid lookup —
    /// a control-mode round-trip — happens on the worker, not here.
    Pane {
        id: SessionId,
        backend: Arc<dyn SessionBackend>,
        backend_id: String,
    },
    /// A ghost: answerable without touching the process table.
    Ghost { id: SessionId },
}

impl App {
    /// Kick off a background memory scan over every local session. Remote
    /// sessions and unreachable placeholders are skipped (they own no local
    /// process); ghosts are included, because their `—` is the feature.
    pub(super) fn start_memory_refresh(&mut self) {
        if self.memory_refresh.in_progress() {
            return;
        }
        let inputs: Vec<MemoryInput> = self
            .sessions
            .iter()
            .filter(|s| s.info.remote_host.is_none())
            .filter_map(|s| {
                if s.is_ghost() {
                    return Some(MemoryInput::Ghost { id: s.info.id });
                }
                // A non-ghost placeholder is an unreachable remote whose
                // `remote_host` never resolved: no pane, so no pid to ask for.
                if s.is_placeholder() {
                    return None;
                }
                let (backend, backend_id) = s.backend_handle();
                Some(MemoryInput::Pane {
                    id: s.info.id,
                    backend,
                    backend_id,
                })
            })
            .collect();
        if inputs.is_empty() {
            return;
        }
        let tx = self.memory_refresh.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(collect_session_memory(inputs));
        });
    }

    /// Apply a completed memory scan. Repainting is left to the normal
    /// throttled redraw (ADR-P1) like the sibling git-stats poll — a byte count
    /// moving is not worth forcing a frame.
    pub(super) fn poll_memory_refresh(&mut self) {
        let background::TaskPoll::Done(refresh) = self.memory_refresh.poll() else {
            return;
        };
        for (id, memory) in refresh.updates {
            if let Some(session) = self.sessions.iter_mut().find(|s| s.info.id == id) {
                session.info.memory = memory;
            }
        }
    }
}

/// The whole scan pass, run on a blocking thread.
///
/// Pids are resolved first so the process table is read **once**, after, and
/// only when at least one session has a root to look up — the common
/// all-ghosts case then costs no process-table read at all.
fn collect_session_memory(inputs: Vec<MemoryInput>) -> MemoryRefresh {
    let mut ghosts: Vec<SessionId> = Vec::new();
    let mut roots: Vec<(SessionId, Option<u32>)> = Vec::new();
    for input in inputs {
        match input {
            MemoryInput::Ghost { id } => ghosts.push(id),
            MemoryInput::Pane {
                id,
                backend,
                backend_id,
            } => roots.push((id, backend.pane_pid(&backend_id).ok().flatten())),
        }
    }
    let table = if roots.iter().any(|(_, pid)| pid.is_some()) {
        ProcTable::new(read_process_table())
    } else {
        ProcTable::default()
    };
    MemoryRefresh {
        updates: attribute(&table, &roots, &ghosts),
    }
}

/// Pure: turn resolved root pids into per-session results.
///
/// A pid that wouldn't resolve — and an empty process table, which means the
/// read failed or the platform has no implementation — reports `None`
/// (unknown). Reporting `Unloaded` there would paint every session as freed the
/// moment a single `ps` fork failed.
fn attribute(
    table: &ProcTable,
    roots: &[(SessionId, Option<u32>)],
    ghosts: &[SessionId],
) -> Vec<(SessionId, Option<SessionMemory>)> {
    let mut updates = Vec::with_capacity(roots.len() + ghosts.len());
    updates.extend(ghosts.iter().map(|id| (*id, Some(SessionMemory::Unloaded))));
    for (id, pid) in roots {
        let memory = match pid {
            // `> 1` rejects the init/launchd pid: every user process descends
            // from it, so a garbled `#{pane_pid}` would otherwise price the
            // whole machine as one session's cost. No pane can legitimately
            // report it.
            Some(pid) if *pid > 1 && !table.is_empty() => Some(table.subtree(*pid)),
            _ => None,
        };
        updates.push((*id, memory));
    }
    updates
}

/// Read the machine's process table: `(pid, ppid, rss)` for every process.
///
/// Linux walks procfs directly — `/proc/<pid>/stat` for the parent pid,
/// `/proc/<pid>/statm` for the resident page count — so the scan forks nothing.
/// macOS has no procfs and no stable public API for another process's footprint
/// short of a C binding, so it shells out to one `ps` covering every process at
/// once (~50 ms, off-thread, every few seconds). Windows reads `sysinfo`'s
/// snapshot, whose working-set size is the RSS analogue.
///
/// Any other target returns an empty table, which [`attribute`] reads as
/// "unknown" — the feature reports nothing rather than guessing.
#[cfg(target_os = "linux")]
fn read_process_table() -> Vec<ProcEntry> {
    use crate::session::memory::{parse_proc_stat_ppid, parse_statm_resident_pages};

    let page_size = page_size();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut table = Vec::new();
    for entry in entries.flatten() {
        // `/proc` also holds `self`, `cpuinfo`, … — only the numeric dirs are
        // processes.
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        // A process can exit between the readdir and either read; a vanished
        // pid is simply not in this pass's table.
        let Some(ppid) = std::fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|s| parse_proc_stat_ppid(&s))
        else {
            continue;
        };
        // Kernel threads have no address space, but their `statm` is readable
        // and parses as a genuine 0 — they stay in the table as parents,
        // costing nothing. A `statm` that won't read or parse is the vanished
        // pid again, dropped like the `stat` read above rather than folded to
        // a zero-cost live process.
        let Some(pages) = std::fs::read_to_string(dir.join("statm"))
            .ok()
            .and_then(|s| parse_statm_resident_pages(&s))
        else {
            continue;
        };
        table.push(ProcEntry {
            pid,
            ppid,
            rss_bytes: pages.saturating_mul(page_size),
        });
    }
    table
}

/// The page size `statm` counts in. Read from the kernel rather than assumed:
/// aarch64 Linux is routinely built with 16K or 64K pages, where a hardcoded
/// 4K would under-report every session by 4–16×.
#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer name and returns a `long`; it touches
    // no pointers and has no failure mode beyond the -1 handled below.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).unwrap_or(4096)
}

#[cfg(target_os = "macos")]
fn read_process_table() -> Vec<ProcEntry> {
    use crate::session::memory::parse_ps_table;

    // One process listing for the whole machine: `-A` all processes, `-o …=`
    // suppresses the header so every line is data.
    let output = std::process::Command::new("ps")
        .args(["-Ao", "pid=,ppid=,rss="])
        .output();
    match output {
        Ok(out) if out.status.success() => parse_ps_table(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

#[cfg(windows)]
fn read_process_table() -> Vec<ProcEntry> {
    // A throwaway collector: unlike the CPU-usage sampler in `metrics_refresh`,
    // memory needs no delta against a previous refresh, so nothing has to
    // persist between scans.
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    sys.processes()
        .values()
        .map(|p| ProcEntry {
            pid: p.pid().as_u32(),
            // A process whose parent has already been reaped roots its own tree.
            ppid: p.parent().map(|p| p.as_u32()).unwrap_or(0),
            rss_bytes: p.memory(),
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_process_table() -> Vec<ProcEntry> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, ppid: u32, rss_bytes: u64) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            rss_bytes,
        }
    }

    fn table() -> ProcTable {
        // 900 (pane) ── 912 (agent) ─┬─ 913 (mcp)
        //                            └─ 914 (tool)
        ProcTable::new([
            entry(1, 0, 1_000),
            entry(900, 1, 2_000),
            entry(912, 900, 300_000),
            entry(913, 912, 40_000),
            entry(914, 912, 8_000),
        ])
    }

    #[test]
    fn live_pane_reports_its_whole_subtree() {
        let id = SessionId::default();
        let updates = attribute(&table(), &[(id, Some(900))], &[]);
        assert_eq!(
            updates,
            vec![(
                id,
                Some(SessionMemory::Live {
                    rss_bytes: 350_000,
                    procs: 4
                })
            )]
        );
    }

    #[test]
    fn ghost_reports_unloaded_without_a_pid() {
        let id = SessionId::default();
        let updates = attribute(&table(), &[], &[id]);
        assert_eq!(updates, vec![(id, Some(SessionMemory::Unloaded))]);
    }

    #[test]
    fn unresolvable_pid_reports_unknown_not_zero() {
        // The pane is alive but tmux wouldn't answer: "don't know" must not
        // render as "costs nothing".
        let id = SessionId::default();
        let updates = attribute(&table(), &[(id, None)], &[]);
        assert_eq!(updates, vec![(id, None)]);
    }

    #[test]
    fn empty_process_table_reports_unknown_for_every_pane() {
        // A failed `ps` (or an unsupported target) must not flip the whole
        // fleet to "unloaded".
        let a = SessionId::default();
        let b = SessionId::default();
        let updates = attribute(
            &ProcTable::default(),
            &[(a, Some(900)), (b, Some(912))],
            &[],
        );
        assert_eq!(updates, vec![(a, None), (b, None)]);
    }

    #[test]
    fn the_init_pid_is_rejected_rather_than_summing_the_machine() {
        // Every process descends from pid 1, so trusting a garbled pane pid
        // would price the whole machine as one session.
        let id = SessionId::default();
        for bogus in [0, 1] {
            assert_eq!(
                attribute(&table(), &[(id, Some(bogus))], &[]),
                vec![(id, None)],
                "pid {bogus} must not resolve to a subtree"
            );
        }
    }

    #[test]
    fn a_pid_absent_from_a_live_table_reports_unloaded() {
        // The pane's process exited between the pid lookup and the table read.
        let id = SessionId::default();
        let updates = attribute(&table(), &[(id, Some(4242))], &[]);
        assert_eq!(updates, vec![(id, Some(SessionMemory::Unloaded))]);
    }

    #[test]
    fn a_ghost_only_input_never_reads_the_process_table() {
        // The `MemoryInput::Pane`-free path must resolve entirely from the
        // input list: a fleet of ghosts costs no `ps` fork and no procfs walk.
        let id = SessionId::default();
        let refresh = collect_session_memory(vec![MemoryInput::Ghost { id }]);
        assert_eq!(refresh.updates, vec![(id, Some(SessionMemory::Unloaded))]);
    }

    #[test]
    fn poll_applies_each_result_to_its_own_session() {
        let (mut app, _guard, _tmp) = super::super::state::tests::app_with_sessions(2);
        let first = app.sessions[0].info.id;
        let second = app.sessions[1].info.id;
        let live = SessionMemory::Live {
            rss_bytes: 347_078_656,
            procs: 3,
        };

        let tx = app.memory_refresh.start();
        tx.send(MemoryRefresh {
            updates: vec![
                (second, Some(SessionMemory::Unloaded)),
                (first, Some(live)),
                // A session that has since been closed is simply dropped.
                (SessionId::default(), Some(live)),
            ],
        })
        .unwrap();
        app.poll_memory_refresh();

        assert_eq!(app.sessions[0].info.memory, Some(live));
        assert_eq!(app.sessions[1].info.memory, Some(SessionMemory::Unloaded));
        assert!(!app.memory_refresh.in_progress());
    }

    #[test]
    fn poll_clears_a_session_that_stopped_being_measurable() {
        // `None` overwrites: a stale figure must never outlive the measurement.
        let (mut app, _guard, _tmp) = super::super::state::tests::app_with_sessions(1);
        let id = app.sessions[0].info.id;
        app.sessions[0].info.memory = Some(SessionMemory::Live {
            rss_bytes: 1,
            procs: 1,
        });

        let tx = app.memory_refresh.start();
        tx.send(MemoryRefresh {
            updates: vec![(id, None)],
        })
        .unwrap();
        app.poll_memory_refresh();

        assert_eq!(app.sessions[0].info.memory, None);
    }

    #[tokio::test]
    async fn unloading_reports_the_freed_memory_in_the_same_frame() {
        // Alt+U killed the pane, so the absence is measured, not guessed —
        // waiting a cadence for it would leave the badge claiming the memory
        // the unload just freed. A pass measuring the live pane is in flight to
        // prove its late result can't paint the figure back.
        let (mut app, _guard, _tmp) = super::super::state::tests::app_with_sessions(1);
        let id = app.sessions[0].info.id;
        let live = SessionMemory::Live {
            rss_bytes: 347_078_656,
            procs: 3,
        };
        app.sessions[0].info.memory = Some(live);
        let tx = app.memory_refresh.start();

        app.unload_active_session();

        assert!(app.sessions[0].is_ghost());
        assert_eq!(app.sessions[0].info.memory, Some(SessionMemory::Unloaded));
        assert!(!app.memory_refresh.in_progress());
        let _ = tx.send(MemoryRefresh {
            updates: vec![(id, Some(live))],
        });
        app.poll_memory_refresh();
        assert_eq!(app.sessions[0].info.memory, Some(SessionMemory::Unloaded));
    }

    #[test]
    fn disabling_the_feature_live_clears_the_measured_figures() {
        // The three surfaces render straight off `info.memory`: turning the
        // flag off must take the badges with it, and an in-flight scan must not
        // paint them back.
        let (mut app, _guard, _tmp) = super::super::state::tests::app_with_sessions(2);
        let id = app.sessions[0].info.id;
        app.sessions[0].info.memory = Some(SessionMemory::Live {
            rss_bytes: 347_078_656,
            procs: 3,
        });
        app.sessions[1].info.memory = Some(SessionMemory::Unloaded);

        let tx = app.memory_refresh.start();
        tx.send(MemoryRefresh {
            updates: vec![(
                id,
                Some(SessionMemory::Live {
                    rss_bytes: 1,
                    procs: 1,
                }),
            )],
        })
        .unwrap();

        let mut settings = crate::session::settings::Settings::default();
        settings.features.session_memory = false;
        app.apply_live_settings(&settings);

        assert!(app.sessions.iter().all(|s| s.info.memory.is_none()));
        assert!(!app.memory_refresh.in_progress());
        app.poll_memory_refresh();
        assert!(app.sessions.iter().all(|s| s.info.memory.is_none()));
    }

    #[test]
    fn the_real_process_table_prices_this_test_process() {
        // Integration smoke test of whichever platform arm compiled: our own
        // pid must be present with a non-zero footprint.
        let entries = read_process_table();
        // On a supported target an empty table means the reader itself broke (a
        // failed `ps` fork, an unreadable /proc, an empty sysinfo snapshot) —
        // the one thing this test exists to catch, so it must not pass silently.
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        assert!(
            !entries.is_empty(),
            "the platform process-table reader returned nothing"
        );
        // Targets with no implementation legitimately return nothing.
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        if entries.is_empty() {
            return;
        }
        let me = std::process::id();
        let table = ProcTable::new(entries);
        let SessionMemory::Live { rss_bytes, procs } = table.subtree(me) else {
            panic!("the running test process must be in the process table");
        };
        assert!(procs >= 1);
        assert!(
            rss_bytes > 1024 * 1024,
            "a running Rust test process should hold more than 1 MiB resident, got {rss_bytes}"
        );
    }
}
