//! `monkey processes limits` runs, on this host, as a real process.
//!
//! # Why the shipped binary and not a function call
//!
//! This command was crashing on Windows with `STATUS_STACK_OVERFLOW`
//! (0xc00000fd) before `main`'s first statement, and no in-process test could
//! have caught it: the fault was the *binary's* main-thread stack reserve, which
//! is a field in the PE header and does not exist for a `cargo test` harness
//! thread. A test that called `print_limit_matrix` directly would have passed on
//! the very Windows build where the shipped command died — see
//! `reserve_a_unix_sized_main_stack_for_the_cli` in `build.rs` for the cause.
//!
//! So this spawns the real executable and asserts what a user would see. It is
//! cheap (one short-lived process) and it is the only shape that can fail for the
//! reason the original bug had.
//!
//! It is also the acceptance test behind the CI step that reports each runner's
//! enforcement backend. That step no longer carries `continue-on-error`, so a
//! crashed command fails the build; an environment-dependent *fallback* backend
//! still passes, which is the distinction that matters. This test states the same
//! rule locally: the command must produce the static contract and this host's
//! real answer, whichever backend that answer names.

use std::path::PathBuf;
use std::process::Command;

/// A HOME/profile root of this test's own.
///
/// `app_data_dir()` follows the platform's data directory, which follows `HOME`
/// (and `LOCALAPPDATA` on Windows). Left alone, a test run would resolve the
/// developer's real ledger directory — harmless for this read-only command, and
/// exactly the habit that makes some later test destructive.
fn isolated_home() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "little-monkey-cli-limits-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).expect("a temp home");
    root
}

fn run_limits(extra: &[&str]) -> std::process::Output {
    let home = isolated_home();
    let mut command = Command::new(env!("CARGO_BIN_EXE_monkey-cli"));
    command.arg("processes").arg("limits").args(extra);
    command.env("HOME", &home);
    command.env("USERPROFILE", &home);
    command.env("LOCALAPPDATA", &home);
    command.env("XDG_DATA_HOME", home.join("data"));
    let output = command.output().expect("the CLI binary runs");
    let _ = std::fs::remove_dir_all(&home);
    output
}

/// The regression test for the Windows crash: the process must *finish*.
///
/// Asserted on the status rather than only on the output, because the crash
/// produced an empty stdout and a non-zero status — a check that only looked for
/// a substring would have reported "output missing" and sent the next reader
/// looking at the formatter.
#[test]
fn the_limits_command_exits_successfully_rather_than_overflowing_its_stack() {
    let output = run_limits(&[]);
    assert!(
        output.status.success(),
        "`monkey processes limits` failed: status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Both halves of what the command is for: the static contract, and this host.
///
/// The host block is the half that has to *construct* a real
/// `ResourceController` — creating a cgroup scope on Linux or a job object on
/// Windows — so asserting it is what proves the command reads production
/// capability information rather than printing a table of constants.
#[test]
fn the_limits_command_prints_the_static_matrix_and_this_host_s_real_backend() {
    let output = run_limits(&[]);
    assert!(output.status.success(), "the command must run");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The static half: every kind against every limit.
    for kind in little_monkey_lib::process_table::ProcessKind::ALL {
        assert!(
            stdout.contains(kind.as_str()),
            "the static matrix is missing {}:\n{stdout}",
            kind.as_str()
        );
    }

    // The host half, whatever it turns out to be here.
    assert!(
        stdout.contains("this host: "),
        "the command printed no host backend:\n{stdout}"
    );
    assert!(
        stdout.contains("tree owned by: "),
        "the command printed no tree primitive:\n{stdout}"
    );
    let backend = little_monkey_lib::resource_control::ResourceController::new(
        little_monkey_lib::resource_control::probe_limits(),
    )
    .capabilities()
    .backend;
    assert!(
        stdout.contains(&format!("this host: {backend}")),
        "the command and the library disagree about this host's backend ({backend}):\n{stdout}"
    );
}

/// The JSON form is what a script reads, and it must survive the same path.
#[test]
fn the_limits_command_emits_parseable_json() {
    let output = run_limits(&["--json"]);
    assert!(
        output.status.success(),
        "`monkey processes limits --json` failed: {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the --json form is JSON");
    let rows = parsed.as_array().expect("an array of kind/limit rows");
    assert_eq!(
        rows.len(),
        little_monkey_lib::process_table::ProcessKind::ALL.len()
            * little_monkey_lib::process_table::ProcessLimitKind::ALL.len(),
        "every kind must be answered for every limit"
    );
}
