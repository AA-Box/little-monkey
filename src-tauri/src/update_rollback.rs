//! Rollback for the in-app updater (ROADMAP #8, roadmap K22).
//!
//! The updater replaces the installed app in place, which is what makes it
//! quiet — and what makes a bad release unrecoverable without a download. This
//! module keeps exactly one snapshot of the *currently installed* app, taken
//! immediately before an update is applied, and can put it back.
//!
//! # Why a copy rather than re-downloading the old version
//!
//! Tauri's updater endpoint serves one release: the latest. There is no request
//! that asks it for the version you were happily running an hour ago, and the
//! machine that most needs a rollback is the one whose new build will not start
//! or has no network. A local copy is the only rollback that works when the
//! thing being rolled back is the app itself. The price is disk — one extra
//! copy of the install, reported in bytes by [`update_rollback_status`] so it is the
//! user's choice to keep or discard.
//!
//! # Why a detached script rather than doing it in-process
//!
//! An app cannot overwrite the files it is currently executing from on Windows
//! at all, and can only do it by accident on macOS. So the restore is a small
//! script that waits for this process to exit, swaps the directories, and
//! relaunches. [`restore_script`] builds it as a pure function — it is
//! generated identically on every host and unit-tested for all three, which
//! matters because two of the three cannot be exercised on a developer's
//! machine.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};

/// What kind of thing the installed app *is* on this platform, which decides
/// both how it is copied and how it is relaunched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallKind {
    /// macOS: a `.app` bundle directory, relaunched with `open`.
    MacBundle,
    /// Windows: the installation directory holding the `.exe`.
    WindowsDir,
    /// Linux: a single AppImage file, or the executable itself.
    LinuxFile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackSnapshot {
    /// The app version the snapshot holds — the one a rollback returns to.
    pub version: String,
    pub kind: InstallKind,
    /// Where the snapshot is restored to.
    pub install_root: String,
    /// Where the copy lives.
    pub payload: String,
    /// Executable to relaunch after restoring, absolute.
    pub relaunch: String,
    pub created_at_ms: u64,
    pub size_bytes: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn rollback_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("updates").join("rollback")
}

fn snapshot_file(app_data_dir: &Path) -> PathBuf {
    rollback_dir(app_data_dir).join("snapshot.json")
}

/// Where the running app is installed, in the form this platform can restore.
pub fn current_install(executable: &Path) -> Result<(InstallKind, PathBuf), String> {
    if cfg!(target_os = "macos") {
        let bundle = executable.ancestors().find(|ancestor| {
            ancestor
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        });
        return match bundle {
            Some(bundle) => Ok((InstallKind::MacBundle, bundle.to_path_buf())),
            None => Err(
                "This build is not running from an .app bundle, so there is nothing to snapshot"
                    .to_string(),
            ),
        };
    }
    if cfg!(target_os = "windows") {
        let directory = executable
            .parent()
            .ok_or_else(|| "The running executable has no install directory".to_string())?;
        return Ok((InstallKind::WindowsDir, directory.to_path_buf()));
    }
    // Linux: an AppImage tells us so itself; anything else (a distribution
    // package, a `cargo run` binary) is snapshotted as the single executable,
    // which is the only file the updater would have replaced anyway.
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    Ok((
        InstallKind::LinuxFile,
        appimage.unwrap_or_else(|| executable.to_path_buf()),
    ))
}

/// Copies a file or a directory tree, refusing symlinks.
///
/// A snapshot is restored over the install root with the app's own privileges,
/// so a symlink inside it is a way to write somewhere else entirely. Bundles
/// legitimately contain symlinks (`Contents/Frameworks/*.framework/Versions`),
/// so they are recreated as links rather than followed or rejected.
fn copy_tree(source: &Path, destination: &Path) -> Result<u64, String> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("Failed to inspect {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = std::fs::read_link(source)
                .map_err(|error| format!("Failed to read link {}: {error}", source.display()))?;
            std::os::unix::fs::symlink(&target, destination).map_err(|error| {
                format!("Failed to recreate link {}: {error}", destination.display())
            })?;
            return Ok(0);
        }
        #[cfg(not(unix))]
        return Ok(0);
    }
    if metadata.is_file() {
        std::fs::copy(source, destination).map_err(|error| {
            format!(
                "Failed to copy {} to {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode();
            let _ = std::fs::set_permissions(destination, std::fs::Permissions::from_mode(mode));
        }
        return Ok(metadata.len());
    }
    std::fs::create_dir_all(destination)
        .map_err(|error| format!("Failed to create {}: {error}", destination.display()))?;
    let mut total = 0;
    for entry in std::fs::read_dir(source)
        .map_err(|error| format!("Failed to list {}: {error}", source.display()))?
    {
        let entry =
            entry.map_err(|error| format!("Failed to read {}: {error}", source.display()))?;
        total += copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(total)
}

/// Single-quotes a path for `/bin/sh`, so a space or a `$` in it is inert.
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// The restore script: wait for this process to exit, put the snapshot back,
/// relaunch. Pure so every platform's script is testable from any host.
///
/// Returns the file name to write it as and its contents. The script takes the
/// pid to wait for as its only argument.
pub fn restore_script(snapshot: &RollbackSnapshot) -> (String, String) {
    let install_root = Path::new(&snapshot.install_root);
    let payload = Path::new(&snapshot.payload);
    let relaunch = Path::new(&snapshot.relaunch);
    match snapshot.kind {
        InstallKind::WindowsDir => (
            "rollback.cmd".to_string(),
            format!(
                "@echo off\r\n\
                 setlocal\r\n\
                 set PID=%1\r\n\
                 set TRIES=0\r\n\
                 :wait\r\n\
                 tasklist /FI \"PID eq %PID%\" 2>nul | find \"%PID%\" >nul\r\n\
                 if errorlevel 1 goto restore\r\n\
                 set /a TRIES+=1\r\n\
                 if %TRIES% GEQ 120 goto restore\r\n\
                 timeout /t 1 /nobreak >nul\r\n\
                 goto wait\r\n\
                 :restore\r\n\
                 robocopy \"{payload}\" \"{install}\" /MIR /NFL /NDL /NJH /NJS /NP >nul\r\n\
                 if errorlevel 8 exit /b 1\r\n\
                 start \"\" \"{relaunch}\"\r\n\
                 exit /b 0\r\n",
                payload = payload.display(),
                install = install_root.display(),
                relaunch = relaunch.display(),
            ),
        ),
        kind => {
            // The old install is moved aside rather than deleted first, so a
            // failed copy leaves something to go back to instead of nothing.
            let launch = if kind == InstallKind::MacBundle {
                format!("open {}", sh_quote(install_root))
            } else {
                format!(
                    "chmod +x {relaunch} 2>/dev/null; {relaunch} &",
                    relaunch = sh_quote(relaunch)
                )
            };
            (
                "rollback.sh".to_string(),
                format!(
                    "#!/bin/sh\n\
                     PID=\"$1\"\n\
                     TRIES=0\n\
                     while kill -0 \"$PID\" 2>/dev/null && [ \"$TRIES\" -lt 600 ]; do\n\
                     \x20 sleep 0.2\n\
                     \x20 TRIES=$((TRIES+1))\n\
                     done\n\
                     PREVIOUS={install}.rollback-previous\n\
                     rm -rf \"$PREVIOUS\"\n\
                     mv {install} \"$PREVIOUS\" 2>/dev/null\n\
                     if cp -R {payload} {install}; then\n\
                     \x20 rm -rf \"$PREVIOUS\"\n\
                     else\n\
                     \x20 rm -rf {install}\n\
                     \x20 mv \"$PREVIOUS\" {install}\n\
                     \x20 exit 1\n\
                     fi\n\
                     {launch}\n",
                    install = sh_quote(install_root),
                    payload = sh_quote(payload),
                    launch = launch,
                ),
            )
        }
    }
}

fn read_snapshot(app_data_dir: &Path) -> Option<RollbackSnapshot> {
    let bytes = std::fs::read(snapshot_file(app_data_dir)).ok()?;
    let snapshot: RollbackSnapshot = serde_json::from_slice(&bytes).ok()?;
    Path::new(&snapshot.payload)
        .symlink_metadata()
        .ok()
        .map(|_| snapshot)
}

fn app_data<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve the app data directory: {error}"))
}

/// Snapshots the installed app so the version running right now can be
/// restored. Replaces any earlier snapshot: one rollback step, not a history.
pub fn create_snapshot(
    app_data_dir: &Path,
    executable: &Path,
    version: &str,
) -> Result<RollbackSnapshot, String> {
    let (kind, install_root) = current_install(executable)?;
    let directory = rollback_dir(app_data_dir);
    // Clear the whole directory rather than just the payload: a stale
    // snapshot.json pointing at a half-deleted copy is worse than none.
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("Failed to create {}: {error}", directory.display()))?;

    let payload = directory.join(match kind {
        InstallKind::LinuxFile => install_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "payload".to_string()),
        _ => "payload".to_string(),
    });
    let size_bytes = copy_tree(&install_root, &payload).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&directory);
    })?;

    let relaunch = match kind {
        // The bundle is relaunched with `open`, but the executable is still
        // recorded so a caller can tell what would start.
        InstallKind::MacBundle | InstallKind::LinuxFile => executable.to_path_buf(),
        InstallKind::WindowsDir => install_root.join(
            executable
                .file_name()
                .ok_or_else(|| "The running executable has no file name".to_string())?,
        ),
    };
    let snapshot = RollbackSnapshot {
        version: version.to_string(),
        kind,
        install_root: install_root.to_string_lossy().into_owned(),
        payload: payload.to_string_lossy().into_owned(),
        relaunch: relaunch.to_string_lossy().into_owned(),
        created_at_ms: now_ms(),
        size_bytes,
    };
    let json = serde_json::to_vec_pretty(&snapshot)
        .map_err(|error| format!("Failed to serialize the rollback snapshot: {error}"))?;
    std::fs::write(snapshot_file(app_data_dir), json)
        .map_err(|error| format!("Failed to record the rollback snapshot: {error}"))?;
    Ok(snapshot)
}

/// Writes the restore script and starts it detached. The caller exits after
/// this returns — the script is waiting for exactly that.
fn spawn_restore(app_data_dir: &Path, snapshot: &RollbackSnapshot) -> Result<(), String> {
    let (name, contents) = restore_script(snapshot);
    let script = rollback_dir(app_data_dir).join(name);
    std::fs::write(&script, contents)
        .map_err(|error| format!("Failed to write the rollback script: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Failed to make the rollback script executable: {error}"))?;
    }
    let pid = std::process::id().to_string();
    #[cfg(windows)]
    let spawned = {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NO_WINDOW: the script has to outlive this
        // process and must not flash a console window while it waits.
        std::process::Command::new("cmd")
            .args(["/C"])
            .arg(&script)
            .arg(&pid)
            .creation_flags(0x0000_0008 | 0x0800_0000)
            .spawn()
    };
    #[cfg(not(windows))]
    let spawned = std::process::Command::new("/bin/sh")
        .arg(&script)
        .arg(&pid)
        .spawn();
    spawned
        .map(|_| ())
        .map_err(|error| format!("Failed to start the rollback: {error}"))
}

/// What shape this install is, and whether the in-app updater can replace it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallInfo {
    pub kind: InstallKind,
    pub root: String,
    /// False for a Linux install that is not an AppImage. Tauri's updater
    /// replaces an AppImage file; a `.deb`/`.rpm` install is owned by the
    /// package manager and must be updated through it. Saying so is the
    /// difference between "Linux is covered" and "Linux fails quietly".
    pub self_updatable: bool,
}

/// Whether the updater can replace this install in place.
pub fn self_updatable(kind: InstallKind) -> bool {
    match kind {
        InstallKind::MacBundle | InstallKind::WindowsDir => true,
        // Set by the AppImage runtime itself, and by nothing else.
        InstallKind::LinuxFile => std::env::var_os("APPIMAGE").is_some(),
    }
}

#[tauri::command]
pub async fn update_install_info() -> Result<InstallInfo, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Failed to resolve the running executable: {error}"))?;
    let (kind, root) = current_install(&executable)?;
    Ok(InstallInfo {
        kind,
        root: root.to_string_lossy().into_owned(),
        self_updatable: self_updatable(kind),
    })
}

#[tauri::command]
pub async fn update_snapshot_create<R: Runtime>(
    app: AppHandle<R>,
) -> Result<RollbackSnapshot, String> {
    let app_data_dir = app_data(&app)?;
    let version = app.package_info().version.to_string();
    let executable = std::env::current_exe()
        .map_err(|error| format!("Failed to resolve the running executable: {error}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        create_snapshot(&app_data_dir, &executable, &version)
    })
    .await
    .map_err(|error| format!("Rollback snapshot worker failed: {error}"))?
}

#[tauri::command]
pub async fn update_rollback_status<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<RollbackSnapshot>, String> {
    Ok(read_snapshot(&app_data(&app)?))
}

#[tauri::command]
pub async fn update_rollback_discard<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let directory = rollback_dir(&app_data(&app)?);
    match std::fs::remove_dir_all(&directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to discard the rollback snapshot: {error}")),
    }
}

/// Restores the snapshot and relaunches. Returns only if the restore could not
/// be *started*; on success the app exits and the script takes over.
#[tauri::command]
pub async fn update_rollback_apply<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let app_data_dir = app_data(&app)?;
    let snapshot = read_snapshot(&app_data_dir)
        .ok_or_else(|| "There is no rollback snapshot to restore".to_string())?;
    spawn_restore(&app_data_dir, &snapshot)?;
    // The script polls for this pid, so exiting is the handoff.
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    /// A private directory per test — the crate's convention (see
    /// `test_support`'s module doc): never the shared app-data root.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("lm-rollback-{name}-{}", Uuid::new_v4().simple()));
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot(
        kind: InstallKind,
        install: &str,
        payload: &str,
        relaunch: &str,
    ) -> RollbackSnapshot {
        RollbackSnapshot {
            version: "1.2.0".to_string(),
            kind,
            install_root: install.to_string(),
            payload: payload.to_string(),
            relaunch: relaunch.to_string(),
            created_at_ms: 0,
            size_bytes: 0,
        }
    }

    #[test]
    fn a_snapshot_round_trips_a_directory_and_can_be_restored_from() {
        let root = Scratch::new("roundtrip");
        let app_data = root.path().join("data");
        let install = root.path().join("Little Monkey.app");
        fs::create_dir_all(install.join("Contents/MacOS")).unwrap();
        fs::write(install.join("Contents/MacOS/app"), b"original build").unwrap();
        fs::create_dir_all(&app_data).unwrap();

        let executable = install.join("Contents/MacOS/app");
        let snapshot = create_snapshot(&app_data, &executable, "1.2.0").unwrap();
        assert_eq!(snapshot.version, "1.2.0");
        // What the install root *is* differs per platform (bundle, install
        // directory, single file), so the portable assertion is that the
        // snapshot holds the bytes that were installed and can be found again.
        assert_eq!(snapshot.size_bytes, b"original build".len() as u64);
        assert!(Path::new(&snapshot.payload).exists());
        assert!(read_snapshot(&app_data).is_some());
        #[cfg(target_os = "macos")]
        {
            assert_eq!(snapshot.kind, InstallKind::MacBundle);
            assert_eq!(snapshot.install_root, install.to_string_lossy());
            assert_eq!(
                fs::read(Path::new(&snapshot.payload).join("Contents/MacOS/app")).unwrap(),
                b"original build"
            );
        }

        // A second snapshot replaces the first rather than accumulating.
        fs::write(install.join("Contents/MacOS/app"), b"newer build").unwrap();
        let second = create_snapshot(&app_data, &executable, "1.3.0").unwrap();
        assert_eq!(second.version, "1.3.0");
        assert_eq!(
            fs::read_dir(rollback_dir(&app_data)).unwrap().count(),
            2,
            "one payload plus snapshot.json"
        );
    }

    #[test]
    fn a_missing_payload_is_not_a_restorable_snapshot() {
        let root = Scratch::new("missing-payload");
        let app_data = root.path().join("data");
        let install = root.path().join("app");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("binary"), b"x").unwrap();
        fs::create_dir_all(&app_data).unwrap();

        let snapshot = create_snapshot(&app_data, &install.join("binary"), "1.2.0");
        // macOS refuses a non-bundle install root outright; every other host
        // snapshots something. Both are correct answers, and neither may leave
        // a snapshot behind that points at a payload that is not there.
        if snapshot.is_ok() {
            fs::remove_dir_all(Path::new(&snapshot.unwrap().payload).parent().unwrap()).unwrap();
        }
        assert!(read_snapshot(&app_data).is_none());
    }

    #[test]
    fn the_unix_script_waits_for_the_pid_and_keeps_the_old_install_until_the_copy_lands() {
        let script = restore_script(&snapshot(
            InstallKind::MacBundle,
            "/Applications/Little Monkey.app",
            "/data/updates/rollback/payload",
            "/Applications/Little Monkey.app/Contents/MacOS/little-monkey",
        ));
        assert_eq!(script.0, "rollback.sh");
        let body = script.1;
        assert!(body.contains("kill -0 \"$PID\""), "{body}");
        assert!(body.contains("'/Applications/Little Monkey.app'"), "{body}");
        assert!(
            body.contains("mv \"$PREVIOUS\"") && body.contains("exit 1"),
            "a failed copy must restore what it moved aside: {body}"
        );
        assert!(
            body.contains("open '/Applications/Little Monkey.app'"),
            "{body}"
        );
    }

    #[test]
    fn the_linux_script_relaunches_the_appimage_itself() {
        let (name, body) = restore_script(&snapshot(
            InstallKind::LinuxFile,
            "/opt/little-monkey.AppImage",
            "/data/updates/rollback/little-monkey.AppImage",
            "/opt/little-monkey.AppImage",
        ));
        assert_eq!(name, "rollback.sh");
        assert!(
            body.contains("chmod +x '/opt/little-monkey.AppImage'"),
            "{body}"
        );
        assert!(!body.contains("open '"), "Linux has no `open`: {body}");
    }

    #[test]
    fn the_windows_script_mirrors_the_install_directory_and_restarts_the_exe() {
        let (name, body) = restore_script(&snapshot(
            InstallKind::WindowsDir,
            "C:\\Program Files\\Little Monkey",
            "C:\\data\\updates\\rollback\\payload",
            "C:\\Program Files\\Little Monkey\\little-monkey.exe",
        ));
        assert_eq!(name, "rollback.cmd");
        assert!(body.contains("tasklist /FI \"PID eq %PID%\""), "{body}");
        assert!(
            body.contains("robocopy \"C:\\data\\updates\\rollback\\payload\" \"C:\\Program Files\\Little Monkey\" /MIR"),
            "{body}"
        );
        assert!(
            body.contains("if errorlevel 8 exit /b 1"),
            "robocopy's success codes are below 8: {body}"
        );
        assert!(
            body.contains("start \"\" \"C:\\Program Files\\Little Monkey\\little-monkey.exe\""),
            "{body}"
        );
    }

    fn invoke_request(cmd: &str) -> tauri::webview::InvokeRequest {
        tauri::webview::InvokeRequest {
            cmd: cmd.to_string(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: if cfg!(any(windows, target_os = "android")) {
                "http://tauri.localhost"
            } else {
                "tauri://localhost"
            }
            .parse()
            .unwrap(),
            body: tauri::ipc::InvokeBody::Json(serde_json::json!({})),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.to_string(),
        }
    }

    /// The panel calls these three over IPC and nothing else does, so an
    /// argument the macro cannot match, or a command missing from the handler
    /// list, would only show up in a running desktop build. Round-trip them.
    #[test]
    fn the_panels_commands_answer_over_ipc() {
        let app = crate::test_support::build(tauri::test::mock_builder().invoke_handler(
            tauri::generate_handler![
                update_install_info,
                update_rollback_status,
                update_rollback_discard,
                crate::self_integrity::self_integrity_report
            ],
        ));
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();

        // No snapshot has been taken in this app's private data directory.
        let status =
            tauri::test::get_ipc_response(&webview, invoke_request("update_rollback_status"))
                .expect("update_rollback_status answers");
        assert!(status
            .deserialize::<Option<RollbackSnapshot>>()
            .unwrap()
            .is_none());

        // Discarding nothing is not an error — the panel's button is safe to
        // press twice.
        tauri::test::get_ipc_response(&webview, invoke_request("update_rollback_discard"))
            .expect("update_rollback_discard answers");

        // The install shape is only unanswerable on macOS, where a test binary
        // is not inside an .app bundle; everywhere else it must classify.
        let install =
            tauri::test::get_ipc_response(&webview, invoke_request("update_install_info"));
        if cfg!(target_os = "macos") {
            assert!(install.is_err(), "a non-bundle macOS install has no root");
        } else {
            let info = install.unwrap().deserialize::<InstallInfo>().unwrap();
            assert!(!info.root.is_empty());
        }

        let report =
            tauri::test::get_ipc_response(&webview, invoke_request("self_integrity_report"))
                .expect("self_integrity_report answers")
                .deserialize::<crate::self_integrity::IntegrityReport>()
                .unwrap();
        assert_eq!(report.components.len(), 4, "signature plus three runtimes");
        assert!(!report.refused, "a source build must not refuse");
    }

    #[test]
    fn a_quote_in_a_path_cannot_break_out_of_the_script() {
        let (_, body) = restore_script(&snapshot(
            InstallKind::LinuxFile,
            "/opt/it's here/app",
            "/data/payload",
            "/opt/it's here/app",
        ));
        assert!(body.contains("'/opt/it'\\''s here/app'"), "{body}");
    }
}
