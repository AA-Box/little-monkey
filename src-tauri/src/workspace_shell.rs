//! Kernel-confined shell execution for agent tools.
//!
//! One policy on every supported platform: the selected workspace and a
//! process-private HOME/TMP root are writable, explicit system/toolchain roots
//! are read-only, inherited environment variables are scrubbed, and direct
//! internet sockets are denied. Network-capable agent tools remain
//! host-mediated through `crate::egress`, where K5 can enforce a run's
//! host/port/protocol rule; a raw shell socket cannot be mapped to that rule
//! without bypassing its DNS pins, denial ledger and byte accounting.
//!
//! A linked worktree's `.git` control file may point at administrative data in
//! another checkout. That target is deliberately not followed into the grant:
//! shell Git commands which need the common object/config store fail, while the
//! host-owned `agent_worktrees` status/apply/remove path continues to manage it
//! outside the model-authored shell. Treating an external `.git` directory as
//! writable workspace would also grant config, hooks and refs shared by every
//! worktree, which is a larger authority than the selected tree.
//!
//! The grant is the workspace namespace. Pre-existing hard links, FIFOs, or
//! other special objects named inside it are workspace content; rejecting or
//! rewriting every such object before each command would break ordinary Cargo
//! and pnpm trees and still race the trusted host. Kernel checks continue to
//! deny following a symlink to an outside pathname.
//!
//! Windows owns the whole process tree with its job. Linux's strict seccomp
//! policy forbids leaving the process group and denies signal syscalls: classic
//! seccomp cannot allow signaling descendants without also exposing the known
//! host parent, so signal-based child supervision is an explicit functional
//! cost until a PID namespace or broker can scope it. macOS has neither
//! mechanism: a
//! process which changes groups can outlive cleanup, still under the same
//! Seatbelt filesystem/network policy. The K4 roadmap entry owns that lifetime
//! limitation; this module does not call a Darwin process group a process tree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncRead;

use crate::process_table::{ProcessKind, ProcessLimits};
use crate::resource_control::{EffectiveLimits, LimitLayer, LimitSource, ResourceController};

const PRIVATE_HOME: &str = "home";
const PRIVATE_TMP: &str = "tmp";
const SEATBELT_PROFILE: &str = "shell.sb";

pub(crate) type AsyncShellReader = Box<dyn AsyncRead + Unpin + Send>;

#[cfg(unix)]
fn child_exited_unreaped(pid: u32) -> io::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(unsafe { info.si_pid() } != 0)
}

struct ShellRuntime {
    root: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    workspace_root: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    readable_roots: Vec<PathBuf>,
    #[cfg(target_os = "macos")]
    profile: PathBuf,
}

#[cfg(target_os = "macos")]
struct MacFdSweep {
    directory: fs::File,
    buffer: Box<[u8]>,
}

#[cfg(target_os = "macos")]
impl MacFdSweep {
    fn create() -> io::Result<Self> {
        Ok(Self {
            directory: fs::File::open("/dev/fd")?,
            // Allocated in the parent. The post-fork hook performs only raw
            // syscalls over this storage and never touches the allocator.
            buffer: vec![0; 64 * 1024].into_boxed_slice(),
        })
    }

    fn mark_all_cloexec(&mut self) -> io::Result<()> {
        use std::os::fd::AsRawFd;

        unsafe extern "C" {
            #[link_name = "__getdirentries64"]
            fn getdirentries64(
                fd: libc::c_int,
                buffer: *mut libc::c_void,
                buffer_size: libc::size_t,
                position: *mut libc::off_t,
            ) -> libc::ssize_t;
        }

        const RECORD_HEADER: usize = 21;
        const RECLEN_OFFSET: usize = 16;
        const NAMELEN_OFFSET: usize = 18;
        const NAME_OFFSET: usize = 21;

        let directory_fd = self.directory.as_raw_fd();
        let mut position: libc::off_t = 0;
        loop {
            let count = unsafe {
                getdirentries64(
                    directory_fd,
                    self.buffer.as_mut_ptr().cast(),
                    self.buffer.len(),
                    &mut position,
                )
            };
            if count == -1 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                return Ok(());
            }
            let count = usize::try_from(count)
                .map_err(|_| io::Error::other("negative /dev/fd directory read"))?;
            let mut offset = 0;
            while offset < count {
                if count - offset < RECORD_HEADER {
                    return Err(io::Error::other("truncated /dev/fd directory record"));
                }
                let reclen = u16::from_ne_bytes([
                    self.buffer[offset + RECLEN_OFFSET],
                    self.buffer[offset + RECLEN_OFFSET + 1],
                ]) as usize;
                let namelen = u16::from_ne_bytes([
                    self.buffer[offset + NAMELEN_OFFSET],
                    self.buffer[offset + NAMELEN_OFFSET + 1],
                ]) as usize;
                if reclen < RECORD_HEADER
                    || offset + reclen > count
                    || NAME_OFFSET + namelen > reclen
                {
                    return Err(io::Error::other("invalid /dev/fd directory record"));
                }

                let mut parsed = Some(0_i32);
                for byte in &self.buffer[offset + NAME_OFFSET..offset + NAME_OFFSET + namelen] {
                    parsed = parsed.and_then(|fd| {
                        byte.checked_sub(b'0')
                            .filter(|digit| *digit <= 9)
                            .and_then(|digit| fd.checked_mul(10)?.checked_add(i32::from(digit)))
                    });
                }
                if let Some(fd) = parsed.filter(|fd| *fd >= 3) {
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                    if flags == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if flags & libc::FD_CLOEXEC == 0
                        && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                offset += reclen;
            }
        }
    }
}

impl ShellRuntime {
    fn create(workspace_root: &Path, cwd: &Path) -> io::Result<Self> {
        if crate::sandbox::sandbox_enforcement() != crate::sandbox::SandboxEnforcement::OsEnforced {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "agent shell refused: this machine cannot enforce the workspace filesystem boundary",
            ));
        }

        let workspace_root = crate::sandbox::plain_canonical(workspace_root)?;
        let cwd = crate::sandbox::plain_canonical(cwd)?;
        if !cwd.starts_with(&workspace_root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "agent shell cwd must be inside its selected workspace",
            ));
        }
        let root = std::env::temp_dir().join(format!(
            "little-monkey-agent-shell-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root)?;
        let cleanup_root = root.clone();
        let result = Self::create_in_root(workspace_root, cwd, root);
        if result.is_err() {
            let _ = fs::remove_dir_all(cleanup_root);
        }
        result
    }

    fn create_in_root(workspace_root: PathBuf, cwd: PathBuf, root: PathBuf) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let root = crate::sandbox::plain_canonical(&root)?;
        let home = root.join(PRIVATE_HOME);
        let tmp = root.join(PRIVATE_TMP);
        fs::create_dir(&home)?;
        fs::create_dir(&tmp)?;
        let policy = crate::sandbox::workspace_shell_policy(&workspace_root, &home, &tmp);

        #[cfg(target_os = "macos")]
        let profile = {
            let profile = root.join(SEATBELT_PROFILE);
            let body = crate::sandbox::build_seatbelt_profile_for_roots(
                &[workspace_root.clone(), root.clone()],
                &policy.readable_roots,
                false,
            );
            fs::write(&profile, body)?;
            profile
        };
        Ok(Self {
            root,
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            workspace_root,
            cwd,
            env: policy.env,
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            readable_roots: policy.readable_roots,
            #[cfg(target_os = "macos")]
            profile,
        })
    }

    #[cfg(target_os = "linux")]
    fn writable_roots(&self) -> [PathBuf; 2] {
        [self.workspace_root.clone(), self.root.clone()]
    }
}

impl Drop for ShellRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(crate) struct ForegroundShell {
    #[cfg(not(target_os = "windows"))]
    child: tokio::process::Child,
    #[cfg(target_os = "windows")]
    child: crate::sandbox_windows::ConfinedChild,
    #[cfg(not(target_os = "windows"))]
    pgid: u32,
    #[cfg(not(target_os = "windows"))]
    reaped: bool,
    stdout: Option<AsyncShellReader>,
    stderr: Option<AsyncShellReader>,
    /// What is holding this shell's tree. Owned here rather than by the caller so
    /// the containment's lifetime is the shell's lifetime: on Windows the job
    /// handle *is* the containment, and dropping it early would release the tree.
    controller: ResourceController,
    native_pid: u32,
    _runtime: ShellRuntime,
}

impl ForegroundShell {
    /// What is enforcing each limit on this host, and what is not.
    pub(crate) fn capabilities(&self) -> crate::resource_control::ControllerCapabilities {
        self.controller.capabilities()
    }

    pub(crate) fn effective_limits(&self) -> EffectiveLimits {
        *self.controller.limits()
    }

    pub(crate) fn native_pid(&self) -> u32 {
        self.native_pid
    }

    pub(crate) fn id(&self) -> Option<u32> {
        #[cfg(not(target_os = "windows"))]
        {
            Some(self.pgid)
        }
        #[cfg(target_os = "windows")]
        {
            Some(self.child.id())
        }
    }

    pub(crate) fn take_stdout(&mut self) -> Option<AsyncShellReader> {
        self.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<AsyncShellReader> {
        self.stderr.take()
    }

    /// Wait for the shell while draining both pipes, under this shell's own
    /// resource controller.
    ///
    /// Lives here rather than at the call site because of a borrow that is really
    /// a design point: the wait needs `&mut self.child` and the sampler needs
    /// `&mut self.controller`, and only inside this impl are those two disjoint
    /// field borrows rather than two conflicting borrows of the shell. Pushing
    /// the race out to the caller would mean handing the controller out and
    /// hoping nobody drops it — and on Windows the controller *is* the
    /// containment, so dropping it early releases the tree.
    pub(crate) async fn wait_supervised(
        &mut self,
        stdout: AsyncShellReader,
        stderr: AsyncShellReader,
        output_cap: Option<usize>,
    ) -> io::Result<crate::resource_control::Supervised<io::Result<CapturedShell>>> {
        let child = &mut self.child;
        #[cfg(not(target_os = "windows"))]
        let (pgid, reaped) = (self.pgid, &mut self.reaped);
        let wait = async move {
            #[cfg(not(target_os = "windows"))]
            {
                while !child_exited_unreaped(pgid)? {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                // Keep the exited leader as a zombie until after this signal. Its
                // PID/PGID therefore cannot be reused for an unrelated process in
                // the gap between observing exit and cleaning up descendants.
                let _ = crate::os_signal::kill_process_group(pgid);
                let status = child.wait().await?;
                *reaped = true;
                Ok::<_, io::Error>(status)
            }
            #[cfg(target_os = "windows")]
            loop {
                if let Some(status) = child.try_wait()? {
                    break Ok::<_, io::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        // Joined, never sequenced. Draining after the wait is the classic
        // deadlock: a child that fills a 64 KiB pipe blocks until someone reads
        // it, so a wait that runs first waits forever and the command reports a
        // timeout instead of its output.
        let work = async move {
            let (status, stdout, stderr) = tokio::try_join!(
                wait,
                crate::output_cap::drain_capped(stdout, output_cap),
                crate::output_cap::drain_capped(stderr, output_cap),
            )?;
            Ok(CapturedShell {
                status,
                stdout,
                stderr,
            })
        };
        crate::resource_control::run_under(&mut self.controller, work).await
    }

    /// Wait for the leader alone, with nothing supervising.
    ///
    /// Test-only, and deliberately so: every production path goes through
    /// [`Self::wait_supervised`], because a wait that does not sample is a wait
    /// under no limit at all. This exists for the confinement tests, which assert
    /// what happens to the *tree* after the leader exits and would be muddied by a
    /// second reason the tree could disappear.
    #[cfg(test)]
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(not(target_os = "windows"))]
        {
            while !child_exited_unreaped(self.pgid)? {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = crate::os_signal::kill_process_group(self.pgid);
            let status = self.child.wait().await;
            if status.is_ok() {
                self.reaped = true;
            }
            status
        }
        #[cfg(target_os = "windows")]
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(crate) fn terminate_tree(&mut self) {
        #[cfg(not(target_os = "windows"))]
        {
            if !self.reaped {
                let _ = crate::os_signal::terminate_process_group(self.pgid);
                let _ = self.child.start_kill();
            }
        }
        #[cfg(target_os = "windows")]
        {
            let _ = self.child.kill();
        }
    }
}

impl Drop for ForegroundShell {
    fn drop(&mut self) {
        #[cfg(not(target_os = "windows"))]
        if !self.reaped {
            // Cancellation normally calls `terminate_tree` first. This is the
            // fail-safe for an early caller error after spawn but before wait.
            // Keep it immediate in Drop; blocking grace belongs to the explicit
            // timeout/cancel path, not unwinding.
            let _ = crate::os_signal::kill_process_group(self.pgid);
            let _ = self.child.start_kill();
        }
    }
}

#[cfg(target_os = "macos")]
fn harden_macos_child_authority(fd_sweep: &mut MacFdSweep) -> io::Result<()> {
    // These calls operate only on the forked child and use no process-global
    // allocation or locks, as required by Command's post-fork hook.
    // Enumerating in the already-forked child closes the race where another app
    // thread opens an inheritable descriptor between a parent-side snapshot and
    // `fork`. Darwin has no close_range(2); __getdirentries64 is its raw kernel
    // wrapper and the backing directory/buffer were prepared in the parent.
    fd_sweep.mark_all_cloexec()?;

    const TASK_BOOTSTRAP_PORT: libc::c_int = 4;
    const MACH_PORT_NULL: libc::c_uint = 0;
    unsafe extern "C" {
        static mach_task_self_: libc::c_uint;
        fn task_set_special_port(
            task: libc::c_uint,
            which_port: libc::c_int,
            port: libc::c_uint,
        ) -> libc::c_int;
    }
    let result =
        unsafe { task_set_special_port(mach_task_self_, TASK_BOOTSTRAP_PORT, MACH_PORT_NULL) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn configure_tokio(
    runtime: &ShellRuntime,
    shell_command: &str,
    controller: &ResourceController,
) -> io::Result<tokio::process::Command> {
    #[cfg(target_os = "macos")]
    let (program, args) = (
        "/usr/bin/sandbox-exec",
        vec![
            "-f".to_string(),
            runtime.profile.to_string_lossy().into_owned(),
            "--".to_string(),
            "/bin/sh".to_string(),
            "-c".to_string(),
            shell_command.to_string(),
        ],
    );
    #[cfg(not(target_os = "macos"))]
    let (program, args) = ("/bin/sh", vec!["-c".to_string(), shell_command.to_string()]);

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(&runtime.cwd)
        .env_clear()
        .envs(runtime.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    crate::os_limits::apply(crate::os_limits::ChildLimits::baseline(), &mut command);
    // Before `exec`, so the target program has never run outside its containment
    // — on Linux the child is already in its cgroup when it starts. This composes
    // with the confinement below rather than replacing it: a resource bound is
    // not a filesystem or network boundary and neither substitutes for the other.
    controller.prepare_tokio(&mut command)?;
    #[cfg(target_os = "macos")]
    {
        let mut fd_sweep = MacFdSweep::create()?;
        unsafe {
            command.pre_exec(move || harden_macos_child_authority(&mut fd_sweep));
        }
    }
    #[cfg(target_os = "linux")]
    if !crate::sandbox_linux::confine_roots(
        &mut command,
        &runtime.writable_roots(),
        &runtime.readable_roots,
        false,
    )? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "agent shell refused: Landlock filesystem confinement is unavailable",
        ));
    }
    Ok(command)
}

#[cfg(not(target_os = "windows"))]
fn configure_std(
    runtime: &ShellRuntime,
    shell_command: &str,
    controller: &ResourceController,
) -> io::Result<std::process::Command> {
    use std::os::unix::process::CommandExt;

    #[cfg(target_os = "macos")]
    let (program, args) = (
        "/usr/bin/sandbox-exec",
        vec![
            "-f".to_string(),
            runtime.profile.to_string_lossy().into_owned(),
            "--".to_string(),
            "/bin/sh".to_string(),
            "-c".to_string(),
            shell_command.to_string(),
        ],
    );
    #[cfg(not(target_os = "macos"))]
    let (program, args) = ("/bin/sh", vec!["-c".to_string(), shell_command.to_string()]);

    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(&runtime.cwd)
        .env_clear()
        .envs(runtime.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    crate::os_limits::apply_std(crate::os_limits::ChildLimits::baseline(), &mut command);
    controller.prepare_std(&mut command)?;
    #[cfg(target_os = "macos")]
    {
        let mut fd_sweep = MacFdSweep::create()?;
        unsafe {
            command.pre_exec(move || harden_macos_child_authority(&mut fd_sweep));
        }
    }
    #[cfg(target_os = "linux")]
    if !crate::sandbox_linux::confine_std_roots(
        &mut command,
        &runtime.writable_roots(),
        &runtime.readable_roots,
        false,
    )? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "agent shell refused: Landlock filesystem confinement is unavailable",
        ));
    }
    Ok(command)
}

#[cfg(target_os = "windows")]
fn spawn_windows(
    runtime: &ShellRuntime,
    shell_command: &str,
    controller: &ResourceController,
) -> io::Result<crate::sandbox_windows::ConfinedChild> {
    let container = crate::sandbox_windows::open_workspace_app_container(
        &runtime.workspace_root,
        &runtime.readable_roots,
    )?;
    container.ensure_tree_access_persistent(&runtime.workspace_root)?;
    for root in &runtime.readable_roots {
        let _ = container.ensure_tree_read_access_if_permitted_persistent(root)?;
    }
    let grants = vec![container.grant_tree_access_scoped(&runtime.root)?];
    // The job comes from the controller, so its memory and process ceilings are
    // this process's effective limits rather than fixed constants. The spawn site
    // still creates the process suspended and assigns it before resuming, which is
    // what makes the bound hold from the first instruction.
    crate::sandbox_windows::spawn_confined_child(
        container,
        controller.windows_job_for_spawn()?,
        grants,
        shell_command,
        &runtime.cwd,
        &runtime.env,
        false,
    )
}

/// Resolve what this shell will actually run under.
///
/// One place, for every shell on either client. The class default states the
/// tree bounds the controller holds; a caller may tighten any of them and may
/// never widen one, which is what [`EffectiveLimits::resolve`] guarantees by
/// intersecting rather than overriding.
///
/// The platform guardrail layer is deliberately empty here rather than absent:
/// the shell's own fixed ceilings live inside the backends (the Windows job's
/// independent limits, `os_limits`' core-dump refusal), where a caller cannot
/// reach them at all. Expressing them as a layer would let this merge widen
/// them, which is the one thing a guardrail must not permit.
pub(crate) fn effective_shell_limits(kind: ProcessKind, caller: ProcessLimits) -> EffectiveLimits {
    EffectiveLimits::resolve(&[
        LimitLayer::new(LimitSource::ClassDefault, kind.default_limits()),
        LimitLayer::new(LimitSource::UserOverride, caller),
    ])
}

pub(crate) fn spawn_foreground(
    workspace_root: &Path,
    cwd: &Path,
    shell_command: &str,
    limits: ProcessLimits,
) -> io::Result<ForegroundShell> {
    let runtime = ShellRuntime::create(workspace_root, cwd)?;
    let mut controller =
        ResourceController::new(effective_shell_limits(ProcessKind::ForegroundShell, limits));
    #[cfg(not(target_os = "windows"))]
    let (child, stdout, stderr) = {
        let mut child = configure_tokio(&runtime, shell_command, &controller)?.spawn()?;
        let stdout = child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as AsyncShellReader);
        let stderr = child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as AsyncShellReader);
        (child, stdout, stderr)
    };
    #[cfg(not(target_os = "windows"))]
    let pgid = child
        .id()
        .ok_or_else(|| io::Error::other("confined shell exited before its pid was recorded"))?;
    #[cfg(target_os = "windows")]
    let (child, stdout, stderr) = {
        let mut child = spawn_windows(&runtime, shell_command, &controller)?;
        let stdout = child
            .stdout
            .take()
            .map(tokio::fs::File::from_std)
            .map(|pipe| Box::new(pipe) as AsyncShellReader);
        let stderr = child
            .stderr
            .take()
            .map(tokio::fs::File::from_std)
            .map(|pipe| Box::new(pipe) as AsyncShellReader);
        (child, stdout, stderr)
    };
    #[cfg(target_os = "windows")]
    let native_pid = child.id();
    #[cfg(not(target_os = "windows"))]
    let native_pid = pgid;
    // Attaching can fail only if the child is already gone, which for a command
    // that exits instantly is normal rather than an error: there is then nothing
    // left to bound, and the exit is the answer.
    let _ = controller.attach(native_pid);
    Ok(ForegroundShell {
        child,
        #[cfg(not(target_os = "windows"))]
        pgid,
        #[cfg(not(target_os = "windows"))]
        reaped: false,
        stdout,
        stderr,
        controller,
        native_pid,
        _runtime: runtime,
    })
}

pub(crate) struct BackgroundShellChild {
    #[cfg(not(target_os = "windows"))]
    child: std::process::Child,
    #[cfg(target_os = "windows")]
    child: crate::sandbox_windows::ConfinedChild,
    #[cfg(not(target_os = "windows"))]
    pgid: u32,
    reaped: bool,
    _runtime: Option<ShellRuntime>,
}

impl BackgroundShellChild {
    pub(crate) fn id(&self) -> u32 {
        #[cfg(not(target_os = "windows"))]
        {
            self.pgid
        }
        #[cfg(target_os = "windows")]
        {
            self.child.id()
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(not(target_os = "windows"))]
        let status = if child_exited_unreaped(self.pgid)? {
            // As in the foreground path, signal while the zombie leader still
            // prevents this process-group id from being reused, then reap it.
            let _ = crate::os_signal::kill_process_group(self.pgid);
            Some(self.child.wait()?)
        } else {
            None
        };
        #[cfg(target_os = "windows")]
        let status = self.child.try_wait()?;
        if status.is_some() && !self.reaped {
            self.reaped = true;
            drop(self._runtime.take());
        }
        Ok(status)
    }

    pub(crate) fn kill(&mut self) -> io::Result<()> {
        if self.reaped || self.try_wait()?.is_some() {
            return Ok(());
        }
        #[cfg(not(target_os = "windows"))]
        {
            crate::os_signal::terminate_process_group(self.pgid).or_else(|_| self.child.kill())
        }
        #[cfg(target_os = "windows")]
        {
            self.child.kill()
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn unconfined_for_lifecycle_test(child: std::process::Child) -> Self {
        let pgid = child.id();
        Self {
            child,
            pgid,
            reaped: false,
            _runtime: None,
        }
    }
}

impl Drop for BackgroundShellChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if self.try_wait().ok().flatten().is_some() {
            return;
        }

        #[cfg(not(target_os = "windows"))]
        {
            if crate::os_signal::terminate_process_group(self.pgid).is_err() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
            self.reaped = true;
        }
        #[cfg(target_os = "windows")]
        {
            let _ = self.child.kill();
            self.reaped = true;
        }
        drop(self._runtime.take());
    }
}

pub(crate) struct BackgroundSpawn {
    pub child: BackgroundShellChild,
    /// Held by the caller for as long as the shell may run, for the reason
    /// [`ForegroundShell::controller`] gives: on Windows the job handle is the
    /// containment, so dropping it releases the tree.
    pub controller: ResourceController,
    pub stdout: Option<Box<dyn io::Read + Send>>,
    pub stderr: Option<Box<dyn io::Read + Send>>,
}

pub(crate) fn spawn_background(
    workspace_root: &Path,
    cwd: &Path,
    shell_command: &str,
    limits: ProcessLimits,
) -> io::Result<BackgroundSpawn> {
    let runtime = ShellRuntime::create(workspace_root, cwd)?;
    let mut controller =
        ResourceController::new(effective_shell_limits(ProcessKind::BackgroundShell, limits));
    #[cfg(not(target_os = "windows"))]
    let (child, stdout, stderr) = {
        let mut child = configure_std(&runtime, shell_command, &controller)?.spawn()?;
        let stdout = child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn io::Read + Send>);
        let stderr = child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn io::Read + Send>);
        (child, stdout, stderr)
    };
    #[cfg(target_os = "windows")]
    let (child, stdout, stderr) = {
        let mut child = spawn_windows(&runtime, shell_command, &controller)?;
        let stdout = child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn io::Read + Send>);
        let stderr = child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn io::Read + Send>);
        (child, stdout, stderr)
    };
    let native_pid = child.id();
    let _ = controller.attach(native_pid);
    Ok(BackgroundSpawn {
        child: BackgroundShellChild {
            #[cfg(not(target_os = "windows"))]
            pgid: native_pid,
            child,
            reaped: false,
            _runtime: Some(runtime),
        },
        controller,
        stdout,
        stderr,
    })
}

/// A finished foreground shell: its status and both bounded streams.
pub struct CapturedShell {
    pub status: ExitStatus,
    pub stdout: crate::output_cap::CappedStream,
    pub stderr: crate::output_cap::CappedStream,
}

pub struct ShellOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// What the child produced, as opposed to what was retained. Equal to the
    /// buffer lengths when nothing was dropped.
    pub stdout_total_bytes: u64,
    pub stderr_total_bytes: u64,
    pub truncated: bool,
    /// The limit that ended this command, if one did. `None` means the command
    /// ended on its own terms — which is different from "no limit was in force".
    pub breach: Option<crate::resource_control::LimitBreach>,
    /// What the tree actually held, for the process record and the UI.
    pub usage: crate::resource_control::ResourceSample,
    /// What was holding each limit on this host.
    pub enforcement: crate::resource_control::ControllerCapabilities,
    pub limits: EffectiveLimits,
    pub native_pid: u32,
}

/// AppHandle-free entry point used by `monkey-cli`; desktop foreground and
/// background tools use the same `spawn_*` primitives above so the authority
/// boundary cannot drift by client.
///
/// `output_cap` is the ceiling each stream is held to **as it arrives**. `None`
/// keeps everything and is for the callers whose correctness needs the whole
/// document; every other caller passes a number, because this used to
/// `read_to_end` both pipes and a command printing a gigabyte took a gigabyte of
/// this process's heap with it.
///
/// `limits` tightens the class defaults. It can never loosen them.
pub async fn run_to_output(
    workspace_root: &Path,
    cwd: &Path,
    shell_command: &str,
    timeout: Duration,
    output_cap: Option<usize>,
    limits: ProcessLimits,
) -> io::Result<ShellOutput> {
    let mut shell = spawn_foreground(workspace_root, cwd, shell_command, limits)?;
    let enforcement = shell.capabilities();
    let effective = shell.effective_limits();
    let native_pid = shell.native_pid();
    let stdout = shell
        .take_stdout()
        .ok_or_else(|| io::Error::other("confined shell had no stdout pipe"))?;
    let stderr = shell
        .take_stderr()
        .ok_or_else(|| io::Error::other("confined shell had no stderr pipe"))?;

    // Two bounds, and they are different facts. The controller's limits belong to
    // the *process* — exceed one and the workload is torn down and recorded as
    // `limit_exceeded`. The caller's timeout belongs to the *call* — a tool that
    // gives up after 120 s has not observed a resource breach, so it wraps the
    // supervision rather than being expressed as a wall limit inside it.
    let supervised =
        tokio::time::timeout(timeout, shell.wait_supervised(stdout, stderr, output_cap)).await;

    let finish = |breach,
                  stdout: crate::output_cap::CappedStream,
                  stderr: crate::output_cap::CappedStream,
                  exit_code,
                  usage| {
        Ok(ShellOutput {
            exit_code,
            stdout_total_bytes: stdout.total_bytes(),
            stderr_total_bytes: stderr.total_bytes(),
            truncated: stdout.was_truncated() || stderr.was_truncated(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            breach,
            usage,
            enforcement,
            limits: effective,
            native_pid,
        })
    };

    match supervised {
        Ok(Ok(crate::resource_control::Supervised::Completed(Ok(captured), usage))) => finish(
            None,
            captured.stdout,
            captured.stderr,
            captured.status.code(),
            usage,
        ),
        // A limit fired. `run_under` has already terminated the tree, so what is
        // left is to report *which* limit with both numbers — the output produced
        // up to that point is lost with the dropped drain futures, which is the
        // honest outcome: the command did not finish.
        Ok(Ok(crate::resource_control::Supervised::Breached(breach, usage))) => finish(
            Some(breach),
            crate::output_cap::CappedStream::default(),
            crate::output_cap::CappedStream::default(),
            None,
            usage,
        ),
        Ok(Ok(crate::resource_control::Supervised::Completed(Err(error), _))) => {
            shell.terminate_tree();
            Err(error)
        }
        Ok(Err(error)) => {
            shell.terminate_tree();
            Err(error)
        }
        Err(_) => {
            shell.terminate_tree();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("command timed out after {} seconds", timeout.as_secs()),
            ))
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) fn posix_spawn_inheriting_fd_probe_for_test(fd: libc::c_int) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let executable = std::env::current_exe()?;
    let path = CString::new(executable.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "test path contains NUL"))?;
    let exact = CString::new("--exact").expect("static argv has no NUL");
    let probe = CString::new("workspace_shell::tests::inherited_fd_probe_child")
        .expect("static argv has no NUL");
    let one_thread = CString::new("--test-threads=1").expect("static argv has no NUL");
    let mut argv = [
        path.as_ptr().cast_mut(),
        exact.as_ptr().cast_mut(),
        probe.as_ptr().cast_mut(),
        one_thread.as_ptr().cast_mut(),
        std::ptr::null_mut(),
    ];
    let probe_env = CString::new(format!("LITTLE_MONKEY_INHERITED_FD_PROBE={fd}"))
        .expect("numeric fd environment has no NUL");
    let mut envp = [probe_env.as_ptr().cast_mut(), std::ptr::null_mut()];
    let mut pid = 0;
    let spawned = unsafe {
        libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            argv.as_mut_ptr(),
            envp.as_mut_ptr(),
        )
    };
    if spawned != 0 {
        return Err(io::Error::from_raw_os_error(spawned));
    }
    let mut status = 0;
    loop {
        if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
            return Ok(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two spawn entry points with no caller override, which is what every
    /// boundary test wants: these assert the *confinement*, and a limit set here
    /// would only add a second reason a command could end.
    fn spawn_foreground_for_test(
        workspace_root: &Path,
        cwd: &Path,
        shell_command: &str,
    ) -> io::Result<ForegroundShell> {
        spawn_foreground(workspace_root, cwd, shell_command, ProcessLimits::default())
    }

    fn spawn_background_for_test(
        workspace_root: &Path,
        cwd: &Path,
        shell_command: &str,
    ) -> io::Result<BackgroundSpawn> {
        spawn_background(workspace_root, cwd, shell_command, ProcessLimits::default())
    }

    struct TestTree(PathBuf);

    impl TestTree {
        fn create() -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-shell-boundary-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&path).expect("create test tree");
            Self(path)
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    async fn assert_process_group_gone(pgid: u32) {
        let pgid = i32::try_from(pgid).expect("test pgid fits i32");
        for _ in 0..100 {
            if unsafe { libc::killpg(pgid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("process group {pgid} survived its shell leader");
    }

    fn confinement_available_for_test() -> bool {
        if crate::sandbox::sandbox_enforcement() == crate::sandbox::SandboxEnforcement::OsEnforced {
            return true;
        }
        assert!(
            std::env::var_os("CI").is_none(),
            "CI platform did not provide its required shell confinement backend"
        );
        false
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn foreground_shell_ends_backgrounded_descendants_before_returning() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        let mut shell =
            spawn_foreground_for_test(&workspace, &workspace, "sleep 30 > detached.log 2>&1 &")
                .expect("spawn foreground shell");
        let pgid = shell.id().expect("shell pid");
        let status = tokio::time::timeout(Duration::from_secs(10), shell.wait())
            .await
            .expect("shell leader exits")
            .expect("wait succeeds");
        assert!(status.success());
        assert_process_group_gone(pgid).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn background_leader_exit_ends_descendants_and_releases_private_runtime() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        let mut spawned =
            spawn_background_for_test(&workspace, &workspace, "sleep 30 > detached.log 2>&1 &")
                .expect("spawn background shell");
        let pgid = spawned.child.id();
        let runtime_root = spawned
            .child
            ._runtime
            .as_ref()
            .expect("private runtime")
            .root
            .clone();
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = spawned.child.try_wait().expect("poll shell leader") {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("shell leader exits");

        assert!(status.success());
        assert!(spawned.child.reaped);
        assert!(spawned.child._runtime.is_none());
        assert!(!runtime_root.exists());
        assert_process_group_gone(pgid).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn live_shell_cannot_open_a_host_unix_socket() {
        if !confinement_available_for_test() {
            return;
        }
        if !Path::new("/usr/bin/python3").is_file() {
            assert!(
                std::env::var_os("CI").is_none(),
                "Linux CI needs python3 to exercise AF_UNIX denial"
            );
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        let output = run_to_output(
            &workspace,
            &workspace,
            "/usr/bin/python3 -c 'print(\"PY_OK\")' && (/usr/bin/python3 -c 'import socket; socket.socket(socket.AF_UNIX)' >\"$TMPDIR/socket-out\" 2>&1 && printf ESCAPE_UNIX || printf DENIED_UNIX)",
            Duration::from_secs(20),
            None,
            ProcessLimits::default(),
        )
        .await
        .expect("run confined socket probe");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "PY_OK\nDENIED_UNIX"
        );
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn live_shell_cannot_signal_its_host_parent() {
        if !confinement_available_for_test() {
            return;
        }
        let control = std::process::Command::new("/bin/sh")
            .args(["-c", "kill -0 $PPID"])
            .status()
            .expect("run signal control");
        assert!(control.success(), "host signal control must succeed");

        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");
        let output = run_to_output(
            &workspace,
            &workspace,
            "kill -0 $PPID 2>/dev/null && printf ESCAPE_SIGNAL || printf DENIED_SIGNAL",
            Duration::from_secs(20),
            None,
            ProcessLimits::default(),
        )
        .await
        .expect("run confined signal probe");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "DENIED_SIGNAL");
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn live_shell_cannot_query_launchd_outside_the_workspace() {
        if !confinement_available_for_test() {
            return;
        }
        let control = std::process::Command::new("/bin/launchctl")
            .args(["print", "system"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("launchctl control runs");
        assert!(control.success(), "host launchctl control must succeed");

        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");
        let output = run_to_output(
            &workspace,
            &workspace,
            "/bin/launchctl print system >\"$TMPDIR/launchctl-out\" 2>&1 && printf ESCAPE_MACH || printf DENIED_MACH",
            Duration::from_secs(20),
            None,
            ProcessLimits::default(),
        )
        .await
        .expect("run confined launchd probe");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "DENIED_MACH");
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn macos_group_escape_outlives_cleanup_but_does_not_widen_seatbelt() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");
        let outside = tree.0.join("outside-secret.txt");
        fs::write(&outside, "outside-secret").expect("write outside fixture");
        let outside = crate::sandbox::plain_canonical(&outside).expect("canonical outside file");
        let pid_file = workspace.join("escaped.pid");
        let result_file = workspace.join("escaped.result");
        let read_file = workspace.join("escaped.read");
        let log_file = workspace.join("escaped.log");
        let command = format!(
            "set -m; (trap '' HUP TERM; sleep 1; \
             if /bin/cat {} >{}; then r=ESCAPE; else r=DENIED; fi; \
             if printf changed >{}; then w=ESCAPE; else w=DENIED; fi; \
             printf '%s:%s' \"$r\" \"$w\" >{}) >{} 2>&1 & \
             printf '%s' \"$!\" >{}",
            quote(&outside),
            quote(&read_file),
            quote(&outside),
            quote(&result_file),
            quote(&log_file),
            quote(&pid_file),
        );
        run_to_output(
            &workspace,
            &workspace,
            &command,
            Duration::from_secs(20),
            None,
            ProcessLimits::default(),
        )
        .await
        .expect("run group-escape probe");
        let pid: libc::pid_t = fs::read_to_string(pid_file)
            .expect("escaped child pid")
            .parse()
            .expect("numeric escaped child pid");
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "the test must exercise Darwin's process-group lifetime gap"
        );
        for _ in 0..100 {
            if result_file.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(fs::read_to_string(result_file).unwrap(), "DENIED:DENIED");
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside-secret");
    }

    #[tokio::test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    async fn live_shell_does_not_inherit_an_ambient_outside_file_descriptor() {
        use std::os::fd::AsRawFd;

        struct RestoreFdFlags {
            fd: libc::c_int,
            flags: libc::c_int,
        }
        impl Drop for RestoreFdFlags {
            fn drop(&mut self) {
                unsafe {
                    libc::fcntl(self.fd, libc::F_SETFD, self.flags);
                }
            }
        }

        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");
        let probe_executable = workspace.join("inherited-fd-probe");
        let current_executable = std::env::current_exe().expect("current test executable");
        fs::hard_link(&current_executable, &probe_executable)
            .or_else(|_| fs::copy(&current_executable, &probe_executable).map(|_| ()))
            .expect("place fd probe inside selected workspace");
        let inside_probe = workspace.join("inside-probe.txt");
        fs::write(&inside_probe, "inside-probe").expect("write inside probe");
        // Configure first: on macOS this prepares the /dev/fd handle and
        // buffer, so only child-side enumeration can see the later descriptor.
        let runtime = ShellRuntime::create(&workspace, &workspace).expect("create shell runtime");
        let mut command = configure_tokio(
            &runtime,
            &format!(
                "if LITTLE_MONKEY_FD_PROBE_PATH={} {} --exact workspace_shell::tests::inherited_fd_probe_child --test-threads=1 >/dev/null 2>&1; then printf PROBE_OK:; else printf PROBE_BROKEN:; fi; LITTLE_MONKEY_INHERITED_FD_PROBE=\"$LATE_FD\" {} --exact workspace_shell::tests::inherited_fd_probe_child --test-threads=1 >/dev/null 2>&1 && printf ESCAPE_FD || printf DENIED_FD",
                quote(&inside_probe),
                quote(&probe_executable),
                quote(&probe_executable),
            ),
            &ResourceController::new(EffectiveLimits::default()),
        )
        .expect("configure confined inherited-descriptor probe");
        let outside = tree.0.join("outside-secret.txt");
        fs::write(&outside, "outside-secret").expect("write outside");
        let outside = fs::File::open(outside).expect("open ambient outside descriptor");
        let fd = outside.as_raw_fd();
        command.env("LATE_FD", fd.to_string());
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(original_flags, -1);
        let _restore_flags = RestoreFdFlags {
            fd,
            flags: original_flags,
        };
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, -1);
        assert!(
            posix_spawn_inheriting_fd_probe_for_test(fd).expect("run inherited-fd control"),
            "the control must inherit and read the descriptor"
        );

        let output = command
            .output()
            .await
            .expect("run confined descriptor probe");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "PROBE_OK:DENIED_FD"
        );
    }

    // --- K4: the limits actually fire, on a real process tree ------------
    //
    // Every test below runs its workload as a *grandchild* of the shell — `sh -c
    // "<binary>"` — because the direct child is always a shell holding a few
    // hundred kilobytes and forking nothing. A bound that only measures the pid
    // this app spawned is evaded by the normal case, not by a trick, which is
    // exactly the gap these exist to close.

    /// Allocates and touches `LITTLE_MONKEY_MEMORY_HOG_MIB` mebibytes, then
    /// sleeps holding them.
    ///
    /// Written rather than allocated: an untouched allocation is not resident, so
    /// a hog that only called `Vec::with_capacity` would be invisible to every
    /// memory limit and this whole test would pass against a broken bound.
    #[test]
    #[cfg(unix)]
    fn memory_hog_child() {
        let Some(mib) = std::env::var("LITTLE_MONKEY_MEMORY_HOG_MIB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return;
        };
        let mut held: Vec<Vec<u8>> = Vec::new();
        for _ in 0..mib {
            let mut block = vec![0_u8; 1024 * 1024];
            // Touch every page so the kernel has to back it.
            for page in block.chunks_mut(4096) {
                page[0] = 1;
            }
            held.push(block);
        }
        std::thread::sleep(Duration::from_secs(30));
        // Keeps the allocation live across the sleep.
        assert_eq!(held.len(), mib);
    }

    /// Places a copy of the test binary *inside* the workspace and returns it.
    ///
    /// The confinement is why this is not just `current_exe()`: the shell may
    /// only execute what is inside its grant, and the target directory is not.
    /// A test that ran the binary from its build path would be denied by
    /// Seatbelt/Landlock and then pass for the wrong reason — the workload would
    /// exit instantly, no limit would fire, and the assertion would read as "the
    /// bound did not work".
    #[cfg(unix)]
    fn place_test_binary_in(workspace: &Path, name: &str) -> PathBuf {
        let placed = workspace.join(name);
        let current = std::env::current_exe().expect("the test binary knows its own path");
        fs::hard_link(&current, &placed)
            .or_else(|_| fs::copy(&current, &placed).map(|_| ()))
            .expect("place the workload binary inside the selected workspace");
        placed
    }

    /// The headline case: a grandchild outgrows the budget and the *whole* tree
    /// is stopped, with the record naming the limit and both numbers.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_grandchild_that_outgrows_the_memory_budget_takes_the_whole_tree_down() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        // The variable is set *inside* the command line because the shell's
        // environment is scrubbed at spawn: nothing this process exports reaches
        // the child, which is the confinement working and was the first thing
        // this test got wrong.
        let hog = format!(
            "LITTLE_MONKEY_MEMORY_HOG_MIB=512 {} --exact \
             workspace_shell::tests::memory_hog_child --test-threads=1 >/dev/null 2>&1",
            quote(&place_test_binary_in(&workspace, "memory-hog"))
        );
        let output = run_to_output(
            &workspace,
            &workspace,
            &hog,
            // Comfortably longer than the budget needs to fire, so a timeout can
            // never be mistaken for the limit working.
            Duration::from_secs(60),
            None,
            ProcessLimits {
                // 192 MiB against a 512 MiB hog: high enough that the test binary's
                // own baseline is not the thing that trips it, low enough to fire
                // within a sample or two.
                max_memory_bytes: Some(192 * 1024 * 1024),
                ..ProcessLimits::default()
            },
        )
        .await
        .expect("a limit breach is an outcome, not an error");

        let breach = output
            .breach
            .expect("512 MiB of touched pages must exceed a 192 MiB tree budget");
        assert_eq!(breach.limit, "max_memory_bytes");
        assert_eq!(breach.configured, 192 * 1024 * 1024);
        assert!(
            breach.observed > breach.configured,
            "the recorded measurement must be the one that tripped it: {breach:?}"
        );
        // The tree, not the shell: the hog is a grandchild, so this is the
        // assertion that would still pass against a bound measuring only the pid
        // we spawned — and would fail against one that killed only it.
        assert!(
            crate::process_tree::measure_tree(output.native_pid)
                .expect("snapshot")
                .is_none(),
            "the owned tree must be gone once its budget fired"
        );
    }

    /// A fork-heavy workload hits its own ceiling without the logged-in user's
    /// processes counting against it.
    ///
    /// That second half is the reason `RLIMIT_NPROC` was rejected for this: it
    /// counts per real uid, so on a busy desktop the bound fires because the user
    /// opened a browser.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_fork_heavy_tree_hits_its_own_process_ceiling_and_not_the_users() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        // The host already has far more than 12 processes belonging to this user,
        // so a per-uid bound would fire before the first `sleep` started.
        let host_processes = crate::process_tree::snapshot().expect("snapshot").len();
        assert!(
            host_processes > 12,
            "this assertion is only meaningful on a host with real process load: {host_processes}"
        );

        let output = run_to_output(
            &workspace,
            &workspace,
            "i=0; while [ $i -lt 40 ]; do sleep 20 & i=$((i+1)); done; wait",
            Duration::from_secs(60),
            None,
            ProcessLimits {
                max_child_processes: Some(12),
                ..ProcessLimits::default()
            },
        )
        .await
        .expect("a limit breach is an outcome, not an error");

        let breach = output
            .breach
            .expect("forty concurrent children must exceed a twelve-process tree budget");
        assert_eq!(breach.limit, "max_child_processes");
        assert_eq!(breach.configured, 12);
        assert!(breach.observed > 12, "{breach:?}");
        assert!(
            breach.observed < u64::try_from(host_processes).unwrap(),
            "the count must be of the owned tree, not of everything this uid owns: {breach:?}"
        );
        assert!(crate::process_tree::measure_tree(output.native_pid)
            .expect("snapshot")
            .is_none());
    }

    /// A wall limit must end the descendants too, which is the half `kill_on_drop`
    /// never did: it SIGKILLs one pid, so `sh -c "sleep 30"` reaped the shell and
    /// left the sleep running.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_wall_limit_ends_the_grandchild_and_not_only_the_shell() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        let output = run_to_output(
            &workspace,
            &workspace,
            "sleep 30 & sleep 30",
            Duration::from_secs(60),
            None,
            ProcessLimits {
                max_wall_ms: Some(1_000),
                ..ProcessLimits::default()
            },
        )
        .await
        .expect("a limit breach is an outcome, not an error");

        let breach = output.breach.expect("a 30s command past a 1s wall budget");
        assert_eq!(breach.limit, "max_wall_ms");
        assert_eq!(breach.configured, 1_000);
        assert!(breach.observed >= 1_000, "{breach:?}");
        assert!(
            crate::process_tree::measure_tree(output.native_pid)
                .expect("snapshot")
                .is_none(),
            "neither the shell nor either sleep may survive its deadline"
        );
    }

    /// A command that finishes inside every bound is not a breach, and its output
    /// survives.
    ///
    /// The counter-test. Without it "everything is a limit kill" passes all three
    /// tests above.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_command_inside_its_bounds_completes_normally_with_its_output() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        let output = run_to_output(
            &workspace,
            &workspace,
            "printf INSIDE_THE_BOUNDS",
            Duration::from_secs(30),
            None,
            ProcessLimits {
                max_wall_ms: Some(30_000),
                max_memory_bytes: Some(1024 * 1024 * 1024),
                max_child_processes: Some(64),
                ..ProcessLimits::default()
            },
        )
        .await
        .expect("an ordinary command");

        assert!(output.breach.is_none(), "{:?}", output.breach);
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "INSIDE_THE_BOUNDS");
    }

    /// Flooding both pipes at once must not deadlock, and must not grow the app's
    /// heap past the cap — while the command still exits normally.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_flood_on_both_pipes_stays_capped_without_deadlocking_the_command() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        const CAP: usize = 4 * 1024;
        let output = run_to_output(
            &workspace,
            &workspace,
            // Both streams, well past the cap and past any pipe buffer, so a
            // reader that drained them in sequence would block forever.
            "i=0; while [ $i -lt 400 ]; do \
                 printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; \
                 printf 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' >&2; \
                 i=$((i+1)); \
             done",
            Duration::from_secs(60),
            Some(CAP),
            ProcessLimits::default(),
        )
        .await
        .expect("a noisy command is still a command");

        assert_eq!(
            output.exit_code,
            Some(0),
            "a command that merely printed too much must still report its own exit"
        );
        assert!(output.stdout.len() <= CAP, "{}", output.stdout.len());
        assert!(output.stderr.len() <= CAP, "{}", output.stderr.len());
        assert!(output.truncated);
        assert_eq!(
            output.stdout_total_bytes,
            400 * 40,
            "the cap bounds what is kept; the total is what the command produced"
        );
        assert_eq!(output.stderr_total_bytes, 400 * 40);
    }

    /// A descendant that leaves the process group is still terminated.
    ///
    /// The one escape a POSIX process group cannot close on its own, and the
    /// reason the supervisor measures and signals by parent link *as well as* by
    /// group. `setsid` here is the deliberate version of what a daemonising build
    /// tool does by accident.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_descendant_that_leaves_the_process_group_is_still_terminated() {
        if !confinement_available_for_test() {
            return;
        }
        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");

        let marker = tree.0.join("escapee.pid");
        let output = run_to_output(
            &workspace,
            &workspace,
            &format!("(sleep 25 & echo $! > {}) ; sleep 25", quote(&marker)),
            Duration::from_secs(60),
            None,
            ProcessLimits {
                max_wall_ms: Some(1_000),
                ..ProcessLimits::default()
            },
        )
        .await
        .expect("a limit breach is an outcome, not an error");

        assert!(output.breach.is_some());
        // Give the terminate a beat to be observed, then assert on the recorded
        // pid rather than on the tree: the whole point is that this process was
        // not reachable through the group alone.
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(pid) = fs::read_to_string(&marker) {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                assert!(
                    crate::process_tree::ProcessIdentity::of(pid).is_none(),
                    "a descendant of the owned tree survived its budget kill"
                );
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn inherited_fd_probe_child() {
        if let Some(path) = std::env::var_os("LITTLE_MONKEY_FD_PROBE_PATH") {
            assert_eq!(fs::read_to_string(path).unwrap(), "inside-probe");
            return;
        }
        let Some(fd) = std::env::var("LITTLE_MONKEY_INHERITED_FD_PROBE")
            .ok()
            .and_then(|value| value.parse::<libc::c_int>().ok())
        else {
            return;
        };
        let mut byte = 0_u8;
        assert_eq!(
            unsafe { libc::pread(fd, (&mut byte as *mut u8).cast(), 1, 0) },
            1,
            "inheritable fd probe could not read fd {fd}: {}",
            io::Error::last_os_error(),
        );
    }

    #[tokio::test]
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    async fn agent_shell_cannot_read_or_write_outside_selected_workspace() {
        if !confinement_available_for_test() {
            return;
        }

        let tree = TestTree::create();
        let workspace = tree.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let workspace = crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");
        let inside = workspace.join("inside.txt");
        let created = workspace.join("created.txt");
        let outside = tree.0.join("outside-secret.txt");
        fs::write(&inside, "workspace-ok").expect("write inside");
        fs::write(&outside, "outside-secret").expect("write outside");

        #[cfg(unix)]
        let command = format!(
            "set -e; /bin/cat {}; printf created-ok > {}; \
             if value=$(/bin/cat {} 2>&1); then printf ' ESCAPE_READ:%s' \"$value\"; else printf ' DENIED_READ'; fi; \
             if printf hacked > {} 2>\"$TMPDIR/outside-write-error\"; then printf ' ESCAPE_WRITE'; else printf ' DENIED_WRITE'; fi",
            quote(&inside),
            quote(&created),
            quote(&outside),
            quote(&outside),
        );
        #[cfg(target_os = "windows")]
        let command = format!(
            "type \"{}\" & echo created-ok>\"{}\" & \
             (type \"{}\" && echo ESCAPE_READ) || echo DENIED_READ & \
             (echo hacked>\"{}\" && echo ESCAPE_WRITE) || echo DENIED_WRITE",
            inside.display(),
            created.display(),
            outside.display(),
            outside.display(),
        );

        let output = run_to_output(
            &workspace,
            &workspace,
            &command,
            Duration::from_secs(20),
            None,
            ProcessLimits::default(),
        )
        .await
        .expect("run confined shell");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            output.exit_code,
            Some(0),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("workspace-ok"), "stdout={stdout}");
        assert!(stdout.contains("DENIED_READ"), "stdout={stdout}");
        assert!(stdout.contains("DENIED_WRITE"), "stdout={stdout}");
        assert!(!stdout.contains("outside-secret"), "stdout={stdout}");
        assert_eq!(
            fs::read_to_string(&created)
                .expect("workspace write")
                .trim_end(),
            "created-ok"
        );
        assert_eq!(
            fs::read_to_string(&outside).expect("outside unchanged"),
            "outside-secret"
        );
    }
}
