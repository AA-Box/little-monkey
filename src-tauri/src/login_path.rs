//! Repairs the process `PATH` when the app is launched from the GUI.
//!
//! A double-clicked `.app` inherits launchd's `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin`), not the one the user's shell builds from
//! `.zshrc`/`.zprofile`. Every tool that runs a command — `background_shell`,
//! `terminal`, and `sandbox::allowlisted_env` — reads `PATH` out of this
//! process, so a user-installed binary (`~/.local/bin/graphify`, anything in
//! `/opt/homebrew/bin`, nvm/pyenv/mise shims) is simply not found, and the
//! agent reports "not in PATH" for a command that works fine in Terminal.
//!
//! So: when `PATH` still looks like launchd's default, ask the user's login
//! shell what `PATH` should be and merge it in. When the app was started from
//! a terminal (`cargo tauri dev`) `PATH` is already rich, this is a no-op, and
//! no shell is spawned.
//!
//! Widening `PATH` also widens the seatbelt profile's read roots, because
//! `sandbox::macos_readable_roots` derives them from `PATH` — a user toolchain
//! directory becomes readable exactly when it becomes reachable.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Everything launchd puts on `PATH`, plus `/usr/local/bin` which the system
/// `path_helper` adds for every login. A `PATH` made only of these carries no
/// user configuration, so it is the signal that we were launched from Finder.
const SYSTEM_PATH_DIRS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// Marks the line carrying `PATH` so that anything an interactive rc file
/// prints on the way (version managers, greetings, `motd`) is ignored.
const SENTINEL: &str = "__little_monkey_path__:";

/// An rc file that blocks (a prompt, a slow network call) must not stop the
/// app from starting. Give up and keep the launchd `PATH` instead.
const SHELL_TIMEOUT: Duration = Duration::from_secs(2);

/// Merge the login shell's `PATH` into this process's `PATH`, if this process
/// looks GUI-launched. Call once, before any thread that spawns a child.
pub fn hydrate() {
    if cfg!(windows) {
        return;
    }
    let Ok(current) = std::env::var("PATH") else {
        return;
    };
    if !is_system_only(&current) {
        return;
    }
    let Some(login) = login_shell_path() else {
        return;
    };
    if let Some(merged) = merge(&login, &current) {
        std::env::set_var("PATH", merged);
    }
}

/// True when every entry is a stock system directory, i.e. nothing on `PATH`
/// came from the user's shell configuration.
fn is_system_only(path: &str) -> bool {
    path.split(':')
        .filter(|entry| !entry.is_empty())
        .all(|entry| SYSTEM_PATH_DIRS.contains(&entry))
}

fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    // `-i` as well as `-l`: plenty of people set PATH in `.zshrc`, which a
    // non-interactive login shell never reads.
    let mut child = Command::new(shell)
        .args(["-ilc", &format!("echo \"{SENTINEL}$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buffer = String::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_string(&mut buffer);
        let _ = tx.send(buffer);
    });
    let output = rx.recv_timeout(SHELL_TIMEOUT).ok();
    let _ = child.kill();
    let _ = child.wait();

    extract(&output?)
}

fn extract(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(SENTINEL))
        .map(str::to_string)
}

/// Login entries first, then anything already on `PATH` that they don't
/// cover. Entries that aren't existing directories are dropped, which is also
/// what saves us from shells whose `echo $PATH` isn't colon-separated (fish):
/// the mangled entry doesn't exist, so nothing is merged and `PATH` stands.
fn merge(login: &str, current: &str) -> Option<String> {
    let mut seen = BTreeSet::new();
    let mut entries: Vec<&str> = Vec::new();
    for entry in login.split(':').chain(current.split(':')) {
        if entry.is_empty() || !seen.insert(entry) {
            continue;
        }
        if PathBuf::from(entry).is_dir() {
            entries.push(entry);
        }
    }
    let merged = entries.join(":");
    (merged != current && !merged.is_empty()).then_some(merged)
}

// Unix-only, like the module: `hydrate` returns before it does anything on
// Windows, and every fixture here is a Unix path. `merge` drops entries that
// are not existing directories, so on Windows `/usr/bin` and `/bin` are
// dropped, the merge comes back empty, and the assertions fail against a
// function that behaved correctly.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn system_only_path_is_detected() {
        assert!(is_system_only("/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(is_system_only("/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin"));
        assert!(!is_system_only("/Users/x/.local/bin:/usr/bin:/bin"));
        assert!(!is_system_only("/opt/homebrew/bin:/usr/bin"));
    }

    #[test]
    fn sentinel_line_survives_rc_file_noise() {
        let output = format!("nvm: v20 loaded\n{SENTINEL}/a:/b\nwelcome back\n");
        assert_eq!(extract(&output).as_deref(), Some("/a:/b"));
        assert_eq!(extract("no sentinel here"), None);
    }

    #[test]
    fn merge_puts_login_first_and_drops_missing_dirs() {
        let merged = merge("/usr/bin:/definitely/not/here", "/bin:/usr/bin").expect("merged");
        assert_eq!(merged, "/usr/bin:/bin");
    }

    #[test]
    fn merge_declines_when_nothing_new_is_reachable() {
        // Fish prints `$PATH` space-separated; the single entry is not a
        // directory, so the current PATH is left alone.
        assert_eq!(merge("/usr/bin /bin", "/usr/bin:/bin"), None);
        assert_eq!(merge("/usr/bin:/bin", "/usr/bin:/bin"), None);
    }
}
