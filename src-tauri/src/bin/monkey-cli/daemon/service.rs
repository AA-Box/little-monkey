use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Serialize;

use super::store::{restrict_file, DaemonConfig, DaemonPaths};

const LAUNCHD_LABEL: &str = "com.littlemonkey.daemon";
const SYSTEMD_UNIT: &str = "little-monkey-daemon.service";
const WINDOWS_TASK: &str = "LittleMonkeyDaemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServicePlatform {
    Launchd,
    SystemdUser,
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

pub struct ServiceManager<R> {
    runner: R,
    platform: ServicePlatform,
    home: PathBuf,
    executable: PathBuf,
}

impl<R: CommandRunner> ServiceManager<R> {
    pub fn new(runner: R, platform: ServicePlatform, home: PathBuf, executable: PathBuf) -> Self {
        Self {
            runner,
            platform,
            home,
            executable,
        }
    }

    pub fn real() -> Result<ServiceManager<RealCommandRunner>, String> {
        let home =
            dirs::home_dir().ok_or_else(|| "Could not resolve the home directory".to_string())?;
        let executable = std::env::current_exe()
            .map_err(|error| format!("Could not resolve monkey executable: {error}"))?;
        Ok(ServiceManager::new(
            RealCommandRunner,
            ServicePlatform::current(),
            home,
            executable,
        ))
    }

    pub fn platform(&self) -> ServicePlatform {
        self.platform
    }

    pub fn manifest_path(&self, paths: &DaemonPaths) -> PathBuf {
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

    pub fn render_manifest(&self, paths: &DaemonPaths) -> String {
        let executable = self.executable.to_string_lossy();
        match self.platform {
            ServicePlatform::Launchd => format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LAUNCHD_LABEL}</string>\n  <key>ProgramArguments</key>\n  <array><string>{}</string><string>daemon</string><string>serve</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ProcessType</key><string>Background</string>\n  <key>StandardOutPath</key><string>{}</string>\n  <key>StandardErrorPath</key><string>{}</string>\n</dict>\n</plist>\n",
                xml_escape(&executable),
                xml_escape(&paths.logs.join("service.stdout.log").to_string_lossy()),
                xml_escape(&paths.logs.join("service.stderr.log").to_string_lossy()),
            ),
            ServicePlatform::SystemdUser => format!(
                "[Unit]\nDescription=Little Monkey durable local agent daemon\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart={} daemon serve\nRestart=on-failure\nRestartSec=2\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=full\nWorkingDirectory={}\n\n[Install]\nWantedBy=default.target\n",
                systemd_escape(&executable),
                systemd_escape(&self.home.to_string_lossy()),
            ),
            ServicePlatform::WindowsTask => format!(
                "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\n  <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>\n  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>\n  <Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>daemon serve</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>\n</Task>\n",
                xml_escape(&executable),
                xml_escape(&paths.root.to_string_lossy()),
            ),
        }
    }

    pub fn install(&self, paths: &DaemonPaths, config: &DaemonConfig) -> Result<PathBuf, String> {
        config.save(paths)?;
        let manifest = self.manifest_path(paths);
        let parent = manifest
            .parent()
            .ok_or_else(|| "Service manifest has no parent directory".to_string())?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create service directory: {error}"))?;
        let tmp = manifest.with_extension("tmp");
        std::fs::write(&tmp, self.render_manifest(paths))
            .map_err(|error| format!("Failed to write service manifest: {error}"))?;
        restrict_file(&tmp)?;
        std::fs::rename(&tmp, &manifest)
            .map_err(|error| format!("Failed to publish service manifest: {error}"))?;
        restrict_file(&manifest)?;
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
                            SYSTEMD_UNIT.into(),
                        ],
                    )?,
                    "systemctl --user enable --now",
                )?;
            }
            ServicePlatform::WindowsTask => {
                checked(
                    self.runner.run(
                        "schtasks",
                        &[
                            "/Create".into(),
                            "/TN".into(),
                            WINDOWS_TASK.into(),
                            "/XML".into(),
                            manifest.display().to_string(),
                            "/F".into(),
                        ],
                    )?,
                    "schtasks /Create",
                )?;
                self.start(paths)?;
            }
        }
        Ok(manifest)
    }

    pub fn start(&self, _paths: &DaemonPaths) -> Result<(), String> {
        match self.platform {
            ServicePlatform::Launchd => checked(
                self.runner.run(
                    "launchctl",
                    &[
                        "kickstart".into(),
                        "-k".into(),
                        format!("gui/{}/{LAUNCHD_LABEL}", self.user_id()?),
                    ],
                )?,
                "launchctl kickstart",
            )
            .map(|_| ()),
            ServicePlatform::SystemdUser => checked(
                self.runner.run(
                    "systemctl",
                    &["--user".into(), "start".into(), SYSTEMD_UNIT.into()],
                )?,
                "systemctl --user start",
            )
            .map(|_| ()),
            ServicePlatform::WindowsTask => checked(
                self.runner.run(
                    "schtasks",
                    &["/Run".into(), "/TN".into(), WINDOWS_TASK.into()],
                )?,
                "schtasks /Run",
            )
            .map(|_| ()),
        }
    }

    pub fn stop(&self, paths: &DaemonPaths) -> Result<(), String> {
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
            )
            .map(|_| ()),
            ServicePlatform::SystemdUser => checked(
                self.runner.run(
                    "systemctl",
                    &["--user".into(), "stop".into(), SYSTEMD_UNIT.into()],
                )?,
                "systemctl --user stop",
            )
            .map(|_| ()),
            ServicePlatform::WindowsTask => checked(
                self.runner.run(
                    "schtasks",
                    &["/End".into(), "/TN".into(), WINDOWS_TASK.into()],
                )?,
                "schtasks /End",
            )
            .map(|_| ()),
        }
    }

    pub fn status(&self, _paths: &DaemonPaths) -> Result<bool, String> {
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

    pub fn uninstall(&self, paths: &DaemonPaths) -> Result<(), String> {
        let _ = self.stop(paths);
        match self.platform {
            ServicePlatform::Launchd => {}
            ServicePlatform::SystemdUser => {
                let _ = self.runner.run(
                    "systemctl",
                    &["--user".into(), "disable".into(), SYSTEMD_UNIT.into()],
                );
            }
            ServicePlatform::WindowsTask => {
                let _ = self.runner.run(
                    "schtasks",
                    &[
                        "/Delete".into(),
                        "/TN".into(),
                        WINDOWS_TASK.into(),
                        "/F".into(),
                    ],
                );
            }
        }
        let manifest = self.manifest_path(paths);
        match std::fs::remove_file(&manifest) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Failed to remove service manifest: {error}")),
        }
        if self.platform == ServicePlatform::SystemdUser {
            let _ = self
                .runner
                .run("systemctl", &["--user".into(), "daemon-reload".into()]);
        }
        Ok(())
    }

    fn user_id(&self) -> Result<String, String> {
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

fn xml_escape(value: &str) -> String {
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
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
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
    use super::*;

    struct NeverRunner;
    impl CommandRunner for NeverRunner {
        fn run(&self, _program: &str, _args: &[String]) -> Result<Output, String> {
            Err("not invoked by render test".into())
        }
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
            let manager = ServiceManager::new(
                NeverRunner,
                platform,
                PathBuf::from("/home/test"),
                PathBuf::from("/opt/Little Monkey/monkey"),
            );
            let rendered = manager.render_manifest(&paths);
            assert!(rendered.contains("daemon"));
            assert!(rendered.contains("serve"));
            assert!(!rendered.contains("bypass"));
        }
    }

    #[test]
    fn systemd_escaping_handles_spaces_percent_and_quotes() {
        assert_eq!(systemd_escape("a b%\"c"), "\"a b%%\\\"c\"");
    }
}
