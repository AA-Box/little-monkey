//! Kernel-enforced ceilings applied to a child *before* it execs.
//!
//! Everything bounding a spawned process in this app until now was cooperative:
//! a wall-clock timeout that drops the future and terminates the process group
//! ([`crate::os_signal::terminate_process_group`]), and the daemon's sampling
//! watchdog. Both need this app to stay alive and keep checking. `setrlimit` is
//! the opposite — the kernel enforces it whether or not any supervisor is left.
//!
//! # What this deliberately does not bound, and why
//!
//! `setrlimit` covers far less of K4's acceptance list than the resource names
//! suggest, so the exclusions are documented here rather than discovered later:
//!
//! - **`RLIMIT_CPU`** would be nearly redundant and is dangerous set naively.
//!   Shell tools already carry a 120 s wall-clock timeout that ends the whole
//!   process group, so the escape CPU-time would close is already closed. Worse,
//!   CPU-seconds accumulate *per core*: a 120 s CPU cap kills `cargo build -j8`
//!   after roughly 15 s of wall time. An honest cap is `wall x cores x headroom`,
//!   which bounds almost nothing the timeout does not.
//! - **`RLIMIT_NPROC` is per real uid, not per process tree.** A fixed low value
//!   counts every process the login user already has — this app, their browser —
//!   so it fails spuriously on a busy desktop. A tool child that cannot fork
//!   because the user opened Chrome is a worse bug than an unbounded fork. Fork
//!   bombs need the cgroup `pids` controller or a Windows job object.
//! - **`RLIMIT_RSS` is a no-op on Darwin** and advisory on Linux. Resident memory
//!   stays with the daemon's watchdog, which measures the whole process group.
//! - **`RLIMIT_AS` bounds virtual address space, not resident memory.** Go, the
//!   JVM, sanitizers and thread stacks reserve enormous ranges, so an AS cap
//!   either kills healthy processes or is set high enough to bound nothing.
//!
//! What is left is genuinely useful but narrow, which is why [`ChildLimits`]
//! exposes three knobs and not six.

/// Bounds the kernel will hold a child to, set between `fork` and `exec` so they
/// are already in force when the target program starts and are inherited by
/// everything it spawns.
///
/// `Copy` on purpose: the value is moved into a `pre_exec` closure, which may not
/// allocate or lock, so it has to be reproducible from bytes alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildLimits {
    /// Largest single file the child may create (`RLIMIT_FSIZE`). Exceeding it
    /// raises `SIGXFSZ`, which kills by default.
    ///
    /// A per-file cap, not a disk quota: it stops one runaway file, not a million
    /// small ones. Real disk exhaustion needs a quota or the cgroup `io`
    /// controller.
    pub max_file_bytes: Option<u64>,
    /// Open descriptors (`RLIMIT_NOFILE`). Per-process, so this bounds the child
    /// rather than protecting the app's own table.
    pub max_open_files: Option<u64>,
    /// Forbid core dumps (`RLIMIT_CORE` = 0).
    ///
    /// The one unambiguous win here: nothing legitimate wants a core dump from a
    /// tool child, a crashing build can drop gigabytes of them into the
    /// workspace, and there is no value that breaks working code.
    pub deny_core_dumps: bool,
}

impl ChildLimits {
    /// Core dumps refused and nothing else bounded.
    ///
    /// The floor every spawn can take without a judgement call about what the
    /// child is for.
    #[must_use]
    pub const fn baseline() -> Self {
        ChildLimits {
            max_file_bytes: None,
            max_open_files: None,
            deny_core_dumps: true,
        }
    }

    #[must_use]
    pub const fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = Some(bytes);
        self
    }

    #[must_use]
    pub const fn with_max_open_files(mut self, files: u64) -> Self {
        self.max_open_files = Some(files);
        self
    }
}

impl Default for ChildLimits {
    fn default() -> Self {
        Self::baseline()
    }
}

/// `RLIMIT_*` constants are `__rlimit_resource_t` on glibc Linux and `c_int`
/// everywhere else this ships, so the helper below cannot name one type.
#[cfg(all(unix, target_env = "gnu", target_os = "linux"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(all(target_env = "gnu", target_os = "linux"))))]
type Resource = libc::c_int;

/// Choose the value to install given what the process already inherited.
///
/// Split out as a pure function because it carries the one rule that is easy to
/// get wrong and impossible to observe from a passing spawn: an unprivileged
/// process **cannot raise a hard limit**, so requesting more than it inherited
/// fails with `EPERM` and takes the whole spawn down instead of bounding
/// anything. A tightening is always allowed; a loosening is silently declined in
/// favour of the stricter inherited ceiling.
#[cfg(unix)]
fn resolve_target(requested: u64, inherited_hard: libc::rlim_t) -> libc::rlim_t {
    let requested = libc::rlim_t::try_from(requested).unwrap_or(libc::rlim_t::MAX);
    if inherited_hard == libc::RLIM_INFINITY {
        // No ceiling inherited, so anything finite is a tightening.
        return requested;
    }
    requested.min(inherited_hard)
}

/// Set one resource's soft *and* hard limit to the same value.
///
/// Both, because a hard limit is a ceiling the child cannot lift: leaving the
/// hard limit alone would let the child restore its own headroom with a
/// `setrlimit` of its own, which makes the bound advice rather than enforcement.
///
/// # Safety-relevant constraints
///
/// Called from `pre_exec`, so it must be async-signal-safe: no allocation, no
/// locks, no shared state. `getrlimit`/`setrlimit` are syscalls over a stack
/// struct, and `io::Error::last_os_error` stores the errno inline without
/// allocating.
#[cfg(unix)]
fn set_limit(resource: Resource, value: u64) -> std::io::Result<()> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // Safe: writes only into the stack struct above, which this call owns.
    if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let target = resolve_target(value, current.rlim_max);
    let limit = libc::rlimit {
        rlim_cur: target,
        rlim_max: target,
    };
    // Safe: reads only the stack struct above.
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Install `limits` on `command`'s child.
///
/// The child is bounded from its first instruction: `pre_exec` runs after `fork`
/// and before `exec`, so the target program never executes unbounded, and the
/// limits are inherited by everything it spawns in turn — which is what makes
/// this reach the grandchildren a supervisor cannot see.
///
/// A failure inside the closure fails the spawn rather than silently producing an
/// unbounded child. That is the right direction: a tool that will not start is
/// visible, and a tool that quietly lost its bounds is not.
#[cfg(unix)]
pub fn apply(limits: ChildLimits, command: &mut tokio::process::Command) {
    // `pre_exec` is an inherent method on `tokio::process::Command` under
    // `cfg(unix)`, so std's `CommandExt` is not imported here.
    //
    // Safe: the closure runs in the forked child and touches nothing but
    // `getrlimit`/`setrlimit` over its own stack. `ChildLimits` is `Copy`, so the
    // closure captures plain integers — no allocation, no lock, and no state
    // shared with the parent, which is what `pre_exec`'s async-signal-safety
    // requirement demands.
    unsafe {
        command.pre_exec(move || {
            if limits.deny_core_dumps {
                set_limit(libc::RLIMIT_CORE, 0)?;
            }
            if let Some(bytes) = limits.max_file_bytes {
                set_limit(libc::RLIMIT_FSIZE, bytes)?;
            }
            if let Some(files) = limits.max_open_files {
                set_limit(libc::RLIMIT_NOFILE, files)?;
            }
            Ok(())
        });
    }
}

/// No-op on Windows, which has no `setrlimit`.
///
/// The Windows equivalent is a **job object** (`SetInformationJobObject` with
/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`), which bounds committed memory, CPU
/// time and process count for a whole tree and would cover more of K4 than
/// `setrlimit` does on unix. It is not built. This is a deliberate no-op rather
/// than an error so the call sites read the same on every platform, and the gap
/// is recorded in the roadmap instead of being hidden behind a silent success.
#[cfg(windows)]
pub fn apply(_limits: ChildLimits, _command: &mut tokio::process::Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_refuses_core_dumps_and_bounds_nothing_else() {
        let limits = ChildLimits::baseline();
        assert!(limits.deny_core_dumps);
        assert_eq!(limits.max_file_bytes, None);
        assert_eq!(limits.max_open_files, None);
        assert_eq!(ChildLimits::default(), limits);

        let bounded = limits.with_max_file_bytes(4_096).with_max_open_files(64);
        assert_eq!(bounded.max_file_bytes, Some(4_096));
        assert_eq!(bounded.max_open_files, Some(64));
        assert!(bounded.deny_core_dumps, "the floor survives a tightening");
    }

    /// The rule that cannot be seen from a passing spawn: asking for more than
    /// was inherited is `EPERM`, which fails the spawn instead of bounding it.
    #[cfg(unix)]
    #[test]
    fn a_requested_limit_is_never_raised_above_the_inherited_hard_limit() {
        // Tightening: allowed, taken as asked.
        assert_eq!(resolve_target(1_024, 4_096), 1_024);
        // Loosening: declined in favour of the stricter inherited ceiling.
        assert_eq!(resolve_target(8_192, 4_096), 4_096);
        // Equal is a no-op, not an error.
        assert_eq!(resolve_target(4_096, 4_096), 4_096);
        // No inherited ceiling: anything finite is a tightening.
        assert_eq!(resolve_target(4_096, libc::RLIM_INFINITY), 4_096);
        // Zero is a real bound (this is how core dumps are refused), not
        // "unset" — it must survive both branches.
        assert_eq!(resolve_target(0, libc::RLIM_INFINITY), 0);
        assert_eq!(resolve_target(0, 4_096), 0);
    }

    /// End to end against the kernel: the point of this module is enforcement
    /// without a supervisor, so the test writes past the cap with nothing
    /// watching and asserts the kernel killed the writer.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_kernel_kills_a_child_that_writes_past_its_file_size_limit() {
        let dir =
            std::env::temp_dir().join(format!("little-monkey-rlimit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            // 64 KiB written in 1 KiB chunks, against a 4 KiB ceiling. `dd`
            // rather than a shell redirect so the write is unmistakably the
            // child's own.
            .arg("dd if=/dev/zero of=big bs=1024 count=64 2>/dev/null")
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        apply(
            ChildLimits::baseline().with_max_file_bytes(4_096),
            &mut command,
        );

        let status = command.status().await.expect("the child must spawn");
        assert!(
            !status.success(),
            "a child that blew its file-size limit must not report success"
        );

        let written = std::fs::metadata(dir.join("big"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert!(
            written <= 4_096,
            "the kernel let {written} bytes through a 4096 byte ceiling"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The counterpart: the same command inside its budget must run untouched.
    /// Without this, a limit set so low that everything dies would pass the test
    /// above.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_inside_its_limits_is_left_alone() {
        let dir =
            std::env::temp_dir().join(format!("little-monkey-rlimit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("dd if=/dev/zero of=small bs=1024 count=2 2>/dev/null")
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        apply(
            ChildLimits::baseline().with_max_file_bytes(4_096),
            &mut command,
        );

        let status = command.status().await.expect("the child must spawn");
        assert!(
            status.success(),
            "a 2 KiB write under a 4 KiB ceiling must succeed"
        );
        assert_eq!(
            std::fs::metadata(dir.join("small")).unwrap().len(),
            2_048,
            "the whole file must be written"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Prove `pre_exec` actually ran, by asking the child what it inherited.
    ///
    /// This asserts on the **file-size** limit rather than the core limit, and the
    /// difference matters: macOS already defaults `ulimit -c` to 0, so a
    /// core-dump assertion passes whether or not this module does anything —
    /// verified by deleting the `apply` call and watching it still pass. `-f`
    /// defaults to `unlimited` on every platform this ships on, so it can only
    /// read as a number if the limit was installed.
    ///
    /// Compared against the string `unlimited` rather than a block count on
    /// purpose: `ulimit -f` reports 512-byte blocks per POSIX, and pinning the
    /// arithmetic would trade a real assertion for a shell-portability question.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_installed_limits_are_what_the_child_inherits() {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("ulimit -f; ulimit -c")
            .stdin(std::process::Stdio::null());
        apply(
            ChildLimits::baseline().with_max_file_bytes(8_192),
            &mut command,
        );

        let output = command.output().await.expect("the child must spawn");
        let reported = String::from_utf8_lossy(&output.stdout);
        let mut lines = reported.lines();
        let file_size = lines.next().unwrap_or_default().trim();
        let core = lines.next().unwrap_or_default().trim();

        assert_ne!(
            file_size, "unlimited",
            "the child inherited no file-size ceiling, so pre_exec did not run"
        );
        assert!(
            file_size.parse::<u64>().is_ok_and(|blocks| blocks > 0),
            "expected a finite block count, got {file_size:?}"
        );
        assert_eq!(core, "0", "core dumps must be refused");
    }
}
