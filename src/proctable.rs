//! Reading the machine's process table — the one platform-specific I/O behind
//! per-session memory.
//!
//! Split out of `app::memory` so both readers can share it: the TUI's
//! background scan (which prices every session onto `SessionInfo::memory`) and
//! `friring-cli session resources`, which must work with no TUI running and
//! cannot reach into `app` (see `tests/architecture_rules.rs`).
//!
//! The arithmetic that turns these rows into a session's cost — the
//! parent→child index and the subtree sum — stays pure in
//! [`crate::session::memory`], as do the three text-format parsers, so they
//! remain unit-tested on every platform regardless of which one is running.

use crate::session::memory::ProcEntry;

/// Read the machine's process table: `(pid, ppid, rss)` for every process.
///
/// Linux walks procfs directly — `/proc/<pid>/stat` for the parent pid,
/// `/proc/<pid>/statm` for the resident page count — so the scan forks nothing.
/// macOS has no procfs and no stable public API for another process's footprint
/// short of a C binding, so it shells out to one `ps` covering every process at
/// once (~50 ms). Windows reads `sysinfo`'s snapshot, whose working-set size is
/// the RSS analogue.
///
/// Any other target returns an empty table. Callers must read that as
/// **unknown**, never as "nothing is running" — otherwise one failed read
/// reports every session as freed.
#[cfg(target_os = "linux")]
pub fn read() -> Vec<ProcEntry> {
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
pub fn read() -> Vec<ProcEntry> {
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
pub fn read() -> Vec<ProcEntry> {
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
pub fn read() -> Vec<ProcEntry> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use crate::session::memory::ProcTable;
    use crate::session::SessionMemory;

    /// Whether the OS source [`read`] reads is reachable on this
    /// host, established independently of the reader itself so an empty table
    /// can be attributed to the environment rather than excused as one.
    /// `false` on a target with no implementation, and on a sandbox that
    /// withholds procfs or `ps`.
    fn process_source_reachable() -> bool {
        #[cfg(target_os = "linux")]
        {
            // Even a `hidepid` container shows a process its own entry; a host
            // that withholds this one withholds the whole walk.
            std::path::Path::new("/proc/self/statm").is_file()
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("ps")
                .args(["-p", &std::process::id().to_string()])
                .output()
                .is_ok_and(|out| out.status.success())
        }
        #[cfg(windows)]
        {
            true
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            false
        }
    }

    #[test]
    fn the_real_process_table_prices_this_test_process() {
        // Integration smoke test of whichever platform arm compiled: our own
        // pid must be present with a non-zero footprint.
        let entries = super::read();
        if entries.is_empty() {
            // Two very different things read as an empty table, and only one is
            // a bug: a reader that broke (bad `ps` flags, wrong procfs path, an
            // empty snapshot) versus a host that cannot enumerate processes at
            // all — an unsupported target, or a locked-down sandbox/container
            // whose `/proc` isn't reachable. Probing the OS source *directly*,
            // rather than trusting the reader under test, tells them apart: an
            // empty table is only tolerated where the source itself is absent.
            // Production agrees — `ProcTable::is_empty` is treated as "unknown",
            // never as an error.
            assert!(
                !process_source_reachable(),
                "the platform process-table reader returned nothing on a host \
                 that can enumerate processes"
            );
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
