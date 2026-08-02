//! Resident memory of a session's agent process tree — the pure core.
//!
//! A session owns a tmux window whose pane runs an agent CLI; that CLI forks
//! children (MCP servers, language servers, shell tools), so "what this session
//! costs" is the summed RSS of the whole tree, not of the pane process alone.
//!
//! This module holds only data and arithmetic: the process-table row
//! ([`ProcEntry`]), the parent→child index that sums a tree ([`ProcTable`]),
//! the per-session result ([`SessionMemory`]), and the three platform text
//! formats the app layer reads. Reading them is I/O and lives in
//! `app::memory`; keeping the parsers here means the Linux ones are unit-tested
//! on macOS CI and vice versa.
//!
//! **RSS is summed, not deduplicated.** Pages shared between a parent and its
//! forked children are counted once per process, so a fork-heavy tree reads
//! high. That is the same metric the "~333 MB for an idle claude" measurement
//! in `FORK.md` uses, so the numbers are comparable — and it is the metric that
//! answers "what does the OS have resident for this session".

use std::collections::HashMap;

/// One row of the machine's process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    /// Resident set size in bytes.
    pub rss_bytes: u64,
}

/// What a session's agent process tree costs, as measured.
///
/// Deliberately three-valued together with its `Option` wrapper on
/// [`SessionInfo::memory`](super::SessionInfo::memory): `Some(Live)` is a
/// measurement, `Some(Unloaded)` is a measured *absence* (the saving an unload
/// bought), and `None` is "not known" — a remote session, a pane whose pid
/// wouldn't resolve, or a scan that hasn't run. Collapsing the last two would
/// report an unmeasurable session as free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMemory {
    /// A live tree: `rss_bytes` summed across `procs` processes.
    Live { rss_bytes: u64, procs: u32 },
    /// The tree is empty — a ghost, or a pane whose process has exited.
    Unloaded,
}

impl SessionMemory {
    /// Bytes to add into a fleet total: the tree's RSS, or `0` when unloaded.
    pub fn rss_bytes(self) -> u64 {
        match self {
            Self::Live { rss_bytes, .. } => rss_bytes,
            Self::Unloaded => 0,
        }
    }

    /// Whether a process tree is actually running (`Live`).
    pub fn is_live(self) -> bool {
        matches!(self, Self::Live { .. })
    }
}

/// The machine's process table, indexed for subtree sums.
///
/// Built once per scan and queried once per session, so the parent→child index
/// is paid for by the first query and reused by the rest.
#[derive(Debug, Default)]
pub struct ProcTable {
    /// Parent pid → its direct children.
    children: HashMap<u32, Vec<u32>>,
    /// Pid → resident bytes. Doubles as the "does this pid exist" set.
    rss: HashMap<u32, u64>,
}

impl ProcTable {
    pub fn new(entries: impl IntoIterator<Item = ProcEntry>) -> Self {
        let mut table = Self::default();
        for e in entries {
            // A pid is its own parent nowhere on a sane system, but a
            // self-edge would make the walk below spin without the visited
            // set; drop it at the source anyway.
            if e.ppid != e.pid {
                table.children.entry(e.ppid).or_default().push(e.pid);
            }
            table.rss.insert(e.pid, e.rss_bytes);
        }
        table
    }

    /// Whether the table holds no processes — a failed or unsupported read.
    /// Callers must treat an empty table as *unknown*, never as "nothing is
    /// running": every session would otherwise read as unloaded at once.
    pub fn is_empty(&self) -> bool {
        self.rss.is_empty()
    }

    /// Sum `root` and every descendant.
    ///
    /// Returns [`SessionMemory::Unloaded`] when `root` isn't in the table — the
    /// process exited (or never existed), which is exactly "nothing running".
    /// The walk carries a visited set so a corrupt parent cycle terminates
    /// instead of hanging the scan thread.
    pub fn subtree(&self, root: u32) -> SessionMemory {
        if !self.rss.contains_key(&root) {
            return SessionMemory::Unloaded;
        }
        let mut rss_bytes = 0u64;
        let mut procs = 0u32;
        let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            rss_bytes = rss_bytes.saturating_add(self.rss.get(&pid).copied().unwrap_or(0));
            procs = procs.saturating_add(1);
            if let Some(kids) = self.children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        SessionMemory::Live { rss_bytes, procs }
    }
}

/// Parse `ps -Ao pid=,ppid=,rss=` output (macOS): one whitespace-separated
/// `pid ppid rss` triple per line, with RSS in **KiB**. Lines that don't parse
/// (a stray header, a blank tail line) are skipped rather than failing the
/// scan — a partial table still prices most sessions correctly.
pub fn parse_ps_table(out: &str) -> Vec<ProcEntry> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let rss_kib: u64 = fields.next()?.parse().ok()?;
            Some(ProcEntry {
                pid,
                ppid,
                rss_bytes: rss_kib.saturating_mul(1024),
            })
        })
        .collect()
}

/// Parse the parent pid (field 4) out of a Linux `/proc/<pid>/stat` line.
///
/// The `comm` field is parenthesized and may itself contain spaces *and*
/// parentheses (`(tmux: server)`, `(a) b`), so splitting on whitespace from the
/// left is wrong. The fixed-width fields start after the **last** `)`, where
/// field 3 (`state`) is the first token and `ppid` the second.
pub fn parse_proc_stat_ppid(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Parse the resident page count (field 2) out of a Linux `/proc/<pid>/statm`
/// line. Pages, not bytes — the caller scales by the kernel page size.
pub fn parse_statm_resident_pages(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
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

    // ── ProcTable::subtree ──

    #[test]
    fn subtree_sums_root_and_all_descendants() {
        // 100 ─┬─ 200 ── 300
        //      └─ 201
        // 999 is an unrelated process and must not be counted.
        let table = ProcTable::new([
            entry(100, 1, 10),
            entry(200, 100, 20),
            entry(300, 200, 30),
            entry(201, 100, 40),
            entry(999, 1, 5_000),
        ]);
        assert_eq!(
            table.subtree(100),
            SessionMemory::Live {
                rss_bytes: 100,
                procs: 4
            }
        );
    }

    #[test]
    fn subtree_of_a_leaf_is_just_itself() {
        let table = ProcTable::new([entry(100, 1, 10), entry(200, 100, 20)]);
        assert_eq!(
            table.subtree(200),
            SessionMemory::Live {
                rss_bytes: 20,
                procs: 1
            }
        );
    }

    #[test]
    fn subtree_of_a_missing_pid_reads_unloaded() {
        // The measured absence, not a zero-byte live tree: an exited pane and
        // a ghost must render the same `—`.
        let table = ProcTable::new([entry(100, 1, 10)]);
        assert_eq!(table.subtree(4242), SessionMemory::Unloaded);
    }

    #[test]
    fn subtree_terminates_on_a_parent_cycle() {
        // 100 → 200 → 100. A corrupt table must not hang the scan thread.
        let table = ProcTable::new([entry(100, 200, 10), entry(200, 100, 20)]);
        assert_eq!(
            table.subtree(100),
            SessionMemory::Live {
                rss_bytes: 30,
                procs: 2
            }
        );
    }

    #[test]
    fn subtree_ignores_a_self_parented_process() {
        let table = ProcTable::new([entry(100, 100, 10)]);
        assert_eq!(
            table.subtree(100),
            SessionMemory::Live {
                rss_bytes: 10,
                procs: 1
            }
        );
    }

    #[test]
    fn empty_table_is_reported_as_empty() {
        assert!(ProcTable::default().is_empty());
        assert!(!ProcTable::new([entry(1, 0, 1)]).is_empty());
    }

    // ── SessionMemory ──

    #[test]
    fn unloaded_contributes_nothing_to_a_total() {
        assert_eq!(SessionMemory::Unloaded.rss_bytes(), 0);
        assert!(!SessionMemory::Unloaded.is_live());
        let live = SessionMemory::Live {
            rss_bytes: 349_175_808,
            procs: 3,
        };
        assert_eq!(live.rss_bytes(), 349_175_808);
        assert!(live.is_live());
    }

    // ── parse_ps_table (macOS) ──

    #[test]
    fn ps_table_parses_padded_columns_and_scales_kib() {
        let out = "    1     0  19076\n  912   1  340892\n";
        assert_eq!(
            parse_ps_table(out),
            vec![entry(1, 0, 19_076 * 1024), entry(912, 1, 340_892 * 1024),]
        );
    }

    #[test]
    fn ps_table_skips_unparseable_lines() {
        let out = "  PID  PPID   RSS\n\n  912   1  340892\ngarbage\n";
        assert_eq!(parse_ps_table(out), vec![entry(912, 1, 340_892 * 1024)]);
    }

    #[test]
    fn ps_table_of_empty_output_is_empty() {
        assert!(parse_ps_table("").is_empty());
    }

    // ── parse_proc_stat_ppid (Linux) ──

    #[test]
    fn proc_stat_ppid_reads_the_field_after_the_comm() {
        let stat = "912 (node) S 900 912 900 0 -1 4194304 12345 0 0 0 42 7 0 0 20 0 11 0 998877";
        assert_eq!(parse_proc_stat_ppid(stat), Some(900));
    }

    #[test]
    fn proc_stat_ppid_survives_spaces_and_parens_in_the_comm() {
        // Splitting from the left would read "server)" as the state field.
        let stat = "77 (tmux: server (x)) S 1 77 77 0 -1 4194304 100 0 0 0 1 2 0 0 20 0 1 0 5";
        assert_eq!(parse_proc_stat_ppid(stat), Some(1));
    }

    #[test]
    fn proc_stat_ppid_rejects_truncated_input() {
        assert_eq!(parse_proc_stat_ppid(""), None);
        assert_eq!(parse_proc_stat_ppid("912 (node"), None);
        assert_eq!(parse_proc_stat_ppid("912 (node) S"), None);
    }

    // ── parse_statm_resident_pages (Linux) ──

    #[test]
    fn statm_reads_the_resident_field() {
        // size resident shared text lib data dt
        assert_eq!(
            parse_statm_resident_pages("100000 85000 4200 12 0 9000 0"),
            Some(85_000)
        );
    }

    #[test]
    fn statm_rejects_truncated_input() {
        assert_eq!(parse_statm_resident_pages(""), None);
        assert_eq!(parse_statm_resident_pages("100000"), None);
        assert_eq!(parse_statm_resident_pages("100000 nope"), None);
    }
}
