//! Native enumeration of an owned process tree.
//!
//! The measurement half of K4's supervised enforcement. [`crate::process_usage`]
//! answers "what did *this pid* cost"; this module answers "what does the tree
//! rooted at this pid cost, and how many processes are in it" — which is the
//! question a memory or child-process limit is actually about. A shell that
//! spawns a compiler is the normal case, not a trick, so a bound that measures
//! only the pid we spawned bounds nothing.
//!
//! # Why not `ps`
//!
//! The daemon's watchdog forks `ps -eo pgid=,rss=` every sample and parses its
//! columns. That works and has tests, but it costs a fork per sample, it cannot
//! report a *parent* link (so it can only ever measure a process **group**, which
//! a descendant escapes with one `setsid`), and its column set differs between
//! BSD and procps in ways that are silent when wrong. Everything here reads the
//! kernel directly:
//!
//! - **Linux** — `/proc/<pid>/stat` for the parent and group links, `VmRSS` from
//!   `/proc/<pid>/status` for residency.
//! - **macOS** — one `sysctl(KERN_PROC_ALL)` for the whole table with parent and
//!   group links, then `proc_pid_rusage` per member for its physical footprint.
//! - **Windows** — a ToolHelp snapshot for the parent links and
//!   `GetProcessMemoryInfo` for the working set. Windows jobs are authoritative
//!   where one is held; this is the path for the processes no job owns.
//!
//! # Membership: parent closure *and* process group
//!
//! A member is in the tree if it is reachable from the root by parent links **or**
//! it carries the root's process group id. Both, because each covers the other's
//! escape: re-parenting to init (a daemonising child) breaks the parent chain but
//! keeps the group, and `setsid` breaks the group but leaves the parent chain
//! intact until the parent exits. Neither is a container — a descendant that does
//! both deliberately is the macOS lifetime gap K4 still records — but requiring
//! *both* escapes rather than either one is the difference between a bound an
//! ordinary `make -j` evades and one it does not.
//!
//! Cycles are possible in reported parent ids (pid reuse, and Windows reports pid
//! 0 as its own parent), so the closure is iterated to a fixed point over a
//! visited set rather than recursed.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

/// One process as the kernel reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessNode {
    pub pid: u32,
    pub parent_pid: u32,
    /// Process group id. Zero where the platform has no such concept (Windows).
    pub process_group_id: u32,
}

/// What a tree is holding right now.
///
/// Instantaneous, not peak: a supervisor folds successive samples into a
/// high-water mark itself. `rss_bytes` is `None` when the platform refused every
/// member's residency — distinct from `Some(0)`, which is a tree that exists and
/// holds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeUsage {
    /// Summed resident/physical footprint over every live member.
    pub rss_bytes: Option<u64>,
    /// Live members, including the root.
    pub process_count: u32,
    /// Members whose residency could not be read, so a caller can tell a small
    /// number from an under-measured one.
    pub unmeasured_members: u32,
}

/// Every process on the host, or an error if the platform would not say.
///
/// Public because the tests exercise the closure against a real snapshot, and
/// because a caller sampling several trees at once should pay for one snapshot
/// rather than one per tree.
pub fn snapshot() -> io::Result<Vec<ProcessNode>> {
    snapshot_platform()
}

/// The members of the tree rooted at `root_pid`, by parent closure unioned with
/// process-group membership.
///
/// Pure, and separated from [`snapshot`] for exactly that reason: the membership
/// rule is the part that can be wrong in a way no passing spawn reveals, so it is
/// tested against hand-written tables on every platform.
#[must_use]
pub fn tree_members(nodes: &[ProcessNode], root_pid: u32) -> BTreeSet<u32> {
    tree_members_of_any(nodes, &[root_pid])
}

/// [`tree_members`] over several roots at once, in one pass.
///
/// A supervisor that has recorded members the closure can no longer reach — a
/// descendant whose ancestors have exited — has to expand from each of them as
/// well as from the original root. Doing that by calling [`tree_members`] per
/// root rebuilds the parent index once per member, which for a forty-process
/// tree on a busy host is forty walks of the whole table every sampling tick.
#[must_use]
pub fn tree_members_of_any(nodes: &[ProcessNode], roots: &[u32]) -> BTreeSet<u32> {
    let mut members = BTreeSet::new();

    let by_parent: BTreeMap<u32, Vec<u32>> = nodes.iter().fold(BTreeMap::new(), |mut map, node| {
        // A process that reports itself as its own parent would otherwise
        // seed a self-edge; the visited set below already handles it, but
        // dropping it here keeps the map honest.
        if node.parent_pid != node.pid {
            map.entry(node.parent_pid).or_default().push(node.pid);
        }
        map
    });

    // Parent closure, iterated rather than recursed: a reported parent cycle must
    // terminate the walk, not the watchdog.
    let mut frontier = Vec::new();
    for root_pid in roots {
        // Nothing owns pid 0, and treating it as a root would select the whole
        // machine on a platform that reports 0 as an ancestor.
        if *root_pid == 0 {
            continue;
        }
        if members.insert(*root_pid) {
            frontier.push(*root_pid);
        }
    }
    while let Some(pid) = frontier.pop() {
        for child in by_parent.get(&pid).into_iter().flatten() {
            if members.insert(*child) {
                frontier.push(*child);
            }
        }
    }

    // Group union. A group id of zero is "no group", never "every ungrouped
    // process on the host".
    let groups: BTreeSet<u32> = roots
        .iter()
        .filter_map(|root_pid| {
            nodes
                .iter()
                .find(|node| node.pid == *root_pid)
                .map(|node| node.process_group_id)
        })
        .filter(|group| *group != 0)
        .collect();
    if !groups.is_empty() {
        for node in nodes {
            if groups.contains(&node.process_group_id) {
                members.insert(node.pid);
            }
        }
    }

    members
}

/// Measure the tree rooted at `root_pid`.
///
/// `Ok(None)` means the root is gone — which is what an exited process is, and
/// deliberately not `Some(TreeUsage { process_count: 0, .. })`: a zero-process
/// tree holding zero bytes is a budget trivially satisfied forever, and a
/// watchdog that reads one as a measurement will never fire again.
pub fn measure_tree(root_pid: u32) -> io::Result<Option<TreeUsage>> {
    let nodes = snapshot()?;
    Ok(measure_tree_in(&nodes, root_pid))
}

/// [`measure_tree`] against an already-taken snapshot.
pub fn measure_tree_in(nodes: &[ProcessNode], root_pid: u32) -> Option<TreeUsage> {
    if !nodes.iter().any(|node| node.pid == root_pid) {
        return None;
    }
    measure_members(&tree_members(nodes, root_pid))
}

/// Measure an already-derived membership set.
///
/// Split out because a supervisor's membership is not always a single closure:
/// it is the closure unioned with descendants it recorded before their ancestry
/// was destroyed, and those have to be measured on the same terms.
///
/// `None` for an empty set, on the same reasoning as [`measure_tree_in`]: zero
/// processes holding zero bytes is a budget satisfied forever.
#[must_use]
pub fn measure_members(members: &BTreeSet<u32>) -> Option<TreeUsage> {
    if members.is_empty() {
        return None;
    }
    let mut rss_bytes: Option<u64> = None;
    let mut unmeasured_members = 0u32;
    for pid in members {
        match resident_bytes(*pid) {
            Some(bytes) => rss_bytes = Some(rss_bytes.unwrap_or(0).saturating_add(bytes)),
            // A member that exited between the snapshot and this read is not an
            // unmeasured member — it is not a member. Only a live-but-unreadable
            // one counts, and the platform reads below cannot tell them apart, so
            // this over-reports rather than under-reports the gap.
            None => unmeasured_members = unmeasured_members.saturating_add(1),
        }
    }
    Some(TreeUsage {
        rss_bytes,
        process_count: u32::try_from(members.len()).unwrap_or(u32::MAX),
        unmeasured_members,
    })
}

/// A pid paired with the start time that makes it unambiguous.
///
/// Pids are reused, and a supervisor that samples or signals a bare pid will
/// eventually sample or signal whatever inherited it. The start time is fixed for
/// the life of a process and monotonic across reuse, so the pair is a stable
/// identity — this is what a resource controller stores, and what it re-checks
/// before every sample and every terminate.
///
/// The unit differs per platform (Mach absolute seconds, kernel jiffies since
/// boot, Windows FILETIME) and is deliberately not normalised: nothing compares
/// it across hosts, and converting it would invite comparing it across boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

impl ProcessIdentity {
    /// The identity of `pid` right now, or `None` if nothing is there.
    pub fn of(pid: u32) -> Option<Self> {
        Some(ProcessIdentity {
            pid,
            start_time: process_start_time(pid)?,
        })
    }

    /// Whether the pid still refers to the same process this identity named.
    ///
    /// A platform that reports no start time answers `true` from liveness alone
    /// — degraded, and better than refusing to supervise at all — which is why
    /// this is a named method rather than a bare `==` at each call site.
    #[must_use]
    pub fn is_still_alive(&self) -> bool {
        match process_start_time(self.pid) {
            Some(start_time) => start_time == self.start_time,
            None => false,
        }
    }

    /// Whether this identity is still *executing*, as opposed to merely existing.
    ///
    /// A killed process that its parent has not reaped is a zombie: it holds no
    /// memory, runs no code and cannot fork, but it still has a `/proc` entry and
    /// a start time, so [`Self::is_still_alive`] answers `true` for it. A
    /// termination pass that used that answer to decide whether anything survived
    /// would loop until its budget ran out and then report a failure to reclaim a
    /// tree it had in fact reclaimed.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.is_still_alive() && !is_zombie(self.pid)
    }
}

/// Every owned member of the tree rooted at `root_pid`, as identities.
///
/// Identities rather than bare pids because capture and signal are separated in
/// time — a pid captured before a group is torn down and signalled after it may
/// by then name a process the kernel handed to somebody else.
#[must_use]
pub fn owned_identities(nodes: &[ProcessNode], root_pid: u32) -> Vec<ProcessIdentity> {
    tree_members(nodes, root_pid)
        .into_iter()
        .filter_map(ProcessIdentity::of)
        .collect()
}

/// Whether `pid` is a child of *this* process that has exited and not yet been
/// reaped.
///
/// The one question that is answerable about a corpse on every Unix host.
/// `/proc/<pid>/stat` reports a Linux zombie's state, but macOS refuses
/// `proc_pidinfo` for one entirely — so a zombie there has no start time, no
/// status, and `kill(pid, 0)` still succeeds, which reads as "running" to
/// everything else here.
///
/// `waitid` with `WNOWAIT` answers it without consuming the exit status, so the
/// real owner can still `wait` for it afterwards. Only valid for our own
/// children, which is exactly the case the containment check faces: the pid it
/// is asked about is the one this process just spawned.
#[cfg(unix)]
#[must_use]
pub fn child_exited_unreaped(pid: u32) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // Safe: writes one `siginfo_t` this stack owns. `WNOHANG` cannot block and
    // `WNOWAIT` leaves the child collectable by whoever owns it.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        // ECHILD means it is not our child, which is not the same as "running";
        // the caller's other checks own that case.
        return false;
    }
    // Safe: `waitid` initialised the struct when it returned success.
    let info = unsafe { info.assume_init() };
    // A zero pid means `WNOHANG` found nothing to report, i.e. the child is still
    // running; anything else is the child whose exit it is reporting.
    let reported = unsafe { info.si_pid() };
    reported != 0
}

/// Windows has no zombie state and no `waitid`: a pid is released at exit.
#[cfg(not(unix))]
#[must_use]
pub fn child_exited_unreaped(_pid: u32) -> bool {
    false
}

// --- Zombie detection --------------------------------------------------------

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    parse_proc_stat_is_zombie(&stat)
}

/// The state field, which is the first token after the parenthesised comm — so
/// the same last-`)` rule the link and start-time parsers use applies here.
#[cfg(target_os = "linux")]
fn parse_proc_stat_is_zombie(stat: &str) -> bool {
    let Some(index) = stat.rfind(')') else {
        return false;
    };
    stat[index + 1..].split_whitespace().next() == Some("Z")
}

#[cfg(target_os = "macos")]
fn is_zombie(pid: u32) -> bool {
    /// `SZOMB` from `sys/proc.h`: awaiting collection by its parent.
    const SZOMB: u32 = 5;
    proc_bsd_info(pid).is_some_and(|info| info.pbi_status == SZOMB)
}

/// Windows has no zombie state: a process object outlives its exit only as a
/// handle, and the pid is released at exit, so nothing can be both gone and
/// reported alive.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_zombie(_pid: u32) -> bool {
    false
}

// --- Linux ------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn snapshot_platform() -> io::Result<Vec<ProcessNode>> {
    let mut nodes = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        // A process that exits mid-scan is not an error; it is a process that
        // exited mid-scan.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some(node) = parse_proc_stat_links(pid, &stat) {
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// Parent and group ids out of `/proc/<pid>/stat`.
///
/// The comm field is parenthesised and may itself contain spaces *and*
/// parentheses — `(sh (echo))` is a legal process name — so the split is on the
/// **last** `)`, never on whitespace. Splitting naively is the classic way this
/// parse reads the wrong columns for exactly the processes an attacker names.
#[cfg(target_os = "linux")]
fn parse_proc_stat_links(pid: u32, stat: &str) -> Option<ProcessNode> {
    let tail = &stat[stat.rfind(')')? + 1..];
    let mut fields = tail.split_whitespace();
    // After the comm field: state, ppid, pgrp.
    let _state = fields.next()?;
    let parent_pid = fields.next()?.parse().ok()?;
    let process_group_id = fields.next()?.parse().ok()?;
    Some(ProcessNode {
        pid,
        parent_pid,
        process_group_id,
    })
}

/// Field 22 of `/proc/<pid>/stat` — jiffies since boot at which this process
/// started. Fixed for its lifetime, and larger for anything that reuses the pid.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat_start_time(&stat)
}

#[cfg(target_os = "linux")]
fn parse_proc_stat_start_time(stat: &str) -> Option<u64> {
    // Same last-`)` rule as `parse_proc_stat_links`: the comm field may contain
    // both spaces and parentheses.
    let tail = &stat[stat.rfind(')')? + 1..];
    // After comm: state is 1, so starttime (field 22 overall) is the 20th here.
    tail.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn resident_bytes(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_proc_status_vm_rss_kb(&text)?.checked_mul(1_024)
}

/// `VmRSS` — current residency — rather than `VmHWM`, which is this pid's
/// lifetime peak. Summing peaks across a tree would add high-water marks the
/// members never held at the same moment and report memory that never existed.
#[cfg(target_os = "linux")]
fn parse_proc_status_vm_rss_kb(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

// --- macOS ------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn proc_bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // Safe: writes at most `size` bytes into a buffer of exactly that size, and
    // returns how many it wrote. A short write means the kernel filled a
    // different flavour's struct, which is not one we can read.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size as libc::c_int,
        )
    };
    (written == size as libc::c_int).then_some(info)
}

#[cfg(target_os = "macos")]
fn snapshot_platform() -> io::Result<Vec<ProcessNode>> {
    // Sizing call first, then the read, with headroom for processes that start
    // in between. `proc_listallpids` reports bytes when given a null buffer.
    // Safe: a null buffer asks only for the size.
    let sized = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if sized <= 0 {
        return Err(io::Error::last_os_error());
    }
    let capacity = (sized as usize / std::mem::size_of::<libc::c_int>()) + 64;
    let mut pids: Vec<libc::c_int> = vec![0; capacity];
    // Safe: writes at most `capacity` ints into a buffer of exactly that length.
    let written = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (capacity * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    if written <= 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(written as usize);

    Ok(pids
        .into_iter()
        .filter(|pid| *pid > 0)
        .filter_map(|pid| {
            let pid = u32::try_from(pid).ok()?;
            // A process that exits between the listing and this read is not an
            // error; it is a process that exited.
            let info = proc_bsd_info(pid)?;
            Some(ProcessNode {
                pid,
                parent_pid: info.pbi_ppid,
                process_group_id: info.pbi_pgid,
            })
        })
        .collect())
}

/// `ri_phys_footprint` — what this process is holding *now*.
///
/// Not `ri_lifetime_max_phys_footprint`, which `process_usage` reads for the
/// per-pid ledger: that is a lifetime peak, and summing peaks across members
/// reports a total the tree never held at once.
#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> Option<u64> {
    let info = proc_bsd_info(pid)?;
    // Microsecond resolution, so two processes reusing a pid inside the same
    // second still differ.
    Some(info.pbi_start_tvsec.saturating_mul(1_000_000) + info.pbi_start_tvusec)
}

#[cfg(target_os = "macos")]
fn resident_bytes(pid: u32) -> Option<u64> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // Safe: `proc_pid_rusage` writes the V4 struct into a buffer of that type.
    let status = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            std::ptr::addr_of_mut!(info).cast(),
        )
    };
    (status == 0).then_some(info.ri_phys_footprint)
}

// --- Windows ----------------------------------------------------------------

#[cfg(windows)]
fn snapshot_platform() -> io::Result<Vec<ProcessNode>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // Safe: creates a snapshot handle or returns the sentinel.
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut nodes = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // Safe: `entry` is sized as the API requires and lives for the whole walk.
    let mut ok = unsafe { Process32FirstW(handle, &mut entry) } != 0;
    while ok {
        nodes.push(ProcessNode {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            // Windows has no process group. Zero means "no group", which
            // `tree_members` reads as "parent links only" rather than as a group
            // that would select every ungrouped process.
            process_group_id: 0,
        });
        ok = unsafe { Process32NextW(handle, &mut entry) } != 0;
    }
    // Safe: closes the handle this function opened, exactly once.
    unsafe { CloseHandle(handle) };
    Ok(nodes)
}

/// The process creation FILETIME, which Windows keeps fixed for the process's
/// life and which a pid reuse cannot reproduce.
#[cfg(windows)]
fn process_start_time(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // Safe: opens a query-only handle, or null on refusal.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    const EMPTY: FILETIME = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // Four distinct outputs rather than one reused four times: `GetProcessTimes`
    // writes through every pointer it is given, so aliasing them is a borrow
    // error — and would be three writes to the same slot if it compiled. Only
    // the creation time is read; the other three exist because the API has no
    // way to decline them.
    let mut created = EMPTY;
    let (mut exited, mut kernel, mut user) = (EMPTY, EMPTY, EMPTY);
    // Safe: writes four FILETIMEs this stack owns, through the handle above.
    let ok =
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } != 0;
    // Safe: closes the handle this function opened, exactly once.
    unsafe { CloseHandle(handle) };
    ok.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

#[cfg(windows)]
fn resident_bytes(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // Safe: opens a query-only handle, or null on refusal.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // Safe: writes the sized struct through the handle opened above.
    let ok = unsafe {
        GetProcessMemoryInfo(
            handle,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    } != 0;
    // Safe: closes the handle this function opened, exactly once.
    unsafe { CloseHandle(handle) };
    ok.then(|| counters.WorkingSetSize as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn snapshot_platform() -> io::Result<Vec<ProcessNode>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no process-table enumeration is implemented for this platform",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn resident_bytes(_pid: u32) -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(pid: u32, parent_pid: u32, process_group_id: u32) -> ProcessNode {
        ProcessNode {
            pid,
            parent_pid,
            process_group_id,
        }
    }

    #[test]
    fn a_grandchild_is_a_member_of_the_tree() {
        let nodes = [node(10, 1, 10), node(11, 10, 10), node(12, 11, 10)];
        let members = tree_members(&nodes, 10);
        assert!(
            members.contains(&12),
            "the grandchild is the process actually holding the memory: {members:?}"
        );
        assert_eq!(members.len(), 3);
    }

    /// The escape the daemon's group-only measurement misses: a child that
    /// `setsid`s keeps its parent link, so the closure still holds it.
    #[test]
    fn a_child_that_leaves_the_process_group_is_still_held_by_its_parent_link() {
        let nodes = [node(10, 1, 10), node(11, 10, 11), node(12, 11, 11)];
        let members = tree_members(&nodes, 10);
        assert!(
            members.contains(&11) && members.contains(&12),
            "{members:?}"
        );
    }

    /// And the mirror: a child re-parented to init keeps the group, so the group
    /// union still holds it.
    #[test]
    fn a_reparented_child_is_still_held_by_its_process_group() {
        let nodes = [node(10, 1, 10), node(11, 1, 10)];
        let members = tree_members(&nodes, 10);
        assert!(
            members.contains(&11),
            "a daemonised child keeps the group even after losing the parent link: {members:?}"
        );
    }

    #[test]
    fn an_unrelated_process_is_not_a_member() {
        let nodes = [node(10, 1, 10), node(11, 10, 10), node(500, 1, 500)];
        let members = tree_members(&nodes, 10);
        assert!(
            !members.contains(&500),
            "a limit must not count the user's own processes: {members:?}"
        );
    }

    /// Pid reuse can produce a reported cycle, and Windows reports pid 0 as its
    /// own parent. Either would hang a recursive walk.
    #[test]
    fn a_parent_cycle_terminates_instead_of_hanging_the_walk() {
        let nodes = [node(10, 11, 0), node(11, 10, 0)];
        let members = tree_members(&nodes, 10);
        assert_eq!(members, BTreeSet::from([10, 11]));
    }

    #[test]
    fn a_self_parented_process_does_not_loop() {
        let nodes = [node(10, 10, 0)];
        assert_eq!(tree_members(&nodes, 10), BTreeSet::from([10]));
    }

    /// A group id of zero means "no group". Reading it as a real group would
    /// select every ungrouped process on the host, which on Windows is all of
    /// them — a memory limit that fires on the first sample, always.
    #[test]
    fn a_zero_process_group_does_not_union_every_ungrouped_process() {
        let nodes = [node(10, 1, 0), node(500, 1, 0), node(501, 1, 0)];
        assert_eq!(tree_members(&nodes, 10), BTreeSet::from([10]));
    }

    #[test]
    fn pid_zero_is_never_a_root() {
        let nodes = [node(0, 0, 0), node(10, 0, 0)];
        assert!(tree_members(&nodes, 0).is_empty());
    }

    #[test]
    fn a_root_that_is_gone_measures_as_nothing_rather_than_as_zero() {
        let nodes = [node(10, 1, 10)];
        assert!(
            measure_tree_in(&nodes, 999).is_none(),
            "zero bytes across zero processes is a budget satisfied forever"
        );
    }

    #[test]
    fn the_host_process_table_can_be_read_and_contains_this_process() {
        let nodes = snapshot().expect("the host must let a process read the process table");
        let me = std::process::id();
        assert!(
            nodes.iter().any(|node| node.pid == me),
            "the enumeration missed the process doing the enumerating"
        );
    }

    #[test]
    fn this_process_measures_as_a_live_tree_holding_something() {
        let usage = measure_tree(std::process::id())
            .expect("snapshot")
            .expect("the measuring process is alive");
        assert!(usage.process_count >= 1);
        assert!(
            usage.rss_bytes.is_some_and(|bytes| bytes > 0),
            "a running test binary holds resident memory: {usage:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_comm_field_containing_spaces_and_parentheses_does_not_shift_the_columns() {
        let stat = "42 (sh (echo)) S 7 9 9 0 -1 4194304 100 0 0 0 1 2 3 4";
        let node = parse_proc_stat_links(42, stat).expect("parsed");
        assert_eq!(node.parent_pid, 7, "split on the last ')', not the first");
        assert_eq!(node.process_group_id, 9);
    }

    #[test]
    fn this_process_has_a_stable_identity_that_reads_as_alive() {
        let me = ProcessIdentity::of(std::process::id())
            .expect("the host must report this process's start time");
        assert!(me.is_still_alive());
    }

    /// The reuse case, which is the whole reason identity is not a bare pid: a
    /// recorded start time that does not match what the pid reports now must
    /// read as dead, not as "still running".
    #[test]
    fn an_identity_whose_start_time_no_longer_matches_reads_as_gone() {
        let mut stale = ProcessIdentity::of(std::process::id()).expect("identity");
        stale.start_time = stale.start_time.wrapping_add(1);
        assert!(
            !stale.is_still_alive(),
            "a pid that has been reused must not read as the process we recorded"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_start_time_column_is_read_past_a_parenthesised_comm() {
        // Twenty fields after the comm; the twentieth is starttime.
        let stat = format!("42 (sh (echo)) S 7 9 9 {} 4242", "0 ".repeat(15));
        assert_eq!(parse_proc_stat_start_time(&stat), Some(4242));
    }

    #[test]
    fn this_process_reads_as_running_rather_than_merely_existing() {
        let me = ProcessIdentity::of(std::process::id()).expect("identity");
        assert!(me.is_running());
    }

    /// A killed-but-unreaped child still has an entry and a start time, so a
    /// termination pass that counted it as a survivor would never conclude.
    #[cfg(unix)]
    #[test]
    fn a_killed_but_unreaped_child_is_alive_and_not_running() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep spawns");
        let identity = ProcessIdentity::of(child.id()).expect("the child has an identity");
        child.kill().expect("kill");
        // Deliberately not reaped yet: this is the zombie window.
        for _ in 0..200 {
            if !identity.is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !identity.is_running(),
            "a zombie holds nothing and cannot fork; counting it as a survivor makes every \
             termination burn its whole pass budget and then report a failure to reclaim a tree \
             it did reclaim"
        );
        // Linux keeps the `/proc` entry and its start time for an unreaped
        // process, which is exactly what makes this case a trap there. Darwin
        // stops answering `PROC_PIDTBSDINFO` for a zombie, so liveness already
        // reads as gone and only the assertion above is meaningful — asserting
        // the same thing on both would be asserting the platform, not the rule.
        #[cfg(target_os = "linux")]
        assert!(
            identity.is_still_alive(),
            "an unreaped process still exists on Linux"
        );
        child.wait().expect("reaped");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_state_column_is_read_past_a_parenthesised_comm() {
        assert!(parse_proc_stat_is_zombie(
            "42 (sh (echo)) Z 7 9 9 0 -1 4194304"
        ));
        assert!(!parse_proc_stat_is_zombie(
            "42 (sh (echo)) S 7 9 9 0 -1 4194304"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn vm_rss_is_read_rather_than_vm_hwm() {
        let status = "Name:\tsh\nVmHWM:\t 900 kB\nVmRSS:\t 400 kB\n";
        assert_eq!(
            parse_proc_status_vm_rss_kb(status),
            Some(400),
            "summing lifetime peaks across a tree reports memory that never existed at once"
        );
    }
}
