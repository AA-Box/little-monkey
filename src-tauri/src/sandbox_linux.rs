//! Kernel-enforced confinement for sandbox and live-workspace shell runs on Linux.
//!
//! The Linux half of the isolation parity work: a Landlock filesystem ruleset
//! plus (when the run is not allowed network access) a seccomp-BPF filter, both
//! installed in `pre_exec` so they are inherited across the `exec` and by
//! everything the sandboxed command spawns in turn. This is the same mechanism
//! [`crate::os_limits`] uses for `setrlimit`, and it composes with it rather
//! than replacing it: nothing here denies `setrlimit`, and this module's
//! `pre_exec` hook is registered *after* `os_limits`' so the bounds are in place
//! before any restriction can interfere with installing them.
//!
//! # What the ruleset grants, and why it looks like the Seatbelt profile
//!
//! [`crate::sandbox::build_seatbelt_profile`] is deny-by-default with explicit
//! writable and read-only roots. Landlock is deny-by-default for every access
//! right the ruleset *handles*, so the mirror is direct:
//!
//! * read+write ([`AccessFs::from_all`]) beneath each writable root — one
//!   disposable sandbox root, or a selected live workspace plus its private
//!   HOME/TMP runtime root;
//! * read+execute ([`AccessFs::from_read`]: `Execute | ReadFile | ReadDir`) on
//!   the roots `crate::sandbox`'s `readable_roots` computed for
//!   `LINUX_SYSTEM_READ_ROOTS`, which that function has already filtered
//!   against the real workspace and the user's home;
//! * nothing anywhere else, so a disposable run cannot reach the real workspace
//!   and a live shell cannot reach files outside its selected workspace.
//! `/dev/null` is the one writable system object: shell redirection needs it and
//! discarding bytes grants no host data or IPC authority.
//!
//! Two consequences of "mirror the Seatbelt policy, do not widen it" are worth
//! stating because they surprise people:
//!
//! * `/dev/null` is granted **read** only, exactly as the Seatbelt profile
//!   grants it, so `cmd > /dev/null` fails inside the sandbox. That is existing
//!   macOS behaviour, not a new Linux quirk — the sandbox-owned `TMPDIR` is the
//!   place to throw output away.
//! * `/proc` and `/sys` are granted **nothing**. The Seatbelt profile has no
//!   equivalent grant (macOS exposes that class of information through
//!   `sysctl-read`/`mach-lookup` instead of a filesystem tree), and granting
//!   read on `/proc` would hand the sandboxed command `/proc/<pid>/environ` for
//!   every same-uid process — including this app's own, which is precisely what
//!   [`crate::sandbox`]'s `env_clear()` exists to prevent. Commands that need
//!   `/proc/self` are the known cost of that choice.
//!
//! # ABI degradation
//!
//! Landlock access rights arrived over several kernel releases (`Refer` in ABI
//! v2, `Truncate` in v3, `IoctlDev` in v5, `ResolveUnix` in v9), and a ruleset
//! that names a right the running kernel does not know is rejected outright.
//! The `landlock` crate's compatibility engine exists for exactly this, and this
//! module uses both of its modes deliberately:
//!
//! * The ABI v1 rights are requested under [`CompatLevel::HardRequirement`]. If
//!   *that* fails the kernel has no usable Landlock at all (not built in, not
//!   enabled at boot, or the syscall is blocked by an outer sandbox such as a
//!   container's own seccomp policy) and `build_ruleset` answers `None` — the
//!   run then degrades to the restricted-cwd/env isolation and reports
//!   [`crate::sandbox::Isolation::ProcessOnly`]. It never silently claims a
//!   boundary it does not have.
//! * Everything above v1 is requested under [`CompatLevel::BestEffort`], so on
//!   an older kernel the unsupported rights are dropped instead of failing the
//!   ruleset. The confinement that remains is real but narrower — a v1 kernel
//!   cannot deny `truncate(2)` or an `ioctl` on a device file, because it has no
//!   right for either.
//!
//! One narrowing runs the other way and is a *functional* cost rather than a
//! security one: on an ABI v1 kernel (5.13–5.18) `Refer` does not exist, and the
//! kernel's answer to that is to deny linking or renaming across directories
//! unconditionally while any Landlock domain is in force. A build that renames a
//! temporary file into a sibling directory inside the sandbox will fail there.
//! Nothing in userspace can fix it; ABI v2 (5.19) is the fix.
//! The disposable-copy sandbox keeps that reported compatibility behavior.
//! Live-workspace shells instead require ABI v3 (Linux 6.2), because v3 is the
//! first version that can deny `truncate(2)` outside the selected workspace;
//! an older kernel fails the tool call rather than advertising a porous live
//! boundary.
//!
//! # The denied syscall sets
//!
//! The disposable-copy sandbox deliberately keeps its filter to one and a half
//! syscalls. Landlock already confines filesystem paths better than a syscall
//! filter can, and each extra denial is a real compatibility cost:
//!
//! * `socket(2)` with `AF_INET`, `AF_INET6` or `AF_PACKET` → `EACCES`. Denying
//!   socket *creation* rather than `connect`/`sendto`/`bind` is one choke point
//!   instead of five, and it cannot be reached around: without a descriptor
//!   there is nothing to connect. It denies loopback too, which matters —
//!   `127.0.0.1` is the stricter test of a boundary than a public address is,
//!   because "the connection failed" is also what a machine with no egress
//!   produces, and it is what the parity test asserts against.
//! * `io_uring_setup(2)` → `EACCES`, unconditionally. `IORING_OP_SOCKET` would
//!   otherwise create an `AF_INET` socket without ever entering `socket(2)`,
//!   which is a documented way around filters of this shape. No build or test
//!   command needs io_uring, and glibc does not use it.
//!
//! Everything else in the disposable path remains allowed, including
//! `fork`/`execve`, all file I/O (Landlock's job), and `setrlimit`
//! (`os_limits`' job).
//!
//! A live-workspace agent shell has more to protect because it runs beside the
//! long-lived app. Its strict filter denies every `socket(2)` domain, because an
//! abstract socket or Docker-compatible daemon is an authority escape, and also
//! denies `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd` and
//! `kcmp`: each can inspect or copy authority from another same-uid process
//! without opening a Landlock-visible path. Cross-process signal syscalls are
//! denied too: the shell knows its host parent's pid and must not be able to
//! stop or kill the desktop. `add_key`, `request_key` and `keyctl` are denied
//! for the same reason for session keyrings. `socketpair(2)` and ordinary child
//! process creation remain available. `setsid` and `setpgid` are denied so
//! descendants cannot leave the process group the tool lifecycle owns.
//!
//! The strict `pre_exec` also calls `close_range(3, UINT_MAX,
//! CLOSE_RANGE_CLOEXEC)`. This preserves stdin/stdout/stderr and keeps the
//! Landlock descriptor live long enough to install the ruleset, while ensuring
//! every other descriptor inherited from another app thread closes at `exec`.
//! There is no fallback scan: usable Landlock starts on kernels newer than
//! `CLOSE_RANGE_CLOEXEC`, and an outer policy that blocks the syscall must fail
//! the shell rather than leak an unknown descriptor set. The disposable path is
//! unchanged.
//!
//! Note also that seccompiler's filters begin with an architecture check that
//! answers `SECCOMP_RET_KILL_PROCESS` on a mismatch: a network-denied run that
//! `exec`s a foreign-architecture binary (a 32-bit x86 tool on an x86_64 host)
//! is killed rather than refused. `seccompiler` over a hand-rolled filter
//! because the BPF assembly for an argument comparison is exactly the kind of
//! code that must not be written blind, and this is a security boundary.
//!
//! # Why the split between parent and child
//!
//! Everything that allocates, opens a descriptor or resolves a path happens in
//! the parent, before `fork`: `build_ruleset` creates the ruleset descriptor
//! and adds every path rule, and [`network_denial_filter`] compiles the BPF
//! program. The strict `pre_exec` closure first marks inherited descriptors
//! close-on-exec, then runs the same two syscall pairs as the disposable path —
//! `prctl(PR_SET_NO_NEW_PRIVS)` + `seccomp(2)`, then
//! `landlock_restrict_self(2)` over the ruleset descriptor. That is the same
//! async-signal-safety constraint `os_limits::install` documents: after `fork`
//! in a multithreaded process, allocating can deadlock on a lock another thread
//! held at fork time, so the child side allocates nothing — not even to format
//! an error, which is why failures map through
//! [`std::io::Error::last_os_error`] (both crates' failures here *are* failed
//! syscalls, so errno is the accurate report) instead of the error's `Display`.
//!
//! A failure in the child fails the spawn rather than producing an unconfined
//! child, matching `os_limits`' reasoning: a tool that will not start is
//! visible, and a tool that quietly lost its boundary is not.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetError, ABI,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The newest ABI this crate knows how to name rights for. Requested
/// best-effort, so it is a ceiling rather than a requirement: naming it is how
/// a newer kernel's stricter rights get handled at all, and the compatibility
/// engine drops the ones the running kernel lacks.
const NEWEST_KNOWN_ABI: ABI = ABI::V9;
const MINIMUM_LIVE_SHELL_ABI: libc::c_long = 3;

fn require_live_shell_abi() -> io::Result<()> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < MINIMUM_LIVE_SHELL_ABI {
        return Err(if abi == -1 {
            io::Error::last_os_error()
        } else {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "live agent shells require Landlock ABI 3 for truncate confinement",
            )
        });
    }
    Ok(())
}

/// Parent-side only — allocates, and never runs after `fork`.
fn to_io<E: std::fmt::Display>(error: E) -> io::Error {
    io::Error::other(error.to_string())
}

/// Whether this kernel can enforce the ABI v1 baseline.
///
/// Builder-only: it asks the compatibility engine the same question
/// `build_ruleset` asks, but stops before `create()`, so it opens no
/// descriptor, touches no path, and — importantly — never restricts the calling
/// thread. Safe to call from a probe like
/// [`crate::sandbox::sandbox_enforcement`] on any thread at any time.
pub fn landlock_is_enforceable() -> bool {
    baseline_ruleset().is_ok()
}

/// The v1 rights as a hard requirement — see the ABI-degradation notes above.
fn baseline_ruleset() -> Result<Ruleset, RulesetError> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
}

/// `Ok(None)` when this kernel has no usable Landlock, which is a degrade and
/// not an error. `Err` only for a kernel that supports Landlock and then failed
/// anyway (a descriptor limit, say), where degrading would mean reporting a
/// boundary that was never installed.
fn build_ruleset(
    writable_roots: &[PathBuf],
    readable_roots: &[PathBuf],
) -> io::Result<Option<RulesetCreated>> {
    let Ok(baseline) = baseline_ruleset() else {
        return Ok(None);
    };
    let null_device = [PathBuf::from("/dev/null")];
    let ruleset = baseline
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(NEWEST_KNOWN_ABI))
        .map_err(to_io)?
        .create()
        .map_err(to_io)?
        .add_rules(path_beneath_rules(
            writable_roots,
            AccessFs::from_all(NEWEST_KNOWN_ABI),
        ))
        .map_err(to_io)?
        // `path_beneath_rules` silently skips paths it cannot open, which is
        // what makes one list work across distributions: `/lib64`, `/etc/pki`
        // and the rest are present on some and absent on others.
        .add_rules(path_beneath_rules(
            readable_roots,
            AccessFs::from_read(NEWEST_KNOWN_ABI),
        ))
        .map_err(to_io)?
        .add_rules(path_beneath_rules(
            &null_device,
            AccessFs::WriteFile | AccessFs::Truncate | AccessFs::IoctlDev,
        ))
        .map_err(to_io)?;
    Ok(Some(ruleset))
}

/// The compiled BPF program described under "the denied syscall set" above.
///
/// `pub` so the parity tests can assert it compiles on the CI kernel without
/// spawning anything.
pub fn network_denial_filter() -> io::Result<BpfProgram> {
    denial_filter_inner(Some(false), false)
}

/// Live agent shells deny every socket domain, including AF_UNIX host daemons,
/// plus cross-process inspection and the caller's session keyrings.
pub fn strict_network_denial_filter() -> io::Result<BpfProgram> {
    denial_filter_inner(Some(true), true)
}

fn strict_process_denial_filter() -> io::Result<BpfProgram> {
    denial_filter_inner(None, true)
}

fn strict_process_syscalls() -> [i64; 16] {
    [
        libc::SYS_ptrace as i64,
        libc::SYS_process_vm_readv as i64,
        libc::SYS_process_vm_writev as i64,
        libc::SYS_pidfd_getfd as i64,
        libc::SYS_kcmp as i64,
        libc::SYS_kill as i64,
        libc::SYS_tkill as i64,
        libc::SYS_tgkill as i64,
        libc::SYS_pidfd_send_signal as i64,
        libc::SYS_rt_sigqueueinfo as i64,
        libc::SYS_rt_tgsigqueueinfo as i64,
        libc::SYS_add_key as i64,
        libc::SYS_request_key as i64,
        libc::SYS_keyctl as i64,
        libc::SYS_setpgid as i64,
        libc::SYS_setsid as i64,
    ]
}

fn denial_rules(
    socket_policy: Option<bool>,
    deny_process_introspection: bool,
) -> io::Result<BTreeMap<i64, Vec<SeccompRule>>> {
    let deny_domain = |domain: libc::c_int| -> io::Result<SeccompRule> {
        // Argument 0 of `socket(2)` is `int domain`, hence `Dword`.
        SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            domain as u64,
        )
        .map_err(to_io)?])
        .map_err(to_io)
    };
    let mut rules = BTreeMap::new();
    if let Some(deny_all_sockets) = socket_policy {
        let socket_rules = if deny_all_sockets {
            Vec::new()
        } else {
            vec![
                deny_domain(libc::AF_INET)?,
                deny_domain(libc::AF_INET6)?,
                deny_domain(libc::AF_PACKET)?,
            ]
        };
        rules.insert(libc::SYS_socket as i64, socket_rules);
        // An empty rule vector is seccompiler's "match on the syscall number
        // alone", i.e. deny every call regardless of arguments.
        rules.insert(libc::SYS_io_uring_setup as i64, Vec::new());
    }
    if deny_process_introspection {
        for syscall in strict_process_syscalls() {
            rules.insert(syscall, Vec::new());
        }
    }
    Ok(rules)
}

fn denial_filter_inner(
    socket_policy: Option<bool>,
    deny_process_introspection: bool,
) -> io::Result<BpfProgram> {
    // Errors on an architecture seccompiler has no `AUDIT_ARCH_*` for. The
    // caller turns that into a failed run rather than a partially confined one.
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(to_io)?;
    let filter = SeccompFilter::new(
        denial_rules(socket_policy, deny_process_introspection)?,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
        arch,
    )
    .map_err(to_io)?;
    let program: BpfProgram = filter.try_into().map_err(to_io)?;
    Ok(program)
}

/// Install the confinement on `command`'s child.
///
/// Returns whether the **filesystem** boundary applied, which is what the
/// caller reports as [`crate::sandbox::Isolation`]. A run that got the network
/// filter but no Landlock ruleset still answers `false`: its egress is denied by
/// the kernel, but its filesystem is not, and `ProcessOnly` is the honest
/// summary of that pair.
fn confinement_parts(
    writable_roots: &[PathBuf],
    readable_roots: &[PathBuf],
    allow_network: bool,
    deny_all_sockets: bool,
) -> io::Result<(Option<RulesetCreated>, Option<BpfProgram>, bool)> {
    if deny_all_sockets {
        require_live_shell_abi()?;
    }
    let ruleset = build_ruleset(writable_roots, readable_roots)?;
    let filter = match (allow_network, deny_all_sockets) {
        (true, true) => Some(strict_process_denial_filter()?),
        (true, false) => None,
        (false, true) => Some(strict_network_denial_filter()?),
        (false, false) => Some(network_denial_filter()?),
    };
    let filesystem_enforced = ruleset.is_some();
    Ok((ruleset, filter, filesystem_enforced))
}

fn install<C>(
    command: &mut C,
    mut ruleset: Option<RulesetCreated>,
    filter: Option<BpfProgram>,
    strict: bool,
) where
    C: CommandPreExec,
{
    if ruleset.is_none() && filter.is_none() && !strict {
        return;
    }

    // Safe: the closure runs in the forked child and is async-signal-safe — it
    // allocates nothing, locks nothing, and shares nothing with the parent
    // beyond the moved-in ruleset descriptor and BPF program. See "why the split
    // between parent and child" above.
    unsafe {
        command.install_pre_exec(move || {
            if strict {
                // Mark every non-stdio descriptor close-on-exec in one syscall.
                // The Landlock ruleset descriptor stays usable below and then
                // closes with everything else on successful exec.
                let closed = libc::syscall(
                    libc::SYS_close_range,
                    3 as libc::c_uint,
                    libc::c_uint::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                );
                if closed != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(program) = filter.as_deref() {
                seccompiler::apply_filter(program).map_err(|_| io::Error::last_os_error())?;
            }
            // `restrict_self` consumes the ruleset, so it is taken out of the
            // `Option` rather than borrowed; the closure only ever runs once per
            // spawn, and only in the child's copy of this memory.
            if let Some(ruleset) = ruleset.take() {
                ruleset
                    .restrict_self()
                    .map_err(|_| io::Error::last_os_error())?;
            }
            Ok(())
        });
    }
}

/// The small common surface both Tokio and standard-library commands expose
/// for installing a child-side confinement hook.
trait CommandPreExec {
    unsafe fn install_pre_exec<F>(&mut self, hook: F)
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static;
}

impl CommandPreExec for tokio::process::Command {
    unsafe fn install_pre_exec<F>(&mut self, hook: F)
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        self.pre_exec(hook);
    }
}

impl CommandPreExec for std::process::Command {
    unsafe fn install_pre_exec<F>(&mut self, hook: F)
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        use std::os::unix::process::CommandExt;
        self.pre_exec(hook);
    }
}

/// Install Landlock/seccomp on a Tokio child with several writable roots.
/// The disposable sandbox passes one root; a live workspace shell passes its
/// workspace plus its private HOME/TMP runtime root.
pub fn confine_roots(
    command: &mut tokio::process::Command,
    writable_roots: &[PathBuf],
    readable_roots: &[PathBuf],
    allow_network: bool,
) -> io::Result<bool> {
    let (ruleset, filter, filesystem_enforced) =
        confinement_parts(writable_roots, readable_roots, allow_network, true)?;
    install(command, ruleset, filter, true);
    Ok(filesystem_enforced)
}

/// Standard-library command counterpart used by background shell tools.
pub fn confine_std_roots(
    command: &mut std::process::Command,
    writable_roots: &[PathBuf],
    readable_roots: &[PathBuf],
    allow_network: bool,
) -> io::Result<bool> {
    let (ruleset, filter, filesystem_enforced) =
        confinement_parts(writable_roots, readable_roots, allow_network, true)?;
    install(command, ruleset, filter, true);
    Ok(filesystem_enforced)
}

pub fn confine(
    command: &mut tokio::process::Command,
    sandbox_root: &Path,
    readable_roots: &[PathBuf],
    allow_network: bool,
) -> io::Result<bool> {
    let (ruleset, filter, filesystem_enforced) = confinement_parts(
        &[sandbox_root.to_path_buf()],
        readable_roots,
        allow_network,
        false,
    )?;
    install(command, ruleset, filter, false);
    Ok(filesystem_enforced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_rules_deny_cross_process_and_keyring_authority_only_in_strict_mode() {
        let disposable = denial_rules(Some(false), false).expect("disposable rules");
        let strict = denial_rules(Some(true), true).expect("strict rules");
        let strict_with_network = denial_rules(None, true).expect("strict process-only rules");

        let syscalls = strict_process_syscalls();
        assert_eq!(
            syscalls
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            syscalls.len(),
            "strict syscall list contains an architecture-specific collision"
        );
        for syscall in syscalls {
            assert!(!disposable.contains_key(&syscall));
            assert!(strict.get(&syscall).is_some_and(Vec::is_empty));
            assert!(strict_with_network.get(&syscall).is_some_and(Vec::is_empty));
        }
        assert!(strict
            .get(&(libc::SYS_socket as i64))
            .is_some_and(Vec::is_empty));
        assert!(!strict_with_network.contains_key(&(libc::SYS_socket as i64)));
    }

    #[test]
    fn strict_pre_exec_closes_an_inheritable_non_stdio_fd_at_exec() {
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};

        struct RawFd(libc::c_int);
        impl Drop for RawFd {
            fn drop(&mut self) {
                unsafe {
                    libc::close(self.0);
                }
            }
        }

        let fixture_path = std::env::temp_dir().join(format!(
            "little-monkey-inherited-fd-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&fixture_path, b"x").expect("write fd fixture");
        let fixture = std::fs::File::open(&fixture_path).expect("open fd fixture");
        std::fs::remove_file(&fixture_path).expect("unlink fd fixture");
        let inherited = RawFd(unsafe { libc::fcntl(fixture.as_raw_fd(), libc::F_DUPFD, 3) });
        assert!(
            inherited.0 >= 3,
            "F_DUPFD failed: {}",
            io::Error::last_os_error()
        );
        let flags = unsafe { libc::fcntl(inherited.0, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "test fd was not inheritable");
        assert!(
            crate::workspace_shell::posix_spawn_inheriting_fd_probe_for_test(inherited.0)
                .expect("baseline shell"),
            "baseline shell did not inherit the test fd"
        );

        let mut strict = Command::new(std::env::current_exe().expect("current test executable"));
        strict
            .args([
                "--exact",
                "workspace_shell::tests::inherited_fd_probe_child",
                "--test-threads=1",
            ])
            .env("LITTLE_MONKEY_INHERITED_FD_PROBE", inherited.0.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        install(&mut strict, None, None, true);
        let status = strict.status().expect("strict shell");
        assert!(!status.success(), "strict shell inherited a non-stdio fd");
    }
}
