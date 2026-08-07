//! Kernel-enforced confinement for [`crate::sandbox`] runs on Linux.
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
//! [`crate::sandbox::build_seatbelt_profile`] is deny-by-default with writes
//! confined to the sandbox root and reads confined to the sandbox root plus
//! explicit system/toolchain roots. Landlock is deny-by-default for every access
//! right the ruleset *handles*, so the mirror is direct:
//!
//! * read+write ([`AccessFs::from_all`]) beneath the sandbox root — which is
//!   what covers the workspace copy, `SANDBOX_HOME_DIR` and `SANDBOX_TMP_DIR`,
//!   since both live inside it;
//! * read+execute ([`AccessFs::from_read`]: `Execute | ReadFile | ReadDir`) on
//!   the roots `crate::sandbox`'s `readable_roots` computed for
//!   `LINUX_SYSTEM_READ_ROOTS`, which that function has already filtered
//!   against the real workspace and the user's home;
//! * nothing anywhere else, so the real workspace is unreachable by absolute
//!   path even though the process runs as the same uid.
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
//!
//! # The denied syscall set
//!
//! Deliberately one and a half syscalls, not a class list. Landlock already
//! confines the filesystem better than a syscall filter can — it understands
//! paths, so it does not have to guess from an `openat` argument it cannot
//! dereference — and every syscall a filter denies on top of that is a real
//! build command that stops working. The one thing Landlock cannot express
//! (below ABI v4, which is most kernels in the field) is *network denial*, which
//! the Seatbelt profile gets from `(deny network*)`. So that is the entire
//! filter, and it is installed only when `allow_network` is false:
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
//! Everything else is allowed, including `fork`/`execve` (the sandbox
//! deliberately lets a command spawn a build), all file I/O (Landlock's job),
//! `setrlimit` (`os_limits`' job), and `socket(AF_UNIX, …)`.
//!
//! `AF_UNIX` is the one place this is knowingly weaker than Seatbelt, whose
//! `(deny network*)` also covers unix sockets. Denying it on Linux would break
//! ordinary local IPC that has nothing to do with egress — Python's
//! `multiprocessing`, `syslog`, NSS lookups — so a sandboxed command can still
//! reach an abstract or pathname unix socket belonging to a host daemon. (ABI v9
//! adds `ResolveUnix`, which closes the pathname half of that gap for the tiny
//! set of kernels that have it; the abstract namespace is not a filesystem and
//! stays reachable.)
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
//! program. The `pre_exec` closure does nothing but two syscall pairs —
//! `prctl(PR_SET_NO_NEW_PRIVS)` + `seccomp(2)`, then
//! `landlock_restrict_self(2)` over the inherited descriptor. That is the same
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
    sandbox_root: &Path,
    readable_roots: &[PathBuf],
) -> io::Result<Option<RulesetCreated>> {
    let Ok(baseline) = baseline_ruleset() else {
        return Ok(None);
    };
    let ruleset = baseline
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(NEWEST_KNOWN_ABI))
        .map_err(to_io)?
        .create()
        .map_err(to_io)?
        .add_rules(path_beneath_rules(
            [sandbox_root],
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
        .map_err(to_io)?;
    Ok(Some(ruleset))
}

/// The compiled BPF program described under "the denied syscall set" above.
///
/// `pub` so the parity tests can assert it compiles on the CI kernel without
/// spawning anything.
pub fn network_denial_filter() -> io::Result<BpfProgram> {
    // Errors on an architecture seccompiler has no `AUDIT_ARCH_*` for. The
    // caller turns that into a failed run rather than a network-allowed one:
    // `allow_network: false` is a promise, and the Seatbelt path fails the same
    // way if it cannot write its profile.
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(to_io)?;
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
    let rules = BTreeMap::from([
        (
            libc::SYS_socket as i64,
            vec![
                deny_domain(libc::AF_INET)?,
                deny_domain(libc::AF_INET6)?,
                deny_domain(libc::AF_PACKET)?,
            ],
        ),
        // An empty rule vector is seccompiler's "match on the syscall number
        // alone", i.e. deny every call regardless of arguments.
        (libc::SYS_io_uring_setup as i64, Vec::new()),
    ]);
    let filter = SeccompFilter::new(
        rules,
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
pub fn confine(
    command: &mut tokio::process::Command,
    sandbox_root: &Path,
    readable_roots: &[PathBuf],
    allow_network: bool,
) -> io::Result<bool> {
    let mut ruleset = build_ruleset(sandbox_root, readable_roots)?;
    let filter = match allow_network {
        true => None,
        false => Some(network_denial_filter()?),
    };
    let filesystem_enforced = ruleset.is_some();
    if ruleset.is_none() && filter.is_none() {
        return Ok(false);
    }

    // Safe: the closure runs in the forked child and is async-signal-safe — it
    // allocates nothing, locks nothing, and shares nothing with the parent
    // beyond the moved-in ruleset descriptor and BPF program. See "why the split
    // between parent and child" above.
    unsafe {
        command.pre_exec(move || {
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
    Ok(filesystem_enforced)
}
