//! Fixtures shared by this crate's unit tests. Test-only: the whole module is
//! `#[cfg(test)]`.
//!
//! # Why a mock app must not use Tauri's own `mock_app`
//!
//! `tauri::test::mock_context` leaves `identifier` empty, and
//! `PathResolver::app_data_dir` is `dirs::data_dir().join(identifier)` — so
//! every mock app built the stock way resolves the *bare* platform app-data
//! root (`~/Library/Application Support` on macOS) as its own data directory.
//! Two consequences, both bad:
//!
//! 1. Tests write into the developer's real app-data root, mixed in with the
//!    installed app's own files.
//! 2. Every such app resolves the *same* directory, so every test that opens
//!    the run ledger opens one shared SQLite file. `cargo test` runs tests on
//!    parallel threads and each mock app opens its own connection, so two
//!    tests writing at once contend and one loses with `SQLITE_BUSY`
//!    ("database is locked"). That failed PRs which had changed no Rust at
//!    all — most often on Windows, whose file locking is the strictest, but
//!    nothing about the sharing is platform-specific.
//!
//! [`build`] fixes it where the path is decided rather than at each thing that
//! writes: `Path::join` with an *absolute* path discards the base, so an
//! identifier that is itself an absolute temp path redirects `app_data_dir` —
//! and every other app-scoped directory Tauri derives from the identifier —
//! into a directory no other test shares. Use these helpers instead of
//! `tauri::test::mock_app`/`mock_context` in every test.
//!
//! # What is left behind
//!
//! A directory here is a name, not a mkdir: whatever writes there creates it,
//! so an app that resolves the path without writing leaves nothing. One that
//! opens the ledger leaves ~600 KB of SQLite and WAL, which across a full run
//! is tens of megabytes, so [`OWNED`] deletes each directory when the test
//! thread that asked for it exits. Cleanup on *app* drop is not available:
//! Tauri's `AppManager` outlives the `App`, so managed state — and any `Drop`
//! guard put in it — never runs. Whatever survives (a Windows handle still
//! open on the ledger, a test harness running tests on the main thread) is
//! wiped by the next test process to draw this pid, and lives under one root
//! per process meanwhile, so `rm -rf` on the glob clears the lot.

use std::cell::RefCell;
use std::path::PathBuf;
use tauri::test::MockRuntime;

/// [`tauri::test::mock_app`] with that isolation applied.
pub(crate) fn mock_app() -> tauri::App<MockRuntime> {
    build(tauri::test::mock_builder())
}

/// The same, for a test that needs its own `Builder` — a command handler, its
/// own managed state. Take this rather than `tauri::test::mock_context`,
/// which is what hands out the shared directory.
pub(crate) fn build(builder: tauri::Builder<MockRuntime>) -> tauri::App<MockRuntime> {
    let mut context = tauri::test::mock_context(tauri::test::noop_assets());
    context.config_mut().identifier = unique_app_data_dir().to_string_lossy().into_owned();
    builder.build(context).expect("a mock app builds")
}

/// The app-data directories this thread asked for, deleted when it exits.
/// libtest gives each test its own thread, so "this thread" is "this test".
struct OwnedDirs(RefCell<Vec<PathBuf>>);

impl Drop for OwnedDirs {
    fn drop(&mut self) {
        for dir in self.0.borrow().iter() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

thread_local! {
    static OWNED: OwnedDirs = const { OwnedDirs(RefCell::new(Vec::new())) };
}

/// Nanos alone can collide across parallel test threads — the atomic counter
/// guarantees uniqueness within the process. Same recipe as `tools.rs`'s
/// `TempTree`.
fn unique_app_data_dir() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch")
        .as_nanos();
    let dir = process_root().join(format!("{n}_{nanos}"));
    OWNED.with(|owned| owned.0.borrow_mut().push(dir.clone()));
    dir
}

/// One root per test process, wiped on first use so a run never inherits what
/// a previous process holding this pid left behind.
fn process_root() -> std::path::PathBuf {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "little_monkey_test_app_data_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use tauri::Manager;

    /// The property the parallel-test lock contention hinged on: no two mock
    /// apps may resolve the same app-data directory, and none of them may
    /// resolve the real one.
    #[test]
    fn two_mock_apps_never_share_an_app_data_directory() {
        let first = super::mock_app();
        let second = super::mock_app();

        let first = first.path().app_data_dir().expect("an app data dir");
        let second = second.path().app_data_dir().expect("an app data dir");

        assert_ne!(first, second, "two mock apps shared one app data dir");
        for dir in [&first, &second] {
            assert!(
                dir.starts_with(std::env::temp_dir()),
                "a mock app's data dir must live under the temp dir, got {dir:?}"
            );
            assert_ne!(
                Some(dir),
                crate::app_paths::data_dir().as_ref(),
                "a mock app resolved the real app data dir"
            );
        }
    }

    /// Two apps writing the same file name write two files, which is what
    /// keeps two parallel tests off one SQLite database.
    #[test]
    fn what_one_mock_app_writes_is_invisible_to_another() {
        let first = super::mock_app();
        let second = super::mock_app();

        for (app, contents) in [(&first, "first"), (&second, "second")] {
            let dir = app.path().app_data_dir().expect("an app data dir");
            std::fs::create_dir_all(&dir).expect("the dir is creatable");
            std::fs::write(dir.join("written.txt"), contents).expect("a test writes here");
        }

        let read = |app: &tauri::App<tauri::test::MockRuntime>| {
            std::fs::read_to_string(app.path().app_data_dir().unwrap().join("written.txt")).unwrap()
        };
        assert_eq!(read(&first), "first");
        assert_eq!(read(&second), "second");
    }

    /// A full run opens tens of megabytes of ledger across these directories,
    /// so each goes when the test that asked for it does. Driven from a
    /// spawned thread because that is the event being tested — a test cannot
    /// outlive its own thread to watch it.
    #[test]
    fn a_finished_test_takes_its_app_data_directory_with_it() {
        let dir = std::thread::spawn(|| {
            let app = super::mock_app();
            let dir = app.path().app_data_dir().expect("an app data dir");
            std::fs::create_dir_all(&dir).expect("the dir is creatable");
            std::fs::write(dir.join("written.txt"), "x").expect("a test writes here");
            dir
        })
        .join()
        .expect("the thread finishes");

        assert!(!dir.exists(), "the app data dir outlived its test: {dir:?}");
    }
}
