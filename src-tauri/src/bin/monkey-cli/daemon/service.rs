use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use little_monkey_lib::app_paths::AgentConfigRoots;

#[cfg(test)]
use super::store::DaemonStore;
use super::store::{restrict_file, DaemonConfig, DaemonPaths};

const LAUNCHD_LABEL: &str = "com.littlemonkey.daemon";
const SYSTEMD_UNIT: &str = "little-monkey-daemon.service";
const WINDOWS_TASK: &str = "LittleMonkeyDaemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServicePlatform {
    // Each variant is only constructed by `current()` on its own platform
    // (tests render every manifest), so the other platforms' builds see it
    // as never constructed.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Launchd,
    #[cfg_attr(any(not(unix), target_os = "macos"), allow(dead_code))]
    SystemdUser,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    WindowsTask,
}

impl ServicePlatform {
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Launchd
        }
        #[cfg(target_os = "windows")]
        {
            Self::WindowsTask
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Self::SystemdUser
        }
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> Result<Output, String>;
}

pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output, String> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|error| format!("Failed to run {program}: {error}"))
    }
}

trait ServiceHealthChecker: Send + Sync {
    fn wait_until_healthy(
        &self,
        paths: &DaemonPaths,
        profile_id: &str,
        newer_than_ms: u64,
        previous_pid: Option<u32>,
    ) -> Result<(), String>;
}

struct StoreHealthChecker;

impl ServiceHealthChecker for StoreHealthChecker {
    fn wait_until_healthy(
        &self,
        paths: &DaemonPaths,
        profile_id: &str,
        newer_than_ms: u64,
        previous_pid: Option<u32>,
    ) -> Result<(), String> {
        const ATTEMPTS: usize = 100;
        const POLL_INTERVAL: Duration = Duration::from_millis(100);

        for attempt in 0..ATTEMPTS {
            if let Some(state) = live_daemon_state(paths) {
                if state.profile_id.as_deref() == Some(profile_id)
                    && state.heartbeat_ms > newer_than_ms
                    && Some(state.pid) != previous_pid
                {
                    return Ok(());
                }
            }
            if attempt + 1 < ATTEMPTS {
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        Err("Scoped daemon did not publish a fresh heartbeat within 10 seconds".into())
    }
}

struct OwnedLegacyService {
    manifest_path: PathBuf,
    manifest_contents: String,
    was_running: bool,
}

struct CurrentServiceSnapshot {
    manifest_path: PathBuf,
    manifest_contents: Option<String>,
    was_registered: bool,
    was_running: bool,
}

#[derive(Clone, Copy)]
enum FixedServiceRuntime {
    Inactive,
    Running,
}

enum FixedServiceOwnership {
    Owned,
    Other,
    Ambiguous,
}

pub struct ServiceManager<R> {
    runner: R,
    platform: ServicePlatform,
    home: PathBuf,
    executable: PathBuf,
    agent_home: PathBuf,
    profile_id: String,
    registry_active_id: String,
    health_checker: Box<dyn ServiceHealthChecker>,
}

impl<R: CommandRunner> ServiceManager<R> {
    fn new(
        runner: R,
        platform: ServicePlatform,
        home: PathBuf,
        executable: PathBuf,
        roots: AgentConfigRoots,
    ) -> Result<Self, String> {
        Self::new_with_health_checker(
            runner,
            platform,
            home,
            executable,
            roots,
            Box::new(StoreHealthChecker),
        )
    }

    fn new_with_health_checker(
        runner: R,
        platform: ServicePlatform,
        home: PathBuf,
        executable: PathBuf,
        roots: AgentConfigRoots,
        health_checker: Box<dyn ServiceHealthChecker>,
    ) -> Result<Self, String> {
        little_monkey_lib::profiles::validate_id(&roots.profile_id)
            .map_err(|error| error.to_string())?;
        little_monkey_lib::profiles::validate_id(&roots.registry_active_id)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            runner,
            platform,
            home,
            executable,
            agent_home: roots.agent_home,
            profile_id: roots.profile_id,
            registry_active_id: roots.registry_active_id,
            health_checker,
        })
    }

    pub fn real(roots: AgentConfigRoots) -> Result<ServiceManager<RealCommandRunner>, String> {
        let home =
            dirs::home_dir().ok_or_else(|| "Could not resolve the home directory".to_string())?;
        let executable = std::env::current_exe()
            .map_err(|error| format!("Could not resolve monkey executable: {error}"))?;
        ServiceManager::new(
            RealCommandRunner,
            ServicePlatform::current(),
            home,
            executable,
            roots,
        )
    }

    pub fn platform(&self) -> ServicePlatform {
        self.platform
    }

    fn is_default_profile(&self) -> bool {
        self.profile_id == little_monkey_lib::profiles::DEFAULT_PROFILE_ID
    }

    fn launchd_label(&self) -> String {
        if self.is_default_profile() {
            LAUNCHD_LABEL.to_string()
        } else {
            format!("{LAUNCHD_LABEL}.{}", self.profile_id)
        }
    }

    fn systemd_unit(&self) -> String {
        if self.is_default_profile() {
            SYSTEMD_UNIT.to_string()
        } else {
            format!("little-monkey-daemon-{}.service", self.profile_id)
        }
    }

    fn windows_task(&self) -> String {
        if self.is_default_profile() {
            WINDOWS_TASK.to_string()
        } else {
            format!("{WINDOWS_TASK}-{}", self.profile_id)
        }
    }

    fn windows_manifest_name(&self) -> String {
        if self.is_default_profile() {
            "little-monkey-daemon-task.xml".to_string()
        } else {
            format!("little-monkey-daemon-task-{}.xml", self.profile_id)
        }
    }

    pub fn manifest_path(&self, paths: &DaemonPaths) -> PathBuf {
        match self.platform {
            ServicePlatform::Launchd => self
                .home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{}.plist", self.launchd_label())),
            ServicePlatform::SystemdUser => self
                .home
                .join(".config")
                .join("systemd")
                .join("user")
                .join(self.systemd_unit()),
            ServicePlatform::WindowsTask => paths.root.join(self.windows_manifest_name()),
        }
    }

    fn legacy_manifest_path(&self, paths: &DaemonPaths) -> PathBuf {
        match self.platform {
            ServicePlatform::Launchd => self
                .home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            ServicePlatform::SystemdUser => self
                .home
                .join(".config")
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT),
            ServicePlatform::WindowsTask => paths.root.join("little-monkey-daemon-task.xml"),
        }
    }

    fn legacy_manifest_targets_profile(&self, manifest: &str) -> bool {
        match self.platform {
            ServicePlatform::Launchd => {
                let compact: String = manifest.chars().filter(|ch| !ch.is_whitespace()).collect();
                compact.contains(&format!(
                    "<string>--profile</string><string>{}</string>",
                    xml_escape(&self.profile_id)
                ))
            }
            ServicePlatform::SystemdUser => {
                manifest.contains(&format!("--profile={}", systemd_escape(&self.profile_id)))
            }
            ServicePlatform::WindowsTask => {
                let profile_arg = format!("--profile {}", windows_quote(&self.profile_id));
                manifest.contains(&profile_arg) || manifest.contains(&xml_escape(&profile_arg))
            }
        }
    }

    fn fixed_service_ownership(
        &self,
        manifest: &str,
        paths: &DaemonPaths,
        runtime: FixedServiceRuntime,
    ) -> FixedServiceOwnership {
        if self.legacy_manifest_targets_profile(manifest) {
            return FixedServiceOwnership::Owned;
        }
        if manifest.contains("--profile") {
            return FixedServiceOwnership::Other;
        }
        match runtime {
            FixedServiceRuntime::Running => {
                if self.selected_profile_has_live_daemon(paths) {
                    FixedServiceOwnership::Owned
                } else {
                    FixedServiceOwnership::Ambiguous
                }
            }
            FixedServiceRuntime::Inactive if self.registry_active_id == self.profile_id => {
                FixedServiceOwnership::Owned
            }
            FixedServiceRuntime::Inactive => FixedServiceOwnership::Other,
        }
    }

    fn selected_profile_has_live_daemon(&self, paths: &DaemonPaths) -> bool {
        let Some(marker) = live_daemon_profile(paths) else {
            return false;
        };
        if let Some(marker) = marker {
            return marker == self.profile_id;
        }
        let Some(profile_root) = paths.root.parent() else {
            return false;
        };
        let data_base = if self.is_default_profile() {
            profile_root.to_path_buf()
        } else {
            let Some(profiles_dir) = profile_root.parent() else {
                return false;
            };
            let Some(base) = profiles_dir.parent() else {
                return false;
            };
            base.to_path_buf()
        };
        let default_paths = DaemonPaths::under(&data_base);
        if default_paths.root != paths.root && live_daemon_profile(&default_paths).is_some() {
            return false;
        }
        let profiles_dir = data_base.join(little_monkey_lib::profiles::PROFILES_DIR);
        let entries = match std::fs::read_dir(profiles_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        };
        for entry in entries.flatten() {
            let candidate = DaemonPaths::under(&entry.path());
            if candidate.root != paths.root && live_daemon_profile(&candidate).is_some() {
                return false;
            }
        }
        true
    }

    fn current_service_is_owned(&self, paths: &DaemonPaths) -> Result<bool, String> {
        if !self.is_default_profile() {
            return Ok(true);
        }
        let manifest_path = self.manifest_path(paths);
        let (contents, runtime) = match self.platform {
            ServicePlatform::Launchd | ServicePlatform::SystemdUser => {
                let contents = match std::fs::read_to_string(&manifest_path) {
                    Ok(contents) => contents,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                    Err(error) => {
                        return Err(format!("Failed to read service manifest: {error}"));
                    }
                };
                let runtime = if self.current_status()? {
                    FixedServiceRuntime::Running
                } else {
                    FixedServiceRuntime::Inactive
                };
                (contents, runtime)
            }
            ServicePlatform::WindowsTask => {
                let query = self.runner.run(
                    "schtasks",
                    &[
                        "/Query".into(),
                        "/TN".into(),
                        WINDOWS_TASK.into(),
                        "/XML".into(),
                    ],
                )?;
                if query.status.success() {
                    (
                        command_output_text(&query.stdout),
                        if self.windows_task_running(WINDOWS_TASK)? {
                            FixedServiceRuntime::Running
                        } else {
                            FixedServiceRuntime::Inactive
                        },
                    )
                } else {
                    let contents = match std::fs::read_to_string(&manifest_path) {
                        Ok(contents) => contents,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return Ok(true)
                        }
                        Err(error) => {
                            return Err(format!("Failed to read service manifest: {error}"));
                        }
                    };
                    (contents, FixedServiceRuntime::Inactive)
                }
            }
        };
        match self.fixed_service_ownership(&contents, paths, runtime) {
            FixedServiceOwnership::Owned => Ok(true),
            FixedServiceOwnership::Other => Ok(false),
            FixedServiceOwnership::Ambiguous => Err(format!(
                "Cannot prove the running fixed service belongs to profile '{}'; refusing to modify it",
                self.profile_id
            )),
        }
    }

    fn ensure_current_service_is_owned(&self, paths: &DaemonPaths) -> Result<(), String> {
        if self.current_service_is_owned(paths)? {
            Ok(())
        } else {
            Err(format!(
                "The fixed service identifier is owned by profile '{}'; refusing to modify it as '{}'",
                self.registry_active_id, self.profile_id
            ))
        }
    }

    fn snapshot_current(&self, paths: &DaemonPaths) -> Result<CurrentServiceSnapshot, String> {
        let manifest_path = self.manifest_path(paths);
        let mut manifest_contents = match std::fs::read_to_string(&manifest_path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Failed to snapshot service manifest: {error}")),
        };

        let (was_registered, was_running) = match self.platform {
            ServicePlatform::Launchd => {
                let running = self.current_status()?;
                (running, running)
            }
            ServicePlatform::SystemdUser => {
                let running = self.current_status()?;
                let enabled = self
                    .runner
                    .run(
                        "systemctl",
                        &[
                            "--user".into(),
                            "is-enabled".into(),
                            "--quiet".into(),
                            self.systemd_unit(),
                        ],
                    )?
                    .status
                    .success();
                (enabled, running)
            }
            ServicePlatform::WindowsTask => {
                let query = self.runner.run(
                    "schtasks",
                    &[
                        "/Query".into(),
                        "/TN".into(),
                        self.windows_task(),
                        "/XML".into(),
                    ],
                )?;
                if query.status.success() {
                    manifest_contents = Some(normalize_windows_task_xml(&command_output_text(
                        &query.stdout,
                    )));
                    (true, self.windows_task_running(&self.windows_task())?)
                } else {
                    (false, false)
                }
            }
        };
        if manifest_contents.is_none() && (was_registered || was_running) {
            return Err("Cannot safely replace a registered service without its manifest".into());
        }
        Ok(CurrentServiceSnapshot {
            manifest_path,
            manifest_contents,
            was_registered,
            was_running,
        })
    }

    fn restore_current(&self, snapshot: &CurrentServiceSnapshot) -> Result<(), String> {
        if let Some(contents) = snapshot.manifest_contents.as_deref() {
            publish_service_manifest(&snapshot.manifest_path, contents)?;
        }
        match self.platform {
            ServicePlatform::Launchd => {
                if snapshot.was_registered {
                    checked(
                        self.runner.run(
                            "launchctl",
                            &[
                                "bootstrap".into(),
                                format!("gui/{}", self.user_id()?),
                                snapshot.manifest_path.display().to_string(),
                            ],
                        )?,
                        "launchctl bootstrap previous service",
                    )?;
                }
            }
            ServicePlatform::SystemdUser => {
                if snapshot.manifest_contents.is_some() {
                    checked(
                        self.runner
                            .run("systemctl", &["--user".into(), "daemon-reload".into()])?,
                        "systemctl --user daemon-reload previous service",
                    )?;
                }
                if snapshot.was_registered {
                    checked(
                        self.runner.run(
                            "systemctl",
                            &["--user".into(), "enable".into(), self.systemd_unit()],
                        )?,
                        "systemctl --user enable previous service",
                    )?;
                }
                if snapshot.was_running {
                    self.start_current()?;
                }
            }
            ServicePlatform::WindowsTask => {
                if snapshot.was_registered {
                    checked(
                        self.runner.run(
                            "schtasks",
                            &[
                                "/Create".into(),
                                "/TN".into(),
                                self.windows_task(),
                                "/XML".into(),
                                snapshot.manifest_path.display().to_string(),
                                "/F".into(),
                            ],
                        )?,
                        "schtasks /Create previous task",
                    )?;
                }
                if snapshot.was_running {
                    self.start_current()?;
                }
            }
        }
        Ok(())
    }

    fn owned_legacy_service(
        &self,
        paths: &DaemonPaths,
    ) -> Result<Option<OwnedLegacyService>, String> {
        if self.is_default_profile() {
            return Ok(None);
        }

        let manifest_path = self.legacy_manifest_path(paths);
        let (contents, runtime) = match self.platform {
            ServicePlatform::Launchd | ServicePlatform::SystemdUser => {
                let contents = match std::fs::read_to_string(&manifest_path) {
                    Ok(contents) => contents,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => {
                        return Err(format!("Failed to read legacy service manifest: {error}"));
                    }
                };
                let runtime = if self.legacy_status()? {
                    FixedServiceRuntime::Running
                } else {
                    FixedServiceRuntime::Inactive
                };
                (contents, runtime)
            }
            ServicePlatform::WindowsTask => {
                let query = self.runner.run(
                    "schtasks",
                    &[
                        "/Query".into(),
                        "/TN".into(),
                        WINDOWS_TASK.into(),
                        "/XML".into(),
                    ],
                )?;
                if !query.status.success() {
                    return Ok(None);
                }
                let contents = normalize_windows_task_xml(&command_output_text(&query.stdout));
                // `/End` reports whether the task was running during the stop
                // step, so a manifest that already names this profile decides
                // ownership on its own and needs no extra query.
                let runtime = if self.legacy_manifest_targets_profile(&contents) {
                    FixedServiceRuntime::Inactive
                } else if self.windows_task_running(WINDOWS_TASK)? {
                    FixedServiceRuntime::Running
                } else {
                    FixedServiceRuntime::Inactive
                };
                (contents, runtime)
            }
        };
        match self.fixed_service_ownership(&contents, paths, runtime) {
            FixedServiceOwnership::Owned => {}
            FixedServiceOwnership::Other => return Ok(None),
            FixedServiceOwnership::Ambiguous => {
                return Err(format!(
                    "Cannot prove the running fixed service belongs to profile '{}'; refusing to modify it",
                    self.profile_id
                ))
            }
        }
        Ok(Some(OwnedLegacyService {
            manifest_path,
            manifest_contents: contents,
            was_running: matches!(runtime, FixedServiceRuntime::Running),
        }))
    }

    fn legacy_status(&self) -> Result<bool, String> {
        let output = match self.platform {
            ServicePlatform::Launchd => self.runner.run(
                "launchctl",
                &[
                    "print".into(),
                    format!("gui/{}/{LAUNCHD_LABEL}", self.user_id()?),
                ],
            )?,
            ServicePlatform::SystemdUser => self.runner.run(
                "systemctl",
                &[
                    "--user".into(),
                    "is-active".into(),
                    "--quiet".into(),
                    SYSTEMD_UNIT.into(),
                ],
            )?,
            ServicePlatform::WindowsTask => self.runner.run(
                "schtasks",
                &["/Query".into(), "/TN".into(), WINDOWS_TASK.into()],
            )?,
        };
        Ok(output.status.success())
    }

    fn stop_legacy(&self, legacy: &OwnedLegacyService) -> Result<bool, String> {
        if self.platform == ServicePlatform::WindowsTask {
            let output = self.runner.run(
                "schtasks",
                &["/End".into(), "/TN".into(), WINDOWS_TASK.into()],
            )?;
            return Ok(output.status.success());
        }
        if !legacy.was_running {
            return Ok(false);
        }
        match self.platform {
            ServicePlatform::Launchd => checked(
                self.runner.run(
                    "launchctl",
                    &[
                        "bootout".into(),
                        format!("gui/{}", self.user_id()?),
                        legacy.manifest_path.display().to_string(),
                    ],
                )?,
                "launchctl bootout legacy service",
            )?,
            ServicePlatform::SystemdUser => checked(
                self.runner.run(
                    "systemctl",
                    &["--user".into(), "stop".into(), SYSTEMD_UNIT.into()],
                )?,
                "systemctl --user stop legacy service",
            )?,
            ServicePlatform::WindowsTask => unreachable!(),
        };
        Ok(true)
    }

    fn restart_legacy(&self, legacy: &OwnedLegacyService) -> Result<(), String> {
        match self.platform {
            ServicePlatform::Launchd => checked(
                self.runner.run(
                    "launchctl",
                    &[
                        "bootstrap".into(),
                        format!("gui/{}", self.user_id()?),
                        legacy.manifest_path.display().to_string(),
                    ],
                )?,
                "launchctl bootstrap legacy service",
            )
            .map(|_| ()),
            ServicePlatform::SystemdUser => checked(
                self.runner.run(
                    "systemctl",
                    &["--user".into(), "start".into(), SYSTEMD_UNIT.into()],
                )?,
                "systemctl --user start legacy service",
            )
            .map(|_| ()),
            ServicePlatform::WindowsTask => checked(
                self.runner.run(
                    "schtasks",
                    &["/Run".into(), "/TN".into(), WINDOWS_TASK.into()],
                )?,
                "schtasks /Run legacy task",
            )
            .map(|_| ()),
        }
    }

    fn restore_legacy(
        &self,
        legacy: &OwnedLegacyService,
        restart: bool,
        was_registered: bool,
    ) -> Result<(), String> {
        let manifest_unchanged = std::fs::read_to_string(&legacy.manifest_path)
            .is_ok_and(|contents| contents == legacy.manifest_contents);
        if !manifest_unchanged {
            publish_service_manifest(&legacy.manifest_path, &legacy.manifest_contents)?;
        }
        match self.platform {
            ServicePlatform::Launchd => {
                if restart {
                    self.restart_legacy(legacy)?;
                }
            }
            ServicePlatform::SystemdUser => {
                checked(
                    self.runner
                        .run("systemctl", &["--user".into(), "daemon-reload".into()])?,
                    "systemctl --user daemon-reload legacy service",
                )?;
                if was_registered {
                    checked(
                        self.runner.run(
                            "systemctl",
                            &["--user".into(), "enable".into(), SYSTEMD_UNIT.into()],
                        )?,
                        "systemctl --user enable legacy service",
                    )?;
                }
                if restart {
                    self.restart_legacy(legacy)?;
                }
            }
            ServicePlatform::WindowsTask => {
                if was_registered {
                    checked(
                        self.runner.run(
                            "schtasks",
                            &[
                                "/Create".into(),
                                "/TN".into(),
                                WINDOWS_TASK.into(),
                                "/XML".into(),
                                legacy.manifest_path.display().to_string(),
                                "/F".into(),
                            ],
                        )?,
                        "schtasks /Create legacy task",
                    )?;
                }
                if restart {
                    self.restart_legacy(legacy)?;
                }
            }
        }
        Ok(())
    }

    fn retire_legacy(&self, legacy: &OwnedLegacyService) -> Result<(), String> {
        match self.platform {
            ServicePlatform::Launchd => {}
            ServicePlatform::SystemdUser => {
                checked(
                    self.runner.run(
                        "systemctl",
                        &["--user".into(), "disable".into(), SYSTEMD_UNIT.into()],
                    )?,
                    "systemctl --user disable legacy service",
                )?;
            }
            ServicePlatform::WindowsTask => {
                if self.legacy_status()? {
                    checked(
                        self.runner.run(
                            "schtasks",
                            &[
                                "/Delete".into(),
                                "/TN".into(),
                                WINDOWS_TASK.into(),
                                "/F".into(),
                            ],
                        )?,
                        "schtasks /Delete legacy task",
                    )?;
                }
            }
        }
        remove_manifest(&legacy.manifest_path, "legacy service manifest")?;
        if self.platform == ServicePlatform::SystemdUser {
            checked(
                self.runner
                    .run("systemctl", &["--user".into(), "daemon-reload".into()])?,
                "systemctl --user daemon-reload",
            )?;
        }
        Ok(())
    }

    pub fn render_manifest(&self, paths: &DaemonPaths) -> Result<String, String> {
        let executable = utf8_path(&self.executable, "monkey executable")?;
        let agent_home = utf8_path(&self.agent_home, "agent home")?;
        let user_home = utf8_path(&self.home, "user home")?;
        Ok(match self.platform {
            ServicePlatform::Launchd => {
                let launchd_label = self.launchd_label();
                let stdout = paths.logs.join("service.stdout.log");
                let stderr = paths.logs.join("service.stderr.log");
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{launchd_label}</string>\n  <key>ProgramArguments</key>\n  <array><string>{}</string><string>--profile</string><string>{}</string><string>daemon</string><string>serve</string></array>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>LITTLE_MONKEY_HOME</key><string>{}</string>\n    <key>HOME</key><string>{}</string>\n  </dict>\n  <key>WorkingDirectory</key><string>{}</string>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ProcessType</key><string>Background</string>\n  <key>StandardOutPath</key><string>{}</string>\n  <key>StandardErrorPath</key><string>{}</string>\n</dict>\n</plist>\n",
                    xml_escape(&executable),
                    xml_escape(&self.profile_id),
                    xml_escape(&agent_home),
                    xml_escape(user_home),
                    xml_escape(user_home),
                    xml_escape(utf8_path(&stdout, "service stdout log")?),
                    xml_escape(utf8_path(&stderr, "service stderr log")?),
                )
            }
            ServicePlatform::SystemdUser => format!(
                "[Unit]\nDescription=Little Monkey durable local agent daemon\nAfter=network-online.target\n\n[Service]\nType=simple\nEnvironment={}\nExecStart={} --profile={} daemon serve\nRestart=on-failure\nRestartSec=2\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=full\nWorkingDirectory={}\n\n[Install]\nWantedBy=default.target\n",
                systemd_environment_escape(&format!("LITTLE_MONKEY_HOME={agent_home}")),
                systemd_escape(&executable),
                systemd_escape(&self.profile_id),
                systemd_escape(user_home),
            ),
            ServicePlatform::WindowsTask => {
                let script = format!(
                    "$env:LITTLE_MONKEY_HOME={}; & {} --profile {} daemon serve; exit $LASTEXITCODE",
                    powershell_single_quote(&agent_home),
                    powershell_single_quote(&executable),
                    powershell_single_quote(&self.profile_id),
                );
                let encoded = powershell_encoded_command(&script);
                let arguments = format!("-NoProfile -NonInteractive -EncodedCommand {encoded}");
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\n  <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>\n  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>\n  <Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>{}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>\n</Task>\n",
                    "powershell.exe",
                    xml_escape(&arguments),
                    xml_escape(utf8_path(&paths.root, "daemon state root")?),
                )
            }
        })
    }

    pub fn install(&self, paths: &DaemonPaths, config: &DaemonConfig) -> Result<PathBuf, String> {
        self.ensure_current_service_is_owned(paths)?;
        let previous_daemon_pid = live_daemon_state(paths).map(|state| state.pid);
        let current = self.snapshot_current(paths)?;
        let legacy = self.owned_legacy_service(paths)?;
        let legacy_was_registered = match (self.platform, legacy.as_ref()) {
            (_, None) => false,
            (ServicePlatform::Launchd, Some(legacy)) => legacy.was_running,
            (ServicePlatform::WindowsTask, Some(_)) => true,
            (ServicePlatform::SystemdUser, Some(_)) => self
                .runner
                .run(
                    "systemctl",
                    &[
                        "--user".into(),
                        "is-enabled".into(),
                        "--quiet".into(),
                        SYSTEMD_UNIT.into(),
                    ],
                )?
                .status
                .success(),
        };
        config.save(paths)?;
        let manifest = self.manifest_path(paths);
        let rendered = self.render_manifest(paths)?;
        publish_service_manifest(&manifest, &rendered)?;

        if current.was_running {
            if let Err(error) = self.stop_current(paths) {
                let rollback = self.restore_previous_current(paths, &current);
                return Err(transaction_error(error, rollback.err()));
            }
        }
        let legacy_was_running = match legacy.as_ref() {
            Some(legacy) => match self.stop_legacy(legacy) {
                Ok(was_running) => was_running,
                Err(error) => {
                    let mut rollback_errors = Vec::new();
                    if let Err(rollback) = self.restore_previous_current(paths, &current) {
                        rollback_errors.push(rollback);
                    }
                    match self.legacy_status() {
                        Ok(true) => {}
                        Ok(false) => {
                            if let Err(restart) = self.restart_legacy(legacy) {
                                rollback_errors.push(restart);
                            }
                        }
                        Err(status) => rollback_errors.push(status),
                    }
                    return Err(transaction_error_many(error, rollback_errors));
                }
            },
            None => false,
        };
        let activation_started_at = epoch_ms()?;
        let activation = self.activate_current(paths).and_then(|()| {
            if self.current_status()? {
                self.health_checker.wait_until_healthy(
                    paths,
                    &self.profile_id,
                    activation_started_at,
                    previous_daemon_pid,
                )
            } else {
                Err("Scoped service did not become active".to_string())
            }
        });
        if let Err(activation_error) = activation {
            let mut rollback_errors = Vec::new();
            if let Err(error) = self.restore_previous_current(paths, &current) {
                rollback_errors.push(error);
            }
            if legacy_was_running {
                if let Some(legacy) = legacy.as_ref() {
                    if let Err(error) = self.restart_legacy(legacy) {
                        rollback_errors.push(error);
                    }
                }
            }
            return Err(transaction_error_many(activation_error, rollback_errors));
        }
        if let Some(legacy) = legacy.as_ref() {
            if let Err(retirement_error) = self.retire_legacy(legacy) {
                let mut rollback_errors = Vec::new();
                if let Err(error) = self.restore_previous_current(paths, &current) {
                    rollback_errors.push(error);
                }
                if let Err(error) =
                    self.restore_legacy(legacy, legacy_was_running, legacy_was_registered)
                {
                    rollback_errors.push(error);
                }
                return Err(transaction_error_many(retirement_error, rollback_errors));
            }
        }
        Ok(manifest)
    }

    fn restore_previous_current(
        &self,
        paths: &DaemonPaths,
        snapshot: &CurrentServiceSnapshot,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Err(error) = self.rollback_current(paths) {
            errors.push(error);
        }
        if let Err(error) = self.restore_current(snapshot) {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn activate_current(&self, paths: &DaemonPaths) -> Result<(), String> {
        let manifest = self.manifest_path(paths);
        match self.platform {
            ServicePlatform::Launchd => {
                let domain = format!("gui/{}", self.user_id()?);
                let _ = self.runner.run(
                    "launchctl",
                    &[
                        "bootout".into(),
                        domain.clone(),
                        manifest.display().to_string(),
                    ],
                );
                checked(
                    self.runner.run(
                        "launchctl",
                        &["bootstrap".into(), domain, manifest.display().to_string()],
                    )?,
                    "launchctl bootstrap",
                )?;
            }
            ServicePlatform::SystemdUser => {
                let systemd_unit = self.systemd_unit();
                checked(
                    self.runner
                        .run("systemctl", &["--user".into(), "daemon-reload".into()])?,
                    "systemctl --user daemon-reload",
                )?;
                checked(
                    self.runner.run(
                        "systemctl",
                        &[
                            "--user".into(),
                            "enable".into(),
                            "--now".into(),
                            systemd_unit,
                        ],
                    )?,
                    "systemctl --user enable --now",
                )?;
            }
            ServicePlatform::WindowsTask => {
                let windows_task = self.windows_task();
                checked(
                    self.runner.run(
                        "schtasks",
                        &[
                            "/Create".into(),
                            "/TN".into(),
                            windows_task,
                            "/XML".into(),
                            manifest.display().to_string(),
                            "/F".into(),
                        ],
                    )?,
                    "schtasks /Create",
                )?;
                self.start_current()?;
            }
        }
        Ok(())
    }

    pub fn start(&self, paths: &DaemonPaths) -> Result<(), String> {
        self.ensure_current_service_is_owned(paths)?;
        if !self.current_definition_exists(paths)? {
            if let Some(legacy) = self.owned_legacy_service(paths)? {
                if self.platform != ServicePlatform::WindowsTask && self.legacy_status()? {
                    return Ok(());
                }
                return self.restart_legacy(&legacy);
            }
        }
        self.start_current()
    }

    fn start_current(&self) -> Result<(), String> {
        match self.platform {
            ServicePlatform::Launchd => checked(
                self.runner.run(
                    "launchctl",
                    &[
                        "kickstart".into(),
                        "-k".into(),
                        format!("gui/{}/{}", self.user_id()?, self.launchd_label()),
                    ],
                )?,
                "launchctl kickstart",
            )
            .map(|_| ()),
            ServicePlatform::SystemdUser => checked(
                self.runner.run(
                    "systemctl",
                    &["--user".into(), "start".into(), self.systemd_unit()],
                )?,
                "systemctl --user start",
            )
            .map(|_| ()),
            ServicePlatform::WindowsTask => checked(
                self.runner.run(
                    "schtasks",
                    &["/Run".into(), "/TN".into(), self.windows_task()],
                )?,
                "schtasks /Run",
            )
            .map(|_| ()),
        }
    }

    fn current_definition_exists(&self, paths: &DaemonPaths) -> Result<bool, String> {
        if self.manifest_path(paths).is_file() {
            return Ok(true);
        }
        if self.platform == ServicePlatform::WindowsTask {
            return self.current_status();
        }
        Ok(false)
    }

    fn current_status(&self) -> Result<bool, String> {
        let output = match self.platform {
            ServicePlatform::Launchd => self.runner.run(
                "launchctl",
                &[
                    "print".into(),
                    format!("gui/{}/{}", self.user_id()?, self.launchd_label()),
                ],
            )?,
            ServicePlatform::SystemdUser => self.runner.run(
                "systemctl",
                &[
                    "--user".into(),
                    "is-active".into(),
                    "--quiet".into(),
                    self.systemd_unit(),
                ],
            )?,
            ServicePlatform::WindowsTask => self.runner.run(
                "schtasks",
                &["/Query".into(), "/TN".into(), self.windows_task()],
            )?,
        };
        Ok(output.status.success())
    }

    fn windows_task_running(&self, task: &str) -> Result<bool, String> {
        let task = task.replace('\'', "''");
        let output = checked(
            self.runner.run(
                "powershell.exe",
                &[
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    format!("(Get-ScheduledTask -TaskName '{task}').State.ToString()"),
                ],
            )?,
            "Get-ScheduledTask",
        )?;
        Ok(command_output_text(&output.stdout)
            .trim()
            .eq_ignore_ascii_case("Running"))
    }

    fn stop_current(&self, paths: &DaemonPaths) -> Result<bool, String> {
        if self.platform == ServicePlatform::WindowsTask {
            if !self.current_status()? || !self.windows_task_running(&self.windows_task())? {
                return Ok(false);
            }
        } else if !self.current_status()? {
            return Ok(false);
        }
        match self.platform {
            ServicePlatform::Launchd => checked(
                self.runner.run(
                    "launchctl",
                    &[
                        "bootout".into(),
                        format!("gui/{}", self.user_id()?),
                        self.manifest_path(paths).display().to_string(),
                    ],
                )?,
                "launchctl bootout",
            )?,
            ServicePlatform::SystemdUser => checked(
                self.runner.run(
                    "systemctl",
                    &["--user".into(), "stop".into(), self.systemd_unit()],
                )?,
                "systemctl --user stop",
            )?,
            ServicePlatform::WindowsTask => {
                let output = self.runner.run(
                    "schtasks",
                    &["/End".into(), "/TN".into(), self.windows_task()],
                )?;
                return Ok(output.status.success());
            }
        };
        Ok(true)
    }

    fn unregister_current(&self, paths: &DaemonPaths, remove_file: bool) -> Result<(), String> {
        let manifest = self.manifest_path(paths);
        match self.platform {
            ServicePlatform::Launchd => {}
            ServicePlatform::SystemdUser if manifest.is_file() => {
                checked(
                    self.runner.run(
                        "systemctl",
                        &["--user".into(), "disable".into(), self.systemd_unit()],
                    )?,
                    "systemctl --user disable",
                )?;
            }
            ServicePlatform::SystemdUser => {}
            ServicePlatform::WindowsTask => {
                if self.current_status()? {
                    checked(
                        self.runner.run(
                            "schtasks",
                            &[
                                "/Delete".into(),
                                "/TN".into(),
                                self.windows_task(),
                                "/F".into(),
                            ],
                        )?,
                        "schtasks /Delete",
                    )?;
                }
            }
        }
        if remove_file {
            remove_manifest(&manifest, "service manifest")?;
            if self.platform == ServicePlatform::SystemdUser {
                checked(
                    self.runner
                        .run("systemctl", &["--user".into(), "daemon-reload".into()])?,
                    "systemctl --user daemon-reload",
                )?;
            }
        }
        Ok(())
    }

    fn rollback_current(&self, paths: &DaemonPaths) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Err(error) = self.stop_current(paths) {
            errors.push(error);
        }
        if let Err(error) = self.unregister_current(paths, true) {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    pub fn is_installed(&self, paths: &DaemonPaths) -> Result<bool, String> {
        if !self.current_service_is_owned(paths)? {
            return Ok(false);
        }
        if self.current_definition_exists(paths)? {
            return Ok(true);
        }
        Ok(self.owned_legacy_service(paths)?.is_some())
    }

    /// Whether the published service definition is byte-identical to the one
    /// this build would write.
    ///
    /// It names an executable, a profile and log paths, so a definition left
    /// by an earlier install keeps launching *that* install's binary — the app
    /// can be replaced or moved and the service will not follow it. Anything
    /// other than an exact match is drift to be republished, including a
    /// missing or unreadable manifest: `install` is the repair either way, and
    /// guessing which differences are benign is how a stale service survives
    /// an upgrade.
    pub fn manifest_is_current(&self, paths: &DaemonPaths) -> Result<bool, String> {
        let Ok(published) = std::fs::read_to_string(self.manifest_path(paths)) else {
            return Ok(false);
        };
        Ok(published == self.render_manifest(paths)?)
    }

    pub fn status(&self, paths: &DaemonPaths) -> Result<bool, String> {
        if !self.current_service_is_owned(paths)? {
            return Ok(false);
        }
        if self.current_status()? {
            return Ok(true);
        }
        let Some(_legacy) = self.owned_legacy_service(paths)? else {
            return Ok(false);
        };
        self.legacy_status()
    }

    pub fn stop(&self, paths: &DaemonPaths) -> Result<(), String> {
        self.ensure_current_service_is_owned(paths)?;
        let legacy = self.owned_legacy_service(paths)?;
        self.stop_current(paths)?;
        if let Some(legacy) = legacy.as_ref() {
            self.stop_legacy(legacy)?;
        }
        Ok(())
    }

    pub fn uninstall(&self, paths: &DaemonPaths) -> Result<(), String> {
        self.ensure_current_service_is_owned(paths)?;
        let legacy = self.owned_legacy_service(paths)?;
        self.stop_current(paths)?;
        if let Some(legacy) = legacy.as_ref() {
            self.stop_legacy(legacy)?;
        }
        self.unregister_current(paths, true)?;
        if let Some(legacy) = legacy.as_ref() {
            self.retire_legacy(legacy)?;
        }
        Ok(())
    }

    fn user_id(&self) -> Result<String, String> {
        #[cfg(test)]
        {
            return Ok("501".to_string());
        }
        #[cfg(not(test))]
        {
            if let Ok(value) = std::env::var("UID") {
                if !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()) {
                    return Ok(value);
                }
            }
            let output = checked(self.runner.run("id", &["-u".into()])?, "id -u")?;
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if value.is_empty() || !value.chars().all(|ch| ch.is_ascii_digit()) {
                Err("id -u returned an invalid user id".to_string())
            } else {
                Ok(value)
            }
        }
    }
}

fn checked(output: Output, label: &str) -> Result<Output, String> {
    if output.status.success() {
        Ok(output)
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!(
            "{label} failed{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ))
    }
}

fn epoch_ms() -> Result<u64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock is before Unix epoch: {error}"))?
        .as_millis() as u64)
}

fn daemon_meta(database: &Path, key: &str) -> Option<String> {
    let connection =
        Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    connection
        .query_row(
            "SELECT value FROM daemon_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
}

struct LiveDaemonState {
    pid: u32,
    heartbeat_ms: u64,
    profile_id: Option<String>,
}

fn live_daemon_state(paths: &DaemonPaths) -> Option<LiveDaemonState> {
    const MAX_HEARTBEAT_AGE_MS: u64 = 5_000;

    let pid = std::fs::read_to_string(&paths.lock)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    if !process_alive(pid) {
        return None;
    }
    let stored_pid = daemon_meta(&paths.state_db, "pid")?.parse::<u32>().ok()?;
    if stored_pid != pid {
        return None;
    }
    let heartbeat_ms = daemon_meta(&paths.state_db, "heartbeat_ms")?
        .parse::<u64>()
        .ok()?;
    if epoch_ms().ok()?.saturating_sub(heartbeat_ms) > MAX_HEARTBEAT_AGE_MS {
        return None;
    }
    Some(LiveDaemonState {
        pid,
        heartbeat_ms,
        profile_id: daemon_meta(&paths.state_db, "profile_id"),
    })
}

fn live_daemon_profile(paths: &DaemonPaths) -> Option<Option<String>> {
    live_daemon_state(paths).map(|state| state.profile_id)
}

fn transaction_error(error: String, rollback_error: Option<String>) -> String {
    match rollback_error {
        Some(rollback) => format!("{error}; rollback failed: {rollback}"),
        None => error,
    }
}

fn transaction_error_many(error: String, rollback_errors: Vec<String>) -> String {
    transaction_error(
        error,
        (!rollback_errors.is_empty()).then(|| rollback_errors.join("; ")),
    )
}

fn utf8_path<'a>(path: &'a Path, label: &str) -> Result<&'a str, String> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("{label} path '{}' is not valid UTF-8", path.display()))?;
    if value.chars().any(char::is_control) {
        return Err(format!(
            "{label} path '{}' contains an unsupported control character",
            path.display()
        ));
    }
    Ok(value)
}

fn remove_manifest(path: &Path, label: &str) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to remove {label}: {error}")),
    }
}

fn publish_service_manifest(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Service manifest has no parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create service directory: {error}"))?;
    let temporary = parent.join(format!(
        ".service-manifest-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = std::fs::write(&temporary, contents) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("Failed to write service manifest: {error}"));
    }
    if let Err(error) = restrict_file(&temporary) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    replace_service_manifest(&temporary, path)
}

fn replace_service_manifest(temporary: &Path, destination: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            let _ = std::fs::remove_file(temporary);
            return Err(format!(
                "Refusing to replace service manifest directory '{}'",
                destination.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            return Err(format!(
                "Failed to inspect service manifest '{}': {error}",
                destination.display()
            ));
        }
    }
    match std::fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(first_error) if destination.exists() => {
            let parent = destination.parent().ok_or_else(|| {
                format!("Service manifest '{}' has no parent", destination.display())
            })?;
            let backup = parent.join(format!(
                ".service-manifest-{}.bak",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::rename(destination, &backup).map_err(|error| {
                let _ = std::fs::remove_file(temporary);
                format!(
                    "Failed to prepare service manifest replacement after {first_error}: {error}"
                )
            })?;
            if let Err(error) = std::fs::rename(temporary, destination) {
                let restore_error = std::fs::rename(&backup, destination).err();
                let _ = std::fs::remove_file(temporary);
                return Err(match restore_error {
                    Some(restore) => format!(
                        "Failed to publish service manifest: {error}; restoring the previous manifest also failed: {restore}"
                    ),
                    None => format!("Failed to publish service manifest: {error}"),
                });
            }
            // The destination is committed at this point. A stale backup is
            // recoverable cleanup and must not turn a successful replacement
            // into a reported transaction failure.
            discard_service_manifest_backup(&backup);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            Err(format!("Failed to publish service manifest: {error}"))
        }
    }
}

fn discard_service_manifest_backup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn command_output_text(bytes: &[u8]) -> String {
    let (bytes, utf16) = if bytes.starts_with(&[0xff, 0xfe]) {
        (&bytes[2..], Some(false))
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (&bytes[2..], Some(true))
    } else if bytes.len() >= 2 && bytes[1] == 0 {
        (bytes, Some(false))
    };
    let Some(big_endian) = utf16 else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let units = bytes.chunks_exact(2).map(|pair| {
        if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        }
    });
    String::from_utf16_lossy(&units.collect::<Vec<_>>())
}

fn normalize_windows_task_xml(xml: &str) -> String {
    xml.replacen("encoding=\"UTF-16\"", "encoding=\"UTF-8\"", 1)
        .replacen("encoding=\"utf-16\"", "encoding=\"UTF-8\"", 1)
}

/// Escape a value for XML markup. Shared with the carrier answer documents in
/// `telephony::` — a URL built from the operator's own configuration is still
/// interpolated into markup somebody else parses.
pub(crate) fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_escape(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn systemd_environment_escape(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn powershell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn powershell_encoded_command(script: &str) -> String {
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn windows_quote(value: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
            out.push(ch);
            backslashes = 0;
        } else {
            out.extend(std::iter::repeat_n('\\', backslashes));
            out.push(ch);
            backslashes = 0;
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

pub struct DaemonLock {
    path: PathBuf,
    file: File,
}

impl DaemonLock {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .map_err(|error| format!("Failed to write daemon lock: {error}"))?;
                    file.sync_all()
                        .map_err(|error| format!("Failed to sync daemon lock: {error}"))?;
                    restrict_file(path)?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        file,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let pid = std::fs::read_to_string(path)
                        .ok()
                        .and_then(|value| value.trim().parse::<u32>().ok());
                    if pid.is_some_and(process_alive) {
                        return Err(format!(
                            "Daemon is already running with pid {}",
                            pid.unwrap_or_default()
                        ));
                    }
                    std::fs::remove_file(path)
                        .map_err(|error| format!("Failed to clear stale daemon lock: {error}"))?;
                }
                Err(error) => return Err(format!("Failed to create daemon lock: {error}")),
            }
        }
        Err("Could not acquire daemon lock after clearing stale state".to_string())
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = self.file.sync_all();
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    struct NeverRunner;
    impl CommandRunner for NeverRunner {
        fn run(&self, _program: &str, _args: &[String]) -> Result<Output, String> {
            Err("not invoked by render test".into())
        }
    }

    struct FakeRunner {
        calls: Arc<Mutex<Vec<(String, Vec<String>)>>>,
        outputs: Mutex<VecDeque<Output>>,
    }

    impl FakeRunner {
        fn new(outputs: impl IntoIterator<Item = Output>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                outputs: Mutex::new(outputs.into_iter().collect()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output, String> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| format!("unexpected command: {program} {args:?}"))
        }
    }

    struct FakeHealthChecker {
        result: Result<(), String>,
        calls: Arc<AtomicU64>,
    }

    impl ServiceHealthChecker for FakeHealthChecker {
        fn wait_until_healthy(
            &self,
            _paths: &DaemonPaths,
            _profile_id: &str,
            _newer_than_ms: u64,
            _previous_pid: Option<u32>,
        ) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.result.clone()
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "little-monkey-service-test-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_output(success: bool, stdout: impl Into<Vec<u8>>) -> Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 })
        };
        Output {
            status,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    fn decoded_windows_script(manifest: &str) -> String {
        let marker = "-EncodedCommand ";
        let start = manifest.find(marker).unwrap() + marker.len();
        let end = manifest[start..].find('<').unwrap() + start;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&manifest[start..end])
            .unwrap();
        let units = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&units).unwrap()
    }

    fn manager(
        platform: ServicePlatform,
        profile_id: &str,
        agent_home: impl Into<PathBuf>,
    ) -> ServiceManager<NeverRunner> {
        let agent_home = agent_home.into();
        ServiceManager::new(
            NeverRunner,
            platform,
            PathBuf::from("/home/test"),
            PathBuf::from("/opt/Little Monkey/monkey"),
            test_roots(profile_id, profile_id, agent_home),
        )
        .unwrap()
    }

    fn test_roots(
        profile_id: &str,
        registry_active_id: &str,
        agent_home: impl Into<PathBuf>,
    ) -> AgentConfigRoots {
        let agent_home = agent_home.into();
        AgentConfigRoots {
            profile_id: profile_id.to_string(),
            registry_active_id: registry_active_id.to_string(),
            authored: agent_home.join("profiles").join(profile_id),
            legacy: PathBuf::from("/tmp/little-monkey-data")
                .join("profiles")
                .join(profile_id),
            agent_home,
        }
    }

    fn manager_with_fake_health(
        runner: FakeRunner,
        platform: ServicePlatform,
        home: PathBuf,
        executable: PathBuf,
        roots: AgentConfigRoots,
        health_result: Result<(), String>,
    ) -> (ServiceManager<FakeRunner>, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let manager = ServiceManager::new_with_health_checker(
            runner,
            platform,
            home,
            executable,
            roots,
            Box::new(FakeHealthChecker {
                result: health_result,
                calls: Arc::clone(&calls),
            }),
        )
        .unwrap();
        (manager, calls)
    }

    fn mark_profile_live(paths: &DaemonPaths, profile_id: &str) {
        mark_profile_live_with_marker(paths, Some(profile_id));
    }

    fn mark_profile_live_with_marker(paths: &DaemonPaths, profile_id: Option<&str>) {
        paths.ensure().unwrap();
        let pid = std::process::id();
        std::fs::write(&paths.lock, pid.to_string()).unwrap();
        let mut store = DaemonStore::open(paths).unwrap();
        if let Some(profile_id) = profile_id {
            store.set_meta("profile_id", profile_id).unwrap();
        }
        store.set_meta("pid", &pid.to_string()).unwrap();
        store
            .set_meta("heartbeat_ms", &epoch_ms().unwrap().to_string())
            .unwrap();
    }

    #[test]
    fn manifests_quote_executable_and_use_explicit_daemon_serve() {
        let app = PathBuf::from("/tmp/app");
        let paths = DaemonPaths::under(&app);
        for platform in [
            ServicePlatform::Launchd,
            ServicePlatform::SystemdUser,
            ServicePlatform::WindowsTask,
        ] {
            let manager = manager(platform, "work", "/home/test/Agent Home");
            let rendered = manager.render_manifest(&paths).unwrap();
            let command = if platform == ServicePlatform::WindowsTask {
                decoded_windows_script(&rendered)
            } else {
                rendered.clone()
            };
            assert!(command.contains("daemon"));
            assert!(command.contains("serve"));
            assert!(command.contains("LITTLE_MONKEY_HOME"));
            if platform == ServicePlatform::Launchd {
                assert!(rendered.contains("<key>HOME</key><string>/home/test</string>"));
                assert!(rendered.contains(
                    "<key>WorkingDirectory</key><string>/home/test</string>"
                ));
            }
            assert!(!rendered.contains("--agent-home"));
            assert!(!command.contains("--agent-home"));
            assert!(command.contains("profile"));
            assert!(command.contains("work"));
            assert!(!command.contains("bypass"));
        }
    }

    #[test]
    fn default_profile_preserves_existing_service_ids_and_manifest_paths() {
        let paths = DaemonPaths::under(Path::new("/tmp/app"));
        let launchd = manager(
            ServicePlatform::Launchd,
            "default",
            "/home/test/.littlemonkey",
        );
        assert_eq!(launchd.launchd_label(), "com.littlemonkey.daemon");
        assert_eq!(
            launchd.manifest_path(&paths),
            PathBuf::from("/home/test/Library/LaunchAgents/com.littlemonkey.daemon.plist")
        );

        let systemd = manager(
            ServicePlatform::SystemdUser,
            "default",
            "/home/test/.littlemonkey",
        );
        assert_eq!(systemd.systemd_unit(), "little-monkey-daemon.service");
        assert_eq!(
            systemd.manifest_path(&paths),
            PathBuf::from("/home/test/.config/systemd/user/little-monkey-daemon.service")
        );

        let windows = manager(
            ServicePlatform::WindowsTask,
            "default",
            "/home/test/.littlemonkey",
        );
        assert_eq!(windows.windows_task(), "LittleMonkeyDaemon");
        assert_eq!(
            windows.manifest_path(&paths),
            paths.root.join("little-monkey-daemon-task.xml")
        );
    }

    #[test]
    fn named_profile_scopes_every_service_id_and_manifest_path() {
        let paths = DaemonPaths::under(Path::new("/tmp/app"));
        let launchd = manager(ServicePlatform::Launchd, "work-2", "/home/test/agent");
        assert_eq!(launchd.launchd_label(), "com.littlemonkey.daemon.work-2");
        assert!(launchd
            .manifest_path(&paths)
            .ends_with("com.littlemonkey.daemon.work-2.plist"));

        let systemd = manager(ServicePlatform::SystemdUser, "work-2", "/home/test/agent");
        assert_eq!(
            systemd.systemd_unit(),
            "little-monkey-daemon-work-2.service"
        );
        assert!(systemd
            .manifest_path(&paths)
            .ends_with("little-monkey-daemon-work-2.service"));

        let windows = manager(ServicePlatform::WindowsTask, "work-2", "/home/test/agent");
        assert_eq!(windows.windows_task(), "LittleMonkeyDaemon-work-2");
        assert!(windows
            .manifest_path(&paths)
            .ends_with("little-monkey-daemon-task-work-2.xml"));
    }

    #[test]
    fn service_manager_rejects_unvalidated_profile_ids() {
        assert!(ServiceManager::new(
            NeverRunner,
            ServicePlatform::Launchd,
            PathBuf::from("/home/test"),
            PathBuf::from("/opt/monkey"),
            test_roots("../work", "default", "/home/test/agent"),
        )
        .is_err());
    }

    #[test]
    fn manifests_escape_special_character_agent_home_paths() {
        let paths = DaemonPaths::under(Path::new("/tmp/app"));
        let agent_home = r#"/home/test/Agent $HOME % & <portable> "quoted""#;

        let launchd = manager(ServicePlatform::Launchd, "work", agent_home);
        let launchd_manifest = launchd.render_manifest(&paths).unwrap();
        assert!(launchd_manifest.contains(
            r#"<string>/home/test/Agent $HOME % &amp; &lt;portable&gt; &quot;quoted&quot;</string>"#
        ));

        let systemd = manager(ServicePlatform::SystemdUser, "work", agent_home);
        let systemd_manifest = systemd.render_manifest(&paths).unwrap();
        assert!(systemd_manifest.contains(
            r#"Environment="LITTLE_MONKEY_HOME=/home/test/Agent $HOME %% & <portable> \"quoted\"""#
        ));

        let windows = manager(ServicePlatform::WindowsTask, "work", agent_home);
        let windows_manifest = windows.render_manifest(&paths).unwrap();
        assert!(windows_manifest.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        let script = decoded_windows_script(&windows_manifest);
        assert!(script.contains(
            "$env:LITTLE_MONKEY_HOME='/home/test/Agent $HOME % & <portable> \"quoted\"'"
        ));
        assert!(script.ends_with("; exit $LASTEXITCODE"));
        assert!(!script.contains("--agent-home"));
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_non_utf8_executable_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let manager = ServiceManager::new(
            NeverRunner,
            ServicePlatform::SystemdUser,
            PathBuf::from("/home/test"),
            PathBuf::from(OsString::from_vec(vec![b'/', b'o', b'p', b't', b'/', 0xff])),
            test_roots("work", "work", "/home/test/agent"),
        )
        .unwrap();
        let error = manager
            .render_manifest(&DaemonPaths::under(Path::new("/tmp/app")))
            .unwrap_err();
        assert!(error.contains("not valid UTF-8"));
    }

    #[test]
    fn manifests_reject_control_characters_in_paths() {
        let paths = DaemonPaths::under(Path::new("/tmp/app"));
        for platform in [
            ServicePlatform::Launchd,
            ServicePlatform::SystemdUser,
            ServicePlatform::WindowsTask,
        ] {
            let manager = ServiceManager::new(
                NeverRunner,
                platform,
                PathBuf::from("/home/test"),
                PathBuf::from("/opt/monkey\nrenamed"),
                test_roots("work", "work", "/home/test/agent"),
            )
            .unwrap();
            assert!(manager
                .render_manifest(&paths)
                .unwrap_err()
                .contains("unsupported control character"));
        }

        let manager = manager(
            ServicePlatform::Launchd,
            "work",
            PathBuf::from("/home/test/agent\u{1}home"),
        );
        assert!(manager
            .render_manifest(&paths)
            .unwrap_err()
            .contains("unsupported control character"));
    }

    #[test]
    fn systemd_escaping_handles_spaces_percent_dollars_and_quotes() {
        assert_eq!(systemd_escape("a b%$\"c"), "\"a b%%$$\\\"c\"");
    }

    #[test]
    fn windows_quoting_preserves_spaces_and_trailing_backslashes() {
        assert_eq!(windows_quote(r"C:\Agent Home\"), r#""C:\Agent Home\\""#);
    }

    #[test]
    fn named_systemd_status_is_read_only_and_recognizes_owned_released_service() {
        let temp = TestDir::new();
        let home = temp.0.join("home");
        let paths = DaemonPaths::under(&temp.0.join("state"));
        mark_profile_live(&paths, "work");
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
        ]);
        let calls = Arc::clone(&runner.calls);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            home,
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
        )
        .unwrap();
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        assert!(manager.status(&paths).unwrap());
        assert!(legacy_manifest.exists());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (
                    "systemctl".to_string(),
                    vec![
                        "--user".into(),
                        "is-active".into(),
                        "--quiet".into(),
                        "little-monkey-daemon-work.service".into(),
                    ],
                ),
                (
                    "systemctl".to_string(),
                    vec![
                        "--user".into(),
                        "is-active".into(),
                        "--quiet".into(),
                        SYSTEMD_UNIT.into(),
                    ],
                ),
                (
                    "systemctl".to_string(),
                    vec![
                        "--user".into(),
                        "is-active".into(),
                        "--quiet".into(),
                        SYSTEMD_UNIT.into(),
                    ],
                ),
            ]
        );
    }

    #[test]
    fn markerless_selected_daemon_owns_released_service_when_it_is_the_only_live_root() {
        let temp = TestDir::new();
        let data_base = temp.0.join("data");
        let profile_root = data_base
            .join(little_monkey_lib::profiles::PROFILES_DIR)
            .join("work");
        let paths = DaemonPaths::under(&profile_root);
        mark_profile_live_with_marker(&paths, None);
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
        ]);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
        )
        .unwrap();
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        assert!(manager.status(&paths).unwrap());
    }

    #[test]
    fn markerless_selected_daemon_does_not_claim_released_service_with_another_live_root() {
        let temp = TestDir::new();
        let data_base = temp.0.join("data");
        let profile_root = data_base
            .join(little_monkey_lib::profiles::PROFILES_DIR)
            .join("work");
        let paths = DaemonPaths::under(&profile_root);
        mark_profile_live_with_marker(&paths, None);
        mark_profile_live(&DaemonPaths::under(&data_base), "default");
        let runner = FakeRunner::new([fake_output(true, [])]);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
        )
        .unwrap();
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        let error = manager.stop(&paths).unwrap_err();
        assert!(error.contains("Cannot prove"));
        assert!(legacy_manifest.exists());
    }

    #[test]
    fn named_profile_preserves_legacy_service_owned_by_default_profile() {
        let temp = TestDir::new();
        let home = temp.0.join("home");
        let paths = DaemonPaths::under(&temp.0.join("state"));
        let runner = FakeRunner::new([fake_output(false, []), fake_output(false, [])]);
        let calls = Arc::clone(&runner.calls);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            home,
            PathBuf::from("/opt/monkey"),
            test_roots("work", "default", temp.0.join("agent")),
        )
        .unwrap();
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        assert!(!manager.status(&paths).unwrap());
        assert!(legacy_manifest.exists());
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(
            calls.lock().unwrap()[0].1,
            vec![
                "--user".to_string(),
                "is-active".to_string(),
                "--quiet".to_string(),
                "little-monkey-daemon-work.service".to_string(),
            ]
        );
    }

    #[test]
    fn start_leaves_an_already_running_owned_legacy_service_untouched() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        mark_profile_live(&paths, "work");
        let runner = FakeRunner::new([fake_output(true, []), fake_output(true, [])]);
        let calls = Arc::clone(&runner.calls);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
        )
        .unwrap();
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        manager.start(&paths).unwrap();
        assert_eq!(
            calls.lock().unwrap()[0].1,
            vec!["--user", "is-active", "--quiet", SYSTEMD_UNIT]
        );
    }

    #[test]
    fn named_systemd_install_cuts_over_only_after_scoped_state_is_published() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        mark_profile_live(&paths, "work");
        let home = temp.0.join("home");
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
        ]);
        let calls = Arc::clone(&runner.calls);
        let (manager, health_calls) = manager_with_fake_health(
            runner,
            ServicePlatform::SystemdUser,
            home,
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
            Ok(()),
        );
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        let scoped_manifest = manager.install(&paths, &DaemonConfig::default()).unwrap();
        assert!(paths.config.is_file());
        assert!(scoped_manifest.is_file());
        assert!(!legacy_manifest.exists());
        assert_eq!(health_calls.load(Ordering::Relaxed), 1);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[2].1,
            vec!["--user", "is-active", "--quiet", SYSTEMD_UNIT]
        );
        assert_eq!(calls[4].1, vec!["--user", "stop", SYSTEMD_UNIT]);
        assert_eq!(calls[5].1, vec!["--user", "daemon-reload"]);
        assert_eq!(
            calls[6].1,
            vec![
                "--user",
                "enable",
                "--now",
                "little-monkey-daemon-work.service",
            ]
        );
        assert_eq!(
            calls[7].1,
            vec![
                "--user",
                "is-active",
                "--quiet",
                "little-monkey-daemon-work.service"
            ]
        );
        assert_eq!(calls[8].1, vec!["--user", "disable", SYSTEMD_UNIT]);
        assert_eq!(calls[9].1, vec!["--user", "daemon-reload"]);
    }

    #[test]
    fn failed_scoped_activation_restarts_previously_running_legacy_service() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        mark_profile_live(&paths, "work");
        let home = temp.0.join("home");
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
        ]);
        let calls = Arc::clone(&runner.calls);
        let (manager, health_calls) = manager_with_fake_health(
            runner,
            ServicePlatform::SystemdUser,
            home,
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
            Ok(()),
        );
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        assert!(manager.install(&paths, &DaemonConfig::default()).is_err());
        assert_eq!(health_calls.load(Ordering::Relaxed), 0);
        assert!(legacy_manifest.exists());
        assert!(!manager.manifest_path(&paths).exists());
        let calls = calls.lock().unwrap();
        assert_eq!(calls[4].1, vec!["--user", "stop", SYSTEMD_UNIT]);
        assert_eq!(
            calls[6].1,
            vec![
                "--user",
                "enable",
                "--now",
                "little-monkey-daemon-work.service",
            ]
        );
        assert_eq!(
            calls.last().unwrap().1,
            vec!["--user", "start", SYSTEMD_UNIT]
        );
        assert!(!calls
            .iter()
            .any(|(_, args)| args == &["--user", "disable", SYSTEMD_UNIT]));
    }

    #[test]
    fn windows_install_tolerates_idle_owned_legacy_task() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        let legacy_xml = r#"<Arguments>--profile &quot;work&quot; daemon serve</Arguments>"#;
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(true, legacy_xml),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
        ]);
        let calls = Arc::clone(&runner.calls);
        let (manager, health_calls) = manager_with_fake_health(
            runner,
            ServicePlatform::WindowsTask,
            temp.0.join("home"),
            PathBuf::from(r"C:\Program Files\Little Monkey\monkey.exe"),
            test_roots("work", "work", temp.0.join("agent")),
            Ok(()),
        );
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, legacy_xml).unwrap();

        assert!(manager.install(&paths, &DaemonConfig::default()).is_ok());
        assert_eq!(health_calls.load(Ordering::Relaxed), 1);
        assert!(!legacy_manifest.exists());
        let calls = calls.lock().unwrap();
        assert_eq!(calls[2].1, vec!["/End", "/TN", WINDOWS_TASK]);
        assert_eq!(
            calls[3].1,
            vec![
                "/Create",
                "/TN",
                "LittleMonkeyDaemon-work",
                "/XML",
                manager.manifest_path(&paths).to_str().unwrap(),
                "/F",
            ]
        );
        assert_eq!(calls[4].1, vec!["/Run", "/TN", "LittleMonkeyDaemon-work"]);
        assert_eq!(calls[7].1, vec!["/Delete", "/TN", WINDOWS_TASK, "/F"]);
    }

    #[test]
    fn windows_stop_current_tolerates_an_idle_registered_task() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        let runner = FakeRunner::new([fake_output(true, []), fake_output(true, b"Ready".to_vec())]);
        let calls = Arc::clone(&runner.calls);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::WindowsTask,
            temp.0.join("home"),
            PathBuf::from(r"C:\Program Files\Little Monkey\monkey.exe"),
            test_roots("work", "work", temp.0.join("agent")),
        )
        .unwrap();

        assert!(!manager.stop_current(&paths).unwrap());
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert!(!calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, args)| args.first().is_some_and(|arg| arg == "/End")));
    }

    #[test]
    fn uninstall_propagates_failure_to_disable_an_owned_legacy_service() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        mark_profile_live(&paths, "work");
        let runner = FakeRunner::new([
            fake_output(true, []),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(false, []),
        ]);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
        )
        .unwrap();
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        let error = manager.uninstall(&paths).unwrap_err();
        assert!(error.contains("disable legacy service"));
        assert!(legacy_manifest.exists());
    }

    #[test]
    fn default_profile_refuses_unpinned_fixed_service_owned_by_named_registry_profile() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(false, []),
            fake_output(false, []),
            fake_output(false, []),
            fake_output(false, []),
            fake_output(false, []),
        ]);
        let calls = Arc::clone(&runner.calls);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("default", "work", temp.0.join("agent")),
        )
        .unwrap();
        let manifest = manager.manifest_path(&paths);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let released = "ExecStart=/opt/monkey daemon serve\n";
        std::fs::write(&manifest, released).unwrap();

        assert!(!manager.is_installed(&paths).unwrap());
        assert!(!manager.status(&paths).unwrap());
        assert!(manager.start(&paths).is_err());
        assert!(manager.stop(&paths).is_err());
        assert!(manager.uninstall(&paths).is_err());
        assert!(manager.install(&paths, &DaemonConfig::default()).is_err());
        assert_eq!(std::fs::read_to_string(&manifest).unwrap(), released);
        assert!(!paths.config.exists());
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, args)| args.get(1).is_some_and(|arg| arg == "is-active")));
    }

    #[test]
    fn switching_work_to_default_refuses_a_running_unpinned_fixed_service_without_live_proof() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("data"));
        let runner = FakeRunner::new([fake_output(true, [])]);
        let calls = Arc::clone(&runner.calls);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("default", "default", temp.0.join("agent")),
        )
        .unwrap();
        let manifest = manager.manifest_path(&paths);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        let error = manager.stop(&paths).unwrap_err();
        assert!(error.contains("Cannot prove"));
        assert!(manifest.exists());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn switching_default_to_work_refuses_a_running_unpinned_fixed_service_without_live_proof() {
        let temp = TestDir::new();
        let data_base = temp.0.join("data");
        let paths = DaemonPaths::under(
            &data_base
                .join(little_monkey_lib::profiles::PROFILES_DIR)
                .join("work"),
        );
        let runner = FakeRunner::new([fake_output(true, [])]);
        let calls = Arc::clone(&runner.calls);
        let manager = ServiceManager::new(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
        )
        .unwrap();
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        std::fs::write(&legacy_manifest, "ExecStart=/opt/monkey daemon serve\n").unwrap();

        let error = manager.stop(&paths).unwrap_err();
        assert!(error.contains("Cannot prove"));
        assert!(legacy_manifest.exists());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    /// The upgrade case `daemon ensure` exists for: the app is replaced, the
    /// definition still names the previous install's binary, and nothing
    /// notices because the service is registered and running.
    #[test]
    fn a_manifest_from_a_previous_install_does_not_count_as_current() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        let manager = ServiceManager::new(
            NeverRunner,
            ServicePlatform::Launchd,
            temp.0.join("home"),
            PathBuf::from("/Applications/Little Monkey.app/Contents/MacOS/monkey"),
            test_roots("default", "default", temp.0.join("agent")),
        )
        .unwrap();
        let manifest = manager.manifest_path(&paths);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();

        // Never published at all.
        assert!(!manager.manifest_is_current(&paths).unwrap());

        // Published by an install that lived somewhere else.
        let previous = manager.render_manifest(&paths).unwrap().replace(
            "/Applications/Little Monkey.app",
            "/Users/someone/Downloads",
        );
        std::fs::write(&manifest, &previous).unwrap();
        assert!(!manager.manifest_is_current(&paths).unwrap());

        // Published by this build.
        std::fs::write(&manifest, manager.render_manifest(&paths).unwrap()).unwrap();
        assert!(manager.manifest_is_current(&paths).unwrap());
    }

    #[test]
    fn windows_reinstall_replaces_an_existing_manifest() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
        ]);
        let (manager, health_calls) = manager_with_fake_health(
            runner,
            ServicePlatform::WindowsTask,
            temp.0.join("home"),
            PathBuf::from(r"C:\Program Files\Little Monkey\monkey.exe"),
            test_roots("default", "default", temp.0.join("agent")),
            Ok(()),
        );
        let manifest = manager.manifest_path(&paths);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "released manifest").unwrap();

        manager.install(&paths, &DaemonConfig::default()).unwrap();
        assert_eq!(health_calls.load(Ordering::Relaxed), 1);
        let installed = std::fs::read_to_string(&manifest).unwrap();
        let script = decoded_windows_script(&installed);
        assert!(script.contains("$env:LITTLE_MONKEY_HOME="));
        assert!(script.contains("--profile 'default'"));
        assert!(!script.contains("--agent-home"));
    }

    #[test]
    fn service_manifest_replacement_refuses_a_destination_directory() {
        let temp = TestDir::new();
        let destination = temp.0.join("manifest");
        let child = destination.join("keep");
        let temporary = temp.0.join("manifest.tmp");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&child, "preserved").unwrap();
        std::fs::write(&temporary, "new manifest").unwrap();

        let error = replace_service_manifest(&temporary, &destination).unwrap_err();
        assert!(error.contains("Refusing to replace service manifest directory"));
        assert_eq!(std::fs::read_to_string(child).unwrap(), "preserved");
        assert!(!temporary.exists());
    }

    #[test]
    fn windows_manifest_backup_cleanup_failure_is_non_fatal_after_commit() {
        let temp = TestDir::new();
        let committed = temp.0.join("manifest.xml");
        let unremovable_backup = temp.0.join("manifest.backup");
        std::fs::write(&committed, "new manifest").unwrap();
        std::fs::create_dir(&unremovable_backup).unwrap();

        discard_service_manifest_backup(&unremovable_backup);

        assert_eq!(std::fs::read_to_string(committed).unwrap(), "new manifest");
        assert!(unremovable_backup.is_dir());
    }

    #[test]
    fn retirement_failure_rolls_back_scoped_service_and_restores_legacy() {
        let temp = TestDir::new();
        let paths = DaemonPaths::under(&temp.0.join("state"));
        mark_profile_live(&paths, "work");
        let runner = FakeRunner::new([
            fake_output(false, []),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(false, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
            fake_output(true, []),
        ]);
        let calls = Arc::clone(&runner.calls);
        let (manager, health_calls) = manager_with_fake_health(
            runner,
            ServicePlatform::SystemdUser,
            temp.0.join("home"),
            PathBuf::from("/opt/monkey"),
            test_roots("work", "work", temp.0.join("agent")),
            Ok(()),
        );
        let legacy_manifest = manager.legacy_manifest_path(&paths);
        std::fs::create_dir_all(legacy_manifest.parent().unwrap()).unwrap();
        let released = "ExecStart=/opt/monkey daemon serve\n";
        std::fs::write(&legacy_manifest, released).unwrap();

        let error = manager
            .install(&paths, &DaemonConfig::default())
            .unwrap_err();
        assert!(error.contains("disable legacy service"));
        assert_eq!(health_calls.load(Ordering::Relaxed), 1);
        assert!(!manager.manifest_path(&paths).exists());
        assert_eq!(std::fs::read_to_string(&legacy_manifest).unwrap(), released);
        let calls = calls.lock().unwrap();
        assert_eq!(calls[14].1, vec!["--user", "enable", SYSTEMD_UNIT]);
        assert_eq!(calls[15].1, vec!["--user", "start", SYSTEMD_UNIT]);
    }
}
