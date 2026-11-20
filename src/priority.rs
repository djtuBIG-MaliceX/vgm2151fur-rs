//! How many cores a batch may use, and how quiet those processes stay.
//!
//! Workers follow the logical CPUs, leaving one or two threads free and capping
//! at 16; an explicit count may go up to one below the thread count, with 31 as
//! the safety ceiling where the single-threaded renders start competing. Worker
//! processes and Furnace renders run at low priority: Idle on Windows, nice 10
//! elsewhere.

/// Task Manager "Low". Child processes inherit it.
pub const LOW_PRIORITY_CLASS: u32 = 0x0000_0040;

/// Rendering is one thread per worker; past this the renders fight each
/// other more than they parallelize, and the batch goes slower.
pub const WORKER_CEILING: i64 = 31;

#[cfg(unix)]
unsafe extern "C" {
    /// POSIX `nice(2)`; there is no libc dependency, so declare the one call we
    /// need. Python's `os.nice` maps directly onto it.
    fn nice(inc: std::ffi::c_int) -> std::ffi::c_int;
}

/// CPUs this process may run on, narrowed by affinity where available.
///
/// Python tries `len(os.sched_getaffinity(0))` and falls back to
/// `os.cpu_count() or 1`. Rust's `available_parallelism` is the same probe:
/// affinity-aware on Linux, active processor count on Windows.
pub fn visible_cpus() -> i64 {
    std::thread::available_parallelism()
        .map(|count| count.get() as i64)
        .unwrap_or(1)
}

/// One thread stays free below six, two at six or more; ceiling 16.
///
/// The heavy part of a worker is a single-threaded Furnace render, so the
/// count follows logical CPUs and the spare threads keep the desktop
/// responsive.
pub fn default_workers(threads: i64) -> i64 {
    if threads < 6 {
        return 1.max(threads - 1);
    }
    1.max(16.min(threads - 2))
}

/// The default worker count for this machine.
pub fn worker_limit() -> i64 {
    default_workers(visible_cpus())
}

/// Most workers an explicit `--workers` may use, and the default's ceiling.
///
/// Below eight threads every thread may run a worker; at eight or more one
/// stays free, with 31 as the safety ceiling.
pub fn worker_cap(threads: i64) -> i64 {
    if threads < 8 {
        return 1.max(threads);
    }
    1.max((threads - 1).min(WORKER_CEILING))
}

/// `(count, note)`. `requested` None uses the default, anything else is capped.
pub fn resolve_workers(requested: Option<i64>, jobs: i64) -> (i64, String) {
    resolve_workers_for(visible_cpus(), requested, jobs)
}

/// [`resolve_workers`] with the thread count supplied, so callers and tests do
/// not depend on the host CPU count.
pub fn resolve_workers_for(
    threads: i64,
    requested: Option<i64>,
    jobs: i64,
) -> (i64, String) {
    let mut note = format!("low priority, {threads} threads");
    let chosen = if let Some(requested) = requested {
        let asked = 1.max(requested);
        let cap = worker_cap(threads);
        let chosen = asked.min(cap);
        if asked > cap {
            if cap >= WORKER_CEILING {
                note.push_str(&format!(
                    ", capped at {cap} by the safety cap; past {WORKER_CEILING} workers \
                     the renders compete and the converter runs slower, not faster"
                ));
            } else if threads < 8 {
                note.push_str(&format!(", capped at {cap} (all logical threads)"));
            } else {
                note.push_str(&format!(", capped at {cap} (one thread stays free)"));
            }
        }
        chosen
    } else {
        default_workers(threads)
    };
    let chosen = if jobs > 0 { chosen.min(jobs) } else { chosen };
    (1.max(chosen), note)
}

/// Flags for `subprocess` so Furnace starts at low priority.
///
/// Python reads `subprocess.IDLE_PRIORITY_CLASS` (0x40) with
/// [`LOW_PRIORITY_CLASS`] as the fallback; both are the same value.
pub fn creationflags() -> u32 {
    #[cfg(windows)]
    {
        LOW_PRIORITY_CLASS
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// The process priority class, or `None` when it cannot be read.
pub fn current_priority() -> Option<u32> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetPriorityClass};

        // SAFETY: both calls take and return plain handles/values; the pseudo
        // handle from GetCurrentProcess needs no release.
        let value = unsafe { GetPriorityClass(GetCurrentProcess()) };
        if value != 0 { Some(value) } else { None }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Set the process priority class; `false` when the platform refuses.
pub fn set_priority(value: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass};

        // SAFETY: the pseudo handle is valid for the lifetime of the process
        // and the priority class is passed through unchanged.
        unsafe { SetPriorityClass(GetCurrentProcess(), value) != 0 }
    }
    #[cfg(not(windows))]
    {
        let _ = value;
        false
    }
}

/// Drop this process to Low. Furnace children inherit it.
pub fn set_low_priority() -> bool {
    #[cfg(windows)]
    {
        set_priority(LOW_PRIORITY_CLASS)
    }
    #[cfg(not(windows))]
    {
        #[cfg(unix)]
        {
            // SAFETY: `nice` is a plain POSIX system call taking an int.
            if unsafe { nice(10) } == -1 { false } else { true }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

/// Guard returned by [`low_priority`]; restoring on drop mirrors the
/// `try/finally` of Python's context manager.
#[derive(Debug)]
pub struct LowPriority {
    previous: Option<u32>,
}

impl Drop for LowPriority {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            set_priority(previous);
        }
    }
}

/// Run the block at low priority, then restore what the process had.
#[must_use]
pub fn low_priority() -> LowPriority {
    let previous = current_priority();
    set_low_priority();
    LowPriority { previous }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_follows_the_thread_rule() {
        // One thread stays free below six, two at six or more, ceiling 16.
        assert_eq!(default_workers(1), 1);
        assert_eq!(default_workers(2), 1);
        assert_eq!(default_workers(5), 4);
        assert_eq!(default_workers(6), 4);
        assert_eq!(default_workers(12), 10); // 6 core/12 thread desktop
        assert_eq!(default_workers(16), 14);
        assert_eq!(default_workers(22), 16); // P+E cores; ceiling applies
        assert_eq!(default_workers(64), 16);
        assert_eq!(worker_limit(), default_workers(visible_cpus()));
    }

    #[test]
    fn worker_cap_for_explicit_requests() {
        // Every thread below eight, one free from eight up, safety cap at 31.
        assert_eq!(WORKER_CEILING, 31);
        assert_eq!(worker_cap(1), 1);
        assert_eq!(worker_cap(2), 2);
        assert_eq!(worker_cap(7), 7);
        assert_eq!(worker_cap(8), 7);
        assert_eq!(worker_cap(12), 11);
        assert_eq!(worker_cap(32), 31);
        assert_eq!(worker_cap(64), 31);
        let threads = visible_cpus();
        assert_eq!(
            worker_cap(threads),
            if threads >= 8 {
                1.max((threads - 1).min(31))
            } else {
                1.max(threads)
            }
        );
    }

    #[test]
    fn resolve_workers_honors_a_request_up_to_the_cap() {
        let (count, note) = resolve_workers(None, 100);
        assert_eq!(count, worker_limit());
        assert!(note.contains("low priority"));
        assert!(note.contains("threads"));

        // Fixed thread count so the assertions do not depend on this host.
        let (count, fixed_note) = resolve_workers_for(12, None, 100);
        assert_eq!(count, 10);
        assert_eq!(fixed_note, "low priority, 12 threads");
        assert_eq!(resolve_workers_for(12, Some(1), 100).0, 1);
        // Never more workers than jobs.
        assert_eq!(resolve_workers_for(12, Some(1_000_000), 3).0, 3);
        // Within the cap: as given.
        assert_eq!(resolve_workers_for(12, Some(10), 16).0, 10);
        // Beyond it: clamped, with a reason.
        let (count, note) = resolve_workers_for(12, Some(16), 31);
        assert_eq!(count, 11);
        assert!(note.contains("capped"));
        assert!(note.ends_with(", capped at 11 (one thread stays free)"));
        // Below eight threads every logical thread may run.
        let (count, note) = resolve_workers_for(4, Some(9), 100);
        assert_eq!(count, 4);
        assert!(note.ends_with(", capped at 4 (all logical threads)"));
    }

    #[test]
    fn safety_cap_note_on_a_many_thread_cpu() {
        let (count, note) = resolve_workers_for(64, Some(64), 100);
        assert_eq!(worker_cap(64), 31);
        assert_eq!(count, 31);
        assert!(note.contains("safety cap"));
        assert!(note.contains("slower"));
        assert_eq!(
            note,
            "low priority, 64 threads, capped at 31 by the safety cap; past 31 workers \
             the renders compete and the converter runs slower, not faster"
        );
        // A missing request takes the default, which is already under the cap.
        assert_eq!(resolve_workers_for(64, None, 100).0, 16);
    }

    #[test]
    fn creation_flags_match_the_platform() {
        if cfg!(windows) {
            assert_eq!(creationflags(), LOW_PRIORITY_CLASS);
            assert_eq!(creationflags(), 64);
        } else {
            assert_eq!(creationflags(), 0);
        }
    }
}
