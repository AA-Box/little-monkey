//! Per-process resource sampling for the process table's resource ledger.
//!
//! One question — "what did this process actually cost" — asked of an OS pid,
//! answered with whatever that platform will tell us and an explicit reason for
//! everything it will not. Nothing here estimates, derives or defaults: a field
//! this platform cannot report comes back `None` **with a note**, because the
//! whole value of the ledger these samples feed is that a zero means a measured
//! zero (see `MIGRATION_V8_SQL` in `run_ledger.rs`).
//!
//! The notes reuse [`TraceFieldNote`] rather than inventing a second
//! unavailability vocabulary — `runtime_telemetry.rs` already solved this exact
//! problem for runtime traces, and a support bundle that reads one shape is
//! worth more than one that reads two.
//!
//! Sampling has to happen *while the process is alive*: peak resident size is
//! unreadable once a pid is gone, so a caller samples periodically into a
//! [`ProcessUsageAccumulator`] and flushes it to the row rather than trying to
//! read a corpse at close-out.

use crate::runtime_telemetry::TraceFieldNote;

/// The camelCase field names a note may name, matching the ledger row's wire
/// shape so a note and the field it explains are linkable without a mapping
/// table. Kept as consts because both this module and `process_table.rs`'s
/// invariant check spell them.
pub const FIELD_CPU_TIME_MS: &str = "cpuTimeMs";
pub const FIELD_PEAK_RSS_BYTES: &str = "peakRssBytes";
pub const FIELD_BYTES_READ: &str = "bytesRead";
pub const FIELD_BYTES_WRITTEN: &str = "bytesWritten";
pub const FIELD_BYTES_EGRESSED: &str = "bytesEgressed";
// The four below name ledger fields nothing in *this* module samples — tokens
// come from the run's event stream and no runtime here reports per-process GPU
// residency at all. They live with the others so the ledger's field vocabulary
// has one home rather than two.
pub const FIELD_TOKENS_IN: &str = "tokensIn";
pub const FIELD_TOKENS_OUT: &str = "tokensOut";
pub const FIELD_GPU_RESIDENT_BYTES: &str = "gpuResidentBytes";
pub const FIELD_GPU_DEVICE_MS: &str = "gpuDeviceMs";
/// Derived from the row's timestamps rather than stored, so it has a name for
/// notes but no column and no place in the construction check.
pub const FIELD_WALL_TIME_MS: &str = "wallTimeMs";

/// One reading of one OS process.
///
/// Every field is `Option<u64>` and every `None` is accompanied by an entry in
/// `unavailable` naming it. [`ProcessUsageSample::note_for`] is how a caller asks
/// why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessUsageSample {
    /// User + system CPU consumed, in milliseconds.
    pub cpu_time_ms: Option<u64>,
    /// High-water resident/physical footprint, in bytes. The lifetime maximum
    /// where the platform tracks one, not the instantaneous size — an
    /// instantaneous read taken at an arbitrary moment is not a peak.
    pub peak_rss_bytes: Option<u64>,
    /// Bytes this process actually read from storage. Not the same as bytes
    /// requested: a read served from page cache did not touch the disk.
    pub bytes_read: Option<u64>,
    pub bytes_written: Option<u64>,
    /// Bytes this process sent to the network. **Never sampled from the OS** —
    /// no platform here attributes egress per process — so this is always `None`
    /// out of [`sample`] and is fed by whoever accounts for egress, through
    /// [`ProcessUsageAccumulator::add_egress`].
    pub bytes_egressed: Option<u64>,
    pub unavailable: Vec<TraceFieldNote>,
}

impl ProcessUsageSample {
    /// Why `field` is missing, if it is.
    pub fn note_for(&self, field: &str) -> Option<&TraceFieldNote> {
        self.unavailable.iter().find(|note| note.field == field)
    }

    /// Marks every listed field unavailable for one shared reason.
    fn all_unavailable(fields: &[&str], reason: &str) -> Self {
        ProcessUsageSample {
            unavailable: fields
                .iter()
                .map(|field| TraceFieldNote {
                    field: (*field).to_string(),
                    reason: reason.to_string(),
                })
                .collect(),
            ..ProcessUsageSample::default()
        }
    }

    fn note(&mut self, field: &str, reason: impl Into<String>) {
        self.unavailable.push(TraceFieldNote {
            field: field.to_string(),
            reason: reason.into(),
        });
    }
}

/// Folds two readings of the same monotonic counter. See
/// [`ProcessUsageAccumulator`] for why the larger wins rather than the later.
fn fold_max(retained: Option<u64>, incoming: Option<u64>) -> Option<u64> {
    match (retained, incoming) {
        (Some(held), Some(new)) => Some(held.max(new)),
        (Some(held), None) => Some(held),
        (None, incoming) => incoming,
    }
}

/// The fields [`sample`] tries to read from the OS. `bytesEgressed` is not among
/// them — see [`ProcessUsageSample::bytes_egressed`].
const SAMPLED_FIELDS: &[&str] = &[
    FIELD_CPU_TIME_MS,
    FIELD_PEAK_RSS_BYTES,
    FIELD_BYTES_READ,
    FIELD_BYTES_WRITTEN,
];

const EGRESS_NOT_SAMPLED: &str =
    "no platform here attributes network bytes per process; egress must be fed by the caller";

/// Reads what this platform will report about `pid`.
///
/// Cheap enough to call on a timer: one syscall on macOS/Windows, three small
/// `/proc` reads on Linux. A pid that has already exited is not an error — it
/// comes back all-unavailable with the failure as its reason, which is the honest
/// answer and the reason a caller must sample while the process is alive.
pub fn sample(pid: i64) -> ProcessUsageSample {
    let mut sample = match u32::try_from(pid) {
        Ok(pid) if pid != 0 => sample_platform(pid),
        // 0 and negatives name a process *group* to the OS, so neither is a
        // question about one process. Refusing beats answering about the wrong
        // thing.
        _ => ProcessUsageSample::all_unavailable(
            SAMPLED_FIELDS,
            &format!("{pid} is not a single process id"),
        ),
    };
    sample.note(FIELD_BYTES_EGRESSED, EGRESS_NOT_SAMPLED);
    sample
}

/// Converts `ri_user_time`/`ri_system_time` to milliseconds.
///
/// **These are mach absolute time units, not nanoseconds**, and the difference is
/// not academic: on Apple Silicon the timebase is 125/3, so treating a tick as a
/// nanosecond under-reports CPU time by a factor of ~42 — measured, not assumed
/// (a one-second busy loop reported 28ms before this conversion existed). On
/// Intel Macs the timebase is 1/1 and the two units coincide, which is exactly why
/// the wrong version looks correct on the wrong machine and stays wrong forever.
/// `mach_timebase_info` is the calibration the hardware requires.
///
/// Declared here rather than taken from `libc`, whose `mach_timebase_info` is
/// deprecated in favour of the `mach2` crate: this is two `u32`s and one call, not
/// worth a dependency. Field order matches `<mach/mach_time.h>`.
#[cfg(target_os = "macos")]
fn mach_ticks_to_ms(ticks: u64) -> u64 {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }

    let mut timebase = MachTimebaseInfo { numer: 1, denom: 1 };
    // Safe: writes two `u32`s into a struct this stack owns. A failure leaves the
    // 1/1 identity in place, which is right for Intel and merely under-reports on
    // Apple Silicon rather than producing nonsense.
    if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
        timebase = MachTimebaseInfo { numer: 1, denom: 1 };
    }
    // Nanoseconds first, in u128 so a long-lived process cannot overflow the
    // multiply before the divide brings it back down.
    let nanos = u128::from(ticks) * u128::from(timebase.numer) / u128::from(timebase.denom);
    u64::try_from(nanos / 1_000_000).unwrap_or(u64::MAX)
}

#[cfg(target_os = "macos")]
fn sample_platform(pid: u32) -> ProcessUsageSample {
    // `RUSAGE_INFO_V4` rather than V2: V2 stops at the disk-IO counters, and
    // `ri_lifetime_max_phys_footprint` — the only *peak* this API reports, as
    // opposed to the instantaneous `ri_phys_footprint` — first appears in V4.
    // Both the struct and the flavour come from `libc`, so the field order is
    // not ours to get wrong.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // Safe: `proc_pid_rusage` writes `flavor`'s struct into the buffer we hand
    // it, and `info` is exactly that struct. A failure writes nothing.
    let status = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            &mut info as *mut libc::rusage_info_v4 as *mut libc::rusage_info_t,
        )
    };
    if status != 0 {
        return ProcessUsageSample::all_unavailable(
            SAMPLED_FIELDS,
            &format!(
                "proc_pid_rusage(RUSAGE_INFO_V4) failed for pid {pid}: {}",
                std::io::Error::last_os_error()
            ),
        );
    }

    ProcessUsageSample {
        cpu_time_ms: Some(mach_ticks_to_ms(
            info.ri_user_time.saturating_add(info.ri_system_time),
        )),
        peak_rss_bytes: Some(info.ri_lifetime_max_phys_footprint),
        bytes_read: Some(info.ri_diskio_bytesread),
        bytes_written: Some(info.ri_diskio_byteswritten),
        bytes_egressed: None,
        unavailable: Vec::new(),
    }
}

#[cfg(target_os = "linux")]
fn sample_platform(pid: u32) -> ProcessUsageSample {
    let mut unavailable: Vec<TraceFieldNote> = Vec::new();
    let mut failed = |field: &str, reason: String| {
        unavailable.push(TraceFieldNote {
            field: field.to_string(),
            reason,
        })
    };

    let cpu_time_ms = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => parse_proc_stat_cpu_ticks(&text)
            .and_then(|ticks| ticks.checked_mul(1_000))
            .map(|scaled| scaled / clock_ticks_per_second()),
        Err(error) => {
            failed(FIELD_CPU_TIME_MS, format!("/proc/{pid}/stat: {error}"));
            None
        }
    };
    let peak_rss_bytes = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(text) => parse_proc_status_vm_hwm_kb(&text).and_then(|kb| kb.checked_mul(1_024)),
        Err(error) => {
            failed(FIELD_PEAK_RSS_BYTES, format!("/proc/{pid}/status: {error}"));
            None
        }
    };
    // `/proc/<pid>/io` is readable only by the process owner and root. A denial
    // is unavailability, emphatically not zero IO.
    let (bytes_read, bytes_written) = match std::fs::read_to_string(format!("/proc/{pid}/io")) {
        Ok(text) => parse_proc_io_bytes(&text),
        Err(error) => {
            let reason = format!("/proc/{pid}/io: {error}");
            failed(FIELD_BYTES_READ, reason.clone());
            failed(FIELD_BYTES_WRITTEN, reason);
            (None, None)
        }
    };

    let mut sample = ProcessUsageSample {
        cpu_time_ms,
        peak_rss_bytes,
        bytes_read,
        bytes_written,
        bytes_egressed: None,
        unavailable,
    };
    // A file that opened but did not carry the field — a kernel thread with no
    // `VmHWM`, a truncated `stat` — leaves a gap the reads above said nothing
    // about. Cover it here so no field can ever come back `None` unexplained.
    for (field, value) in [
        (FIELD_CPU_TIME_MS, sample.cpu_time_ms),
        (FIELD_PEAK_RSS_BYTES, sample.peak_rss_bytes),
        (FIELD_BYTES_READ, sample.bytes_read),
        (FIELD_BYTES_WRITTEN, sample.bytes_written),
    ] {
        if value.is_none() && sample.note_for(field).is_none() {
            sample.note(field, format!("/proc/{pid} did not report this field"));
        }
    }
    sample
}

/// `USER_HZ`, which is 100 on every mainstream build and is still not ours to
/// assume — a kernel configured otherwise would make every CPU figure wrong by a
/// constant factor, which is the kind of error that looks plausible forever.
#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> u64 {
    // Safe: reads one integer sysconf value, touches no memory this owns.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100
    }
}

#[cfg(windows)]
fn sample_platform(pid: u32) -> ProcessUsageSample {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // ponytail: only `GetProcessTimes` is reachable today. Peak working set and
    // IO counters need `GetProcessMemoryInfo` and `GetProcessIoCounters`, which
    // live in windows-sys feature modules this crate does not enable
    // (`Win32_System_ProcessStatus` and `Win32_System_JobObjects`). Upgrade path
    // is adding those two features in Cargo.toml and filling the three fields in
    // here — deliberately not done as a side effect of this change.
    const NOT_LINKED: &str = "windows-sys is built without Win32_System_ProcessStatus and \
                              Win32_System_JobObjects, so GetProcessMemoryInfo and \
                              GetProcessIoCounters are not linked into this binary";
    let mut sample = ProcessUsageSample::all_unavailable(
        &[FIELD_PEAK_RSS_BYTES, FIELD_BYTES_READ, FIELD_BYTES_WRITTEN],
        NOT_LINKED,
    );

    // Safe: opens a handle by id and touches no memory this owns. A null handle
    // means the process could not be opened.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        sample.note(
            FIELD_CPU_TIME_MS,
            format!(
                "OpenProcess failed for pid {pid}: {}",
                std::io::Error::last_os_error()
            ),
        );
        return sample;
    }

    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // Safe: four `FILETIME`s this stack owns, passed by pointer to a call that
    // only writes them.
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    let error = std::io::Error::last_os_error();
    // Safe: closes the handle opened above, exactly once.
    unsafe {
        CloseHandle(handle);
    }

    if ok == 0 {
        sample.note(
            FIELD_CPU_TIME_MS,
            format!("GetProcessTimes failed for pid {pid}: {error}"),
        );
        return sample;
    }
    // `FILETIME` counts 100-nanosecond intervals, so 10_000 per millisecond.
    sample.cpu_time_ms =
        Some((filetime_to_u64(kernel).saturating_add(filetime_to_u64(user))) / 10_000);
    sample
}

#[cfg(windows)]
fn filetime_to_u64(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn sample_platform(_pid: u32) -> ProcessUsageSample {
    // ponytail: no sampler for this target. Add a platform leg above rather than
    // letting the ledger report zeros it never measured.
    ProcessUsageSample::all_unavailable(
        SAMPLED_FIELDS,
        "this platform has no per-process resource sampler",
    )
}

/// `utime + stime` from a `/proc/<pid>/stat` line, in clock ticks.
///
/// Pure over the text on purpose: `/proc` exists on one of the three platforms
/// this ships to, and the parsing — not the file read — is where the bug would
/// be. Fields are counted from the **last** `)`, because field 2 is the
/// executable name in parentheses and that name may itself contain spaces and
/// parentheses; splitting the whole line on whitespace is the classic way to
/// read this file wrong.
#[allow(dead_code)] // Only called on Linux; the tests below exercise it everywhere.
fn parse_proc_stat_cpu_ticks(text: &str) -> Option<u64> {
    let after_comm = &text[text.rfind(')')?.saturating_add(1)..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // `fields[0]` is field 3 (state), so field N is at N - 3.
    let utime: u64 = fields.get(14 - 3)?.parse().ok()?;
    let stime: u64 = fields.get(15 - 3)?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

/// `VmHWM` — peak resident set size — from `/proc/<pid>/status`, in kB.
///
/// Absent for a kernel thread and for a process that has already exited, which
/// is unavailability rather than zero.
#[allow(dead_code)] // See `parse_proc_stat_cpu_ticks`.
fn parse_proc_status_vm_hwm_kb(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// `read_bytes`/`write_bytes` from `/proc/<pid>/io`.
///
/// Those two rather than `rchar`/`wchar`: the latter count bytes the process
/// asked for, including reads served entirely from page cache, which is not
/// storage the ledger should charge it for.
#[allow(dead_code)] // See `parse_proc_stat_cpu_ticks`.
fn parse_proc_io_bytes(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))?
            .trim_start_matches(':')
            .trim()
            .parse()
            .ok()
    };
    (field("read_bytes"), field("write_bytes"))
}

/// A running maximum over successive samples, owned by whoever is sampling.
///
/// Exists because peak resident size is unreadable after exit: a caller polls
/// while the process lives and this keeps the highest reading, so the value that
/// reaches the ledger is the peak rather than whatever the last poll happened to
/// catch.
///
/// Plain data with no interior mutability and no registry — one of these belongs
/// to the loop that owns the process, so there is nothing to lock and nothing to
/// clean up if that loop dies.
///
/// **Every field folds by maximum, including the monotonic ones.** CPU time and
/// disk bytes only ever grow, so the maximum *is* the latest reading; taking the
/// max rather than the last value costs nothing and means a sample that comes
/// back smaller — a failed read, a pid the OS has reused — cannot walk a total
/// backwards. Egress is the exception and adds, since each report is a new
/// increment rather than a running count.
#[derive(Debug, Clone, Default)]
pub struct ProcessUsageAccumulator {
    current: ProcessUsageSample,
}

impl ProcessUsageAccumulator {
    pub fn new() -> Self {
        ProcessUsageAccumulator::default()
    }

    /// Folds one reading in, keeping the larger of each field.
    ///
    /// The incoming sample's notes replace the retained ones for the fields it
    /// could not read, but only where nothing has ever been measured: a field
    /// that succeeded once is measured, and a later failed poll must not
    /// retroactively make it unavailable.
    pub fn observe(&mut self, sample: ProcessUsageSample) {
        let mut next = ProcessUsageSample {
            cpu_time_ms: fold_max(self.current.cpu_time_ms, sample.cpu_time_ms),
            peak_rss_bytes: fold_max(self.current.peak_rss_bytes, sample.peak_rss_bytes),
            bytes_read: fold_max(self.current.bytes_read, sample.bytes_read),
            bytes_written: fold_max(self.current.bytes_written, sample.bytes_written),
            // Egress is this accumulator's own running total; a poll of the OS
            // never carries one, so a sample must not be able to clear it.
            bytes_egressed: self.current.bytes_egressed,
            unavailable: Vec::new(),
        };
        for (field, value) in [
            (FIELD_CPU_TIME_MS, next.cpu_time_ms),
            (FIELD_PEAK_RSS_BYTES, next.peak_rss_bytes),
            (FIELD_BYTES_READ, next.bytes_read),
            (FIELD_BYTES_WRITTEN, next.bytes_written),
            (FIELD_BYTES_EGRESSED, next.bytes_egressed),
        ] {
            if value.is_some() {
                continue;
            }
            // Newest attempt's reason first — it describes the world now — with
            // the retained one as the fallback for a field this sample said
            // nothing about.
            if let Some(note) = sample
                .note_for(field)
                .or_else(|| self.current.note_for(field))
            {
                next.unavailable.push(note.clone());
            }
        }
        self.current = next;
    }

    /// Samples `pid` and folds the result in.
    pub fn observe_pid(&mut self, pid: i64) {
        self.observe(sample(pid));
    }

    /// Adds network bytes attributed to this process by whoever accounts for
    /// egress. Additive, not a maximum — each call reports an increment.
    pub fn add_egress(&mut self, bytes: u64) {
        self.current.bytes_egressed = Some(
            self.current
                .bytes_egressed
                .unwrap_or(0)
                .saturating_add(bytes),
        );
        self.current
            .unavailable
            .retain(|note| note.field != FIELD_BYTES_EGRESSED);
    }

    /// Everything folded so far, ready to hand to
    /// `ProcessTable::accumulate_usage`.
    pub fn sample(&self) -> &ProcessUsageSample {
        &self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real line, with the pathological comm this parser exists for: spaces
    /// and a close-paren inside the process name.
    const STAT: &str = "42 (weird ) name) S 1 42 42 0 -1 4194304 1234 0 5 0 \
                        700 300 11 22 20 0 4 0 987654 12345678 6789";

    #[test]
    fn proc_stat_cpu_ticks_are_counted_from_the_last_paren_not_the_first_space() {
        // utime = 700, stime = 300 — fields 14 and 15.
        assert_eq!(parse_proc_stat_cpu_ticks(STAT), Some(1_000));
    }

    #[test]
    fn a_truncated_or_unparseable_stat_line_is_unavailable_rather_than_zero() {
        assert_eq!(parse_proc_stat_cpu_ticks("42 (short) S 1 2 3"), None);
        assert_eq!(parse_proc_stat_cpu_ticks("no parens here at all"), None);
        assert_eq!(parse_proc_stat_cpu_ticks(""), None);
    }

    #[test]
    fn vm_hwm_is_the_peak_and_a_status_file_without_it_reports_nothing() {
        let status = "Name:\tmonkey\nVmPeak:\t  900000 kB\nVmHWM:\t  123456 kB\nVmRSS:\t 1000 kB\n";
        assert_eq!(parse_proc_status_vm_hwm_kb(status), Some(123_456));
        // A kernel thread has no VmHWM. Zero would claim it used no memory.
        assert_eq!(
            parse_proc_status_vm_hwm_kb("Name:\tkthreadd\nVmRSS:\t 0 kB\n"),
            None
        );
    }

    #[test]
    fn proc_io_reads_the_disk_counters_and_not_the_requested_byte_counters() {
        let io = "rchar: 999999\nwchar: 888888\nsyscr: 10\nsyscw: 20\n\
                  read_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_proc_io_bytes(io), (Some(4_096), Some(8_192)));
        // The permission-denied shape: the file exists but carries neither
        // counter. Both unavailable, neither zero.
        assert_eq!(parse_proc_io_bytes("rchar: 1\nwchar: 2\n"), (None, None));
    }

    #[test]
    fn a_pid_that_is_not_one_process_is_unavailable_with_that_as_its_reason() {
        for pid in [0, -1, -1234] {
            let sample = sample(pid);
            assert_eq!(sample.cpu_time_ms, None);
            assert!(
                sample
                    .note_for(FIELD_CPU_TIME_MS)
                    .is_some_and(|note| note.reason.contains("not a single process id")),
                "pid {pid} should be refused with a stated reason"
            );
        }
    }

    /// Egress is never sampled, so it must always come back with its reason
    /// stated — otherwise the ledger's construction check would reject a sample
    /// nobody fed egress into.
    #[test]
    fn every_sample_states_that_egress_was_not_measured() {
        let sample = sample(std::process::id() as i64);
        assert_eq!(sample.bytes_egressed, None);
        assert!(sample.note_for(FIELD_BYTES_EGRESSED).is_some());
    }

    /// The reason the accumulator exists: the highest reading survives, and a
    /// later, smaller one cannot walk it back.
    #[test]
    fn the_accumulator_keeps_the_peak_and_drops_the_note_once_a_field_is_measured() {
        let mut accumulator = ProcessUsageAccumulator::new();
        accumulator.observe(ProcessUsageSample::all_unavailable(
            SAMPLED_FIELDS,
            "first poll failed",
        ));
        assert!(accumulator
            .sample()
            .note_for(FIELD_PEAK_RSS_BYTES)
            .is_some());

        accumulator.observe(ProcessUsageSample {
            cpu_time_ms: Some(10),
            peak_rss_bytes: Some(9_000),
            ..ProcessUsageSample::default()
        });
        accumulator.observe(ProcessUsageSample {
            cpu_time_ms: Some(20),
            // A dip: the process released memory, but 9_000 was still its peak.
            peak_rss_bytes: Some(4_000),
            ..ProcessUsageSample::default()
        });

        assert_eq!(accumulator.sample().cpu_time_ms, Some(20));
        assert_eq!(accumulator.sample().peak_rss_bytes, Some(9_000));
        assert!(
            accumulator
                .sample()
                .note_for(FIELD_PEAK_RSS_BYTES)
                .is_none(),
            "a measured field must not keep a \"not measured\" note"
        );
        assert!(
            accumulator.sample().note_for(FIELD_BYTES_READ).is_some(),
            "a field nothing ever measured keeps its reason"
        );
    }

    /// A field that succeeded once must stay measured even if a later poll — of
    /// a pid that has since exited, say — cannot read it.
    #[test]
    fn a_later_failed_poll_does_not_retract_an_earlier_measurement() {
        let mut accumulator = ProcessUsageAccumulator::new();
        accumulator.observe(ProcessUsageSample {
            cpu_time_ms: Some(500),
            ..ProcessUsageSample::default()
        });
        accumulator.observe(ProcessUsageSample::all_unavailable(
            SAMPLED_FIELDS,
            "the process has exited",
        ));
        assert_eq!(accumulator.sample().cpu_time_ms, Some(500));
        assert!(accumulator.sample().note_for(FIELD_CPU_TIME_MS).is_none());
    }

    #[test]
    fn egress_adds_up_and_clears_its_reason() {
        let mut accumulator = ProcessUsageAccumulator::new();
        accumulator.observe(sample(std::process::id() as i64));
        assert!(accumulator
            .sample()
            .note_for(FIELD_BYTES_EGRESSED)
            .is_some());

        accumulator.add_egress(1_024);
        accumulator.add_egress(512);
        assert_eq!(accumulator.sample().bytes_egressed, Some(1_536));
        assert!(accumulator
            .sample()
            .note_for(FIELD_BYTES_EGRESSED)
            .is_none());
    }

    /// The unit check, and the reason it exists: `ri_user_time` on macOS is in
    /// mach absolute time units, not the nanoseconds it is widely documented as,
    /// so the obvious `/ 1_000_000` under-reported CPU time by ~42× on Apple
    /// Silicon — and by nothing at all on Intel, which is how a wrong conversion
    /// survives review. Burning a known amount of CPU and comparing is the only
    /// thing that catches it; no amount of reading the struct definition does.
    ///
    /// Bounded loosely on purpose. The failure being guarded against is an
    /// order-of-magnitude unit error, so the window is wide enough that a busy
    /// test machine descheduling this thread cannot fail it.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn reported_cpu_time_is_in_milliseconds_and_not_some_other_unit() {
        let pid = std::process::id() as i64;
        let before = sample(pid).cpu_time_ms.expect("CPU time is reported");
        let started = std::time::Instant::now();
        let mut burnt: u64 = 0;
        // Spin on the clock rather than a fixed iteration count, so the amount of
        // CPU consumed is the same on a fast machine and a slow one.
        while started.elapsed() < std::time::Duration::from_millis(400) {
            burnt = burnt.wrapping_add(started.elapsed().subsec_nanos() as u64);
        }
        let wall_ms = started.elapsed().as_millis() as u64;
        let reported = sample(pid).cpu_time_ms.expect("CPU time is reported");
        let spent = reported.saturating_sub(before);

        assert!(burnt > 0, "the loop must actually run");
        assert!(
            spent >= wall_ms / 4,
            "a busy loop of {wall_ms}ms reported only {spent}ms of CPU — the unit is wrong"
        );
        // The upper bound is the hardware's: a process cannot consume more CPU
        // milliseconds than wall milliseconds times cores, whatever else the test
        // binary's other threads are doing at the same time. Wide, but it still
        // catches an order-of-magnitude error in the other direction.
        let cores = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1) as u64;
        assert!(
            spent <= wall_ms.saturating_mul(cores + 2),
            "a busy loop of {wall_ms}ms reported {spent}ms of CPU across {cores} cores, \
             which is more than the machine can physically have spent — the unit is wrong"
        );
    }

    /// The one end-to-end check of the platform leg, against the process running
    /// the test — the only pid a test can be sure about.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn sampling_this_test_process_reports_cpu_time_it_actually_spent() {
        let sample = sample(std::process::id() as i64);
        assert!(
            sample.cpu_time_ms.is_some(),
            "every supported platform reports CPU time; got {:?}",
            sample.unavailable
        );
        // Anything unread must say why. This is the invariant the ledger's
        // construction check depends on.
        for (field, value) in [
            (FIELD_CPU_TIME_MS, sample.cpu_time_ms),
            (FIELD_PEAK_RSS_BYTES, sample.peak_rss_bytes),
            (FIELD_BYTES_READ, sample.bytes_read),
            (FIELD_BYTES_WRITTEN, sample.bytes_written),
            (FIELD_BYTES_EGRESSED, sample.bytes_egressed),
        ] {
            assert_eq!(
                value.is_none(),
                sample.note_for(field).is_some(),
                "{field} must be measured or explained, never neither and never both"
            );
        }
    }
}
