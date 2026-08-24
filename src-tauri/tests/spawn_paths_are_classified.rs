//! Every native process this tree can create is classified, and adding one is
//! not silent.
//!
//! K4's enforcement is only as good as the set of spawn sites that go through
//! it, and that set is not visible from any single file: `Command::new` appears
//! in roughly fifty modules, from the agent shell to a `git` invocation to the
//! daemon's own supervisor. Nothing stopped a new agent-controlled spawn path
//! from being added beside them and quietly bypassing the resource controller —
//! the code would compile, the tests would pass, and the process table would
//! simply not know about it.
//!
//! So this is a **source test**, not a behaviour test. It reads the tree, finds
//! every file that creates a native process, and requires each one to appear in
//! the table below with a stated classification. A new spawn site in a new file
//! fails this test until someone writes down which of the three it is.
//!
//! # The three classifications
//!
//! - [`Class::AgentShell`] — a command whose text comes from a model or from the
//!   user's own configuration, executed on their behalf. These **must** enter the
//!   resource infrastructure. The test asserts they do, by requiring the file to
//!   reference the controller.
//! - [`Class::ResourceInfrastructure`] — the machinery itself: the confinement
//!   backends, the limit installer, the signal primitives, and the tests that
//!   spawn a child in order to prove one of them works.
//! - [`Class::HostUtility`] — a bounded, synchronous invocation of a known host
//!   program with arguments this app composed: `git rev-parse`, a version probe,
//!   a package manager the user asked to run. Each carries the reason it is not
//!   agent-controlled, because "it is only `git`" is exactly the reasoning that
//!   would let an agent-authored argument through one day.
//! - [`Class::ManagedService`] — a long-lived process this app owns and
//!   supervises with its own lifecycle: a model server, Chromium, the daemon.
//!   These have owner-sourced bounds rather than `ProcessLimits` ones, which is
//!   what `ProcessKind::limit_support` reports for them.
//!
//! # What this test does not claim
//!
//! It does not prove a classification is *correct* — only that one exists and
//! that the `AgentShell` files reach the controller. A file classified
//! `HostUtility` that starts composing model-authored arguments would still pass.
//! That is a smaller guarantee than "no bypass is possible", and it is the one a
//! source scan can honestly make: the alternative is a whole-program taint
//! analysis, and a test that pretends to more than it checks is worse than one
//! that states its limit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    AgentShell,
    ResourceInfrastructure,
    HostUtility,
    ManagedService,
}

/// Every file in `src-tauri/src` that creates a native process.
///
/// Ordered as the scan finds them so a diff to this list reads as a diff to the
/// tree. The note on each is what a future reader needs: not what the file does,
/// but why it is or is not agent-controlled.
const CLASSIFIED: &[(&str, Class, &str)] = &[
    // --- Agent-controlled: model- or user-authored command text --------------
    (
        "workspace_shell.rs",
        Class::AgentShell,
        "the confined shell every agent command runs through, on both clients",
    ),
    (
        "tools.rs",
        Class::AgentShell,
        "the `run_shell` tool; its child comes from `workspace_shell::spawn_foreground`",
    ),
    (
        "background_shell.rs",
        Class::AgentShell,
        "`run_shell` with `run_in_background`; its child comes from `spawn_background`",
    ),
    (
        "verify.rs",
        Class::AgentShell,
        "verify commands come from the user's Settings rather than from a model, and are \
         executed on the agent's behalf after a turn writes files; the command's own \
         timeout is its wall limit and the class defaults bound the tree",
    ),
    (
        "hooks.rs",
        Class::AgentShell,
        "user-configured hook commands, run at agent lifecycle points; same controller and \
         same class defaults as `verify.rs`, with the hook deadline as the wall limit",
    ),
    (
        "sandbox.rs",
        Class::AgentShell,
        "the opt-in disposable-copy run; the Seatbelt/Landlock boundary confines it in \
         space and the controller bounds it in memory, process count and time. Windows \
         reaches its own job object through `sandbox_windows::run_confined`",
    ),
    // --- The machinery itself ------------------------------------------------
    (
        "os_limits.rs",
        Class::ResourceInfrastructure,
        "installs `setrlimit` bounds before `exec`; spawns only in its own tests",
    ),
    (
        "os_signal.rs",
        Class::ResourceInfrastructure,
        "process-group termination; spawns only in its own tests",
    ),
    (
        "process_table.rs",
        Class::ResourceInfrastructure,
        "spawns only in its own tests, to prove a lifecycle against a real child",
    ),
    (
        "process_commands.rs",
        Class::ResourceInfrastructure,
        "the startup reclaim; spawns only in its own tests, to prove it never signals a pid \
         whose identity it cannot check",
    ),
    (
        "sandbox_windows.rs",
        Class::ResourceInfrastructure,
        "the AppContainer + job-object launcher; creates suspended, assigns, then resumes",
    ),
    (
        "sandbox_linux.rs",
        Class::ResourceInfrastructure,
        "Landlock and seccomp confinement; spawns only in its own tests",
    ),
    (
        "resource_control.rs",
        Class::ResourceInfrastructure,
        "the contract itself; spawns only in its own tests, to prove attachment and \
         termination against a real child",
    ),
    (
        "managed_spawn_windows.rs",
        Class::ResourceInfrastructure,
        "the one Windows spawn ordering — created suspended, assigned to its job, membership \
         read back, and only then resumed. Every agent-controlled Windows spawn goes through \
         it, so the window in which a workload could create a descendant outside its job does \
         not exist; it spawns on its own only in its own tests",
    ),
    (
        "orphan_reclaim.rs",
        Class::ResourceInfrastructure,
        "what a restart may conclude about work a previous session left; spawns only in its \
         own tests, to reclaim a real tree a simulated crash left behind",
    ),
    (
        "process_tree.rs",
        Class::ResourceInfrastructure,
        "the tree enumeration and liveness the supervisor measures and signals through; \
         spawns only in its own tests, to prove a real child's states against the kernel",
    ),
    // --- Managed services with owner-sourced lifecycles ----------------------
    (
        "llama.rs",
        Class::ManagedService,
        "the bundled llama.cpp server, supervised by its own health and restart policy",
    ),
    (
        "ollama.rs",
        Class::ManagedService,
        "a host-installed Ollama server this app starts and probes",
    ),
    (
        "m3_production.rs",
        Class::ManagedService,
        "runtime process management for the production model service",
    ),
    (
        "m4_runtime.rs",
        Class::ManagedService,
        "workflow node execution, bounded by the definition's own per-node budgets",
    ),
    (
        "browser_worker.rs",
        Class::ManagedService,
        "the Chromium a browser session owns. Split by resource rather than by owner: the \
         resource controller holds the tree's memory and process count and reclaims it, and \
         the session keeps the clock, the action budget and the disk budget, which no \
         controller can express. One resource, one owner",
    ),
    (
        "mcp.rs",
        Class::ManagedService,
        "a stdio MCP server the user configured, supervised by its own transport",
    ),
    (
        "terminal.rs",
        Class::ManagedService,
        "the user's own interactive PTY, which is the user driving their machine",
    ),
    (
        "generation.rs",
        Class::ManagedService,
        "the bundled image/video generation server",
    ),
    (
        "generation_commands.rs",
        Class::ManagedService,
        "lifecycle commands for that same server",
    ),
    (
        "quantization.rs",
        Class::ManagedService,
        "a bundled quantization tool run against a model the user selected",
    ),
    (
        "desktop_control.rs",
        Class::ManagedService,
        "platform input backends for Control Desktop, gated by its own allowlist",
    ),
    (
        "m7_companion.rs",
        Class::ManagedService,
        "mobile companion pairing and capture helpers",
    ),
    (
        "execution_target.rs",
        Class::ManagedService,
        "the configured execution-target runner owns its child lifecycle inside the target's \
         transport and durable wall-time boundary",
    ),
    (
        "knowledge_service.rs",
        Class::ManagedService,
        "knowledge pipeline helpers over user-selected sources",
    ),
    (
        "knowledge_adapters.rs",
        Class::ManagedService,
        "per-format extraction helpers for the same pipeline",
    ),
    (
        "studio_tools.rs",
        Class::ManagedService,
        "a Studio sidecar tool binary named by an installed manifest rather than by a \
         model; kept resident under its own LRU and residency budget, which is an \
         owner-sourced bound rather than a `ProcessLimits` one",
    ),
    // --- Bounded host utilities ---------------------------------------------
    (
        "git.rs",
        Class::HostUtility,
        "`git` with arguments this app composes; paths are workspace-resolved first",
    ),
    (
        "agent_worktrees.rs",
        Class::HostUtility,
        "`git worktree`, host-owned precisely so the model-authored shell never manages it",
    ),
    (
        "native_skills.rs",
        Class::HostUtility,
        "`git clone`/`fetch` for installing a skill, with a hardened config this file \
         composes: no system config, no hooks, HTTPS only, no credential prompt. It was \
         classified as an agent shell on the reading that it runs a skill's own \
         executable, and it does not — a skill's program is invoked through the shell \
         tool like anything else, and `git` is the only process this file creates",
    ),
    (
        "system.rs",
        Class::HostUtility,
        "platform probes: version, hardware and capability queries with fixed arguments",
    ),
    (
        "self_integrity.rs",
        Class::HostUtility,
        "signature and notarization checks of this app's own bundle",
    ),
    (
        "update_rollback.rs",
        Class::HostUtility,
        "installer and rollback invocations with fixed arguments",
    ),
    (
        "login_path.rs",
        Class::HostUtility,
        "reads the user's login shell PATH by running it once, non-interactively",
    ),
    (
        "daemon_commands.rs",
        Class::HostUtility,
        "starts and stops the daemon service from the desktop app",
    ),
    (
        "m5_delivery/git.rs",
        Class::HostUtility,
        "`git` for delivery: branch, commit, push, with composed arguments",
    ),
    (
        "m5_delivery/github.rs",
        Class::HostUtility,
        "`gh` for pull-request delivery, with composed arguments",
    ),
    (
        "m5_delivery/reviewer.rs",
        Class::HostUtility,
        "`gh` review operations against a delivery this app created",
    ),
    (
        "imessage_helper/messages.rs",
        Class::HostUtility,
        "`osascript` with argv-passed values, never string interpolation — the recipient \
         and the message text are arguments the script reads, so no text anyone sends can \
         become AppleScript. Runs in the operator-installed helper, never in the daemon",
    ),
];

/// The `monkey-cli` binary's own spawn sites, classified on the same terms.
///
/// A separate list because the binary is a separate target with a separate
/// audience: everything under `daemon/` is the resident supervisor, which owns
/// its children's budgets through the job recipe rather than through
/// `ProcessLimits`.
///
/// `tools_cli.rs` is deliberately absent, and its absence is the finding: the
/// CLI's `run_shell` creates no process of its own, because it goes through
/// `workspace_shell::run_to_output` — the same entry point the desktop uses. That
/// is what "the authority boundary cannot drift by client" looks like from a
/// source scan.
const CLASSIFIED_CLI: &[(&str, Class, &str)] = &[
    (
        "acp.rs",
        Class::ManagedService,
        "an ACP agent process the user configured",
    ),
    (
        "launcher.rs",
        Class::HostUtility,
        "re-execs this binary to attach a terminal session",
    ),
    (
        "cmds.rs",
        Class::HostUtility,
        "top-level command dispatch helpers with fixed arguments",
    ),
    (
        "embed_cli.rs",
        Class::HostUtility,
        "embedding helpers over user-selected inputs",
    ),
    (
        "managed_model_cli.rs",
        Class::HostUtility,
        "managed-runtime install and probe operations",
    ),
    (
        "task.rs",
        Class::HostUtility,
        "autonomous executor Git inspection and verification use fixed arguments",
    ),
    (
        "daemon/callback_exposure.rs",
        Class::ManagedService,
        "the operator's own tunnel client, started and supervised by the daemon \
         so a webhook provider can reach a listener that binds loopback. Every \
         argument is a literal from a fixed per-provider template except a \
         validated absolute path and a port number; the credential is not an \
         argument at all, it goes in the environment. Nothing model-authored \
         reaches it -- React names a provider from a closed set of one, and the \
         argv is built here",
    ),
    (
        "daemon/mod.rs",
        Class::ManagedService,
        "daemon service lifecycle",
    ),
    (
        "daemon/peer_live.rs",
        Class::HostUtility,
        "the opt-in peer live-validation test mints its own self-signed \
         certificate with `openssl`; every argument is a literal and the only \
         paths are inside the test's own temporary directory",
    ),
    (
        "daemon/engine.rs",
        Class::ManagedService,
        "the job runner: each child is bounded by its recipe's own budgets, enforced by \
         the daemon's sampling watchdog",
    ),
    (
        "daemon/service.rs",
        Class::ManagedService,
        "OS service registration and control",
    ),
    (
        "daemon/worktree.rs",
        Class::HostUtility,
        "`git worktree` for a job's isolated checkout",
    ),
    (
        "daemon/remote/desktop.rs",
        Class::ManagedService,
        "remote-controlled desktop operations, gated by the pairing's granted scope",
    ),
    (
        "daemon/remote/api.rs",
        Class::HostUtility,
        "the ignored Windows acceptance test launches the repository-owned WPF fixture with fixed arguments",
    ),
    (
        "daemon/adapters/signal.rs",
        Class::ManagedService,
        "the signal-cli transport the user configured",
    ),
    (
        "daemon/adapters/imessage.rs",
        Class::HostUtility,
        "`osascript` with argv-passed values, never string interpolation",
    ),
    (
        "daemon/channel_agent_e2e.rs",
        Class::ResourceInfrastructure,
        "the channels end-to-end acceptance test; spawns only under `cfg(test)`, to locate \
         the CLI binary and re-exec itself as the agent under test",
    ),
];

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// How a file betrays that it creates a native process.
///
/// `Command::new` is how both `std::process` and `tokio::process` build one, so
/// one marker covers the ordinary case however the type was imported or aliased.
/// The three below it are the ways to make a process *without* that builder — the
/// syscalls the confinement backends call directly — and they are listed because
/// a new file that reached for one of them would otherwise be a spawn site this
/// scan does not see, which is the exact failure the scan exists to prevent.
const SPAWN_MARKERS: &[&str] = &[
    "Command::new",
    "CreateProcessW",
    "posix_spawn",
    "libc::fork",
];

/// Every `.rs` file under `root` that creates a native process, relative to `root`.
fn files_that_spawn(root: &Path, skip_dir: Option<&str>) -> BTreeSet<String> {
    fn walk(dir: &Path, root: &Path, skip_dir: Option<&str>, found: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if skip_dir.is_some_and(|skip| path.file_name().is_some_and(|name| name == skip)) {
                    continue;
                }
                walk(&path, root, skip_dir, found);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if SPAWN_MARKERS.iter().any(|marker| text.contains(marker)) {
                found.insert(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }

    let mut found = BTreeSet::new();
    walk(root, root, skip_dir, &mut found);
    found
}

#[test]
fn every_file_that_creates_a_native_process_is_classified() {
    let root = source_root();
    // `bin/` is the CLI target, classified in its own list below.
    let found = files_that_spawn(&root, Some("bin"));
    let declared: BTreeSet<String> = CLASSIFIED
        .iter()
        .map(|(path, _, _)| (*path).to_string())
        .collect();

    let unclassified: Vec<&String> = found.difference(&declared).collect();
    assert!(
        unclassified.is_empty(),
        "these files create a native process and are not classified in \
         tests/spawn_paths_are_classified.rs: {unclassified:?}\n\n\
         Add each one with a class and a reason. If it is agent-controlled it must enter \
         the resource infrastructure; if it is a host utility, the reason is what stops \
         someone extending it with model-authored arguments later."
    );

    let stale: Vec<&String> = declared.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "these files are classified but no longer create a native process; drop them from \
         the list so it keeps describing the tree: {stale:?}"
    );
}

#[test]
fn every_cli_file_that_creates_a_native_process_is_classified() {
    let root = source_root().join("bin/monkey-cli");
    let found = files_that_spawn(&root, None);
    let declared: BTreeSet<String> = CLASSIFIED_CLI
        .iter()
        .map(|(path, _, _)| (*path).to_string())
        .collect();

    let unclassified: Vec<&String> = found.difference(&declared).collect();
    assert!(
        unclassified.is_empty(),
        "unclassified CLI spawn sites: {unclassified:?}"
    );
    let stale: Vec<&String> = declared.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "stale CLI spawn classifications: {stale:?}"
    );
}

/// The one classification with a testable obligation attached.
///
/// An `AgentShell` file either reaches the resource controller itself or goes
/// through `workspace_shell`, which does. Checked by reference rather than by
/// behaviour, which is the honest limit of a source scan — but it is enough to
/// fail a new agent spawn path that reaches for `Command::new` directly.
#[test]
fn every_agent_controlled_spawn_path_reaches_the_resource_infrastructure() {
    let root = source_root();
    let mut unrouted = Vec::new();
    for (path, class, _) in CLASSIFIED.iter().chain(CLASSIFIED_CLI.iter()) {
        if *class != Class::AgentShell {
            continue;
        }
        let full = if root.join(path).exists() {
            root.join(path)
        } else {
            root.join("bin/monkey-cli").join(path)
        };
        let text = std::fs::read_to_string(&full).unwrap_or_default();
        let routed = text.contains("resource_control")
            || text.contains("workspace_shell::spawn_")
            || text.contains("workspace_shell::run_to_output");
        if !routed {
            unrouted.push(*path);
        }
    }

    // There is no exception list. There was one — `verify.rs`, `hooks.rs`,
    // `sandbox.rs` and `native_skills.rs` — and it is worth recording what
    // emptying it took, because three of the four were real and one was a
    // misreading. The verify runner, the hook runner and the sandboxed run each
    // had a deadline and nothing else: a `sleep` racing the capture, and a
    // process-group kill if it won. Each now resolves the same `EffectiveLimits`,
    // installs the same containment before the first instruction, fails closed if
    // it cannot confirm it, and gets its deadline as a wall limit so one call
    // reclaims the tree whichever bound fired. `native_skills.rs` was never an
    // agent shell at all: its only child is `git`, and it is classified as the
    // host utility it is.
    assert!(
        unrouted.is_empty(),
        "these agent-controlled spawn paths do not reach the resource infrastructure: \
         {unrouted:?}"
    );
}

/// On Windows, an agent-controlled workload's first instruction must not run
/// before its job holds it.
///
/// # Why this is a source test and not a behaviour test
///
/// The window it guards is microseconds wide, so a behaviour test would be
/// asserting that a race did not happen to be lost this time. The ordering
/// itself is the property, and the ordering lives in exactly one place —
/// `managed_spawn_windows`, reached through
/// `ResourceController::spawn_contained_*`. Every owner that builds its own
/// `Command` must go through it, and this fails a new one that does not.
///
/// The two exceptions are not weaker, they are the *stronger* form: the agent
/// shells and the disposable-copy sandbox call `CreateProcessW` themselves,
/// because an AppContainer's capabilities can only be handed to a process
/// through a `STARTUPINFOEX` attribute list that no `Command` can build. Those
/// sites create suspended and assign before resuming too, in
/// `sandbox_windows::spawn_confined`.
#[test]
fn every_agent_controlled_windows_spawn_is_contained_before_its_first_instruction() {
    let root = source_root();
    let mut unordered = Vec::new();
    for (path, class, _) in CLASSIFIED.iter().chain(CLASSIFIED_CLI.iter()) {
        if *class != Class::AgentShell {
            continue;
        }
        let full = if root.join(path).exists() {
            root.join(path)
        } else {
            root.join("bin/monkey-cli").join(path)
        };
        let text = std::fs::read_to_string(&full).unwrap_or_default();
        let ordered = text.contains("spawn_contained_")
            // The `CreateProcessW` sites, and the owners that delegate to them.
            || text.contains("sandbox_windows::spawn_confined")
            || text.contains("sandbox_windows::run_confined")
            || text.contains("workspace_shell::spawn_")
            || text.contains("workspace_shell::run_to_output");
        if !ordered {
            unordered.push(*path);
        }
    }
    assert!(
        unordered.is_empty(),
        "these agent-controlled spawn paths do not establish their Windows job before the          workload's first instruction: {unordered:?}"
    );
}

/// Every file that owns an agent-controlled native process **tree**.
///
/// A separate list from the classes above, because the obligation is separate:
/// `Class` says what kind of thing a spawn site is, and this says which sites own
/// a fan-out that a single pid does not describe. Those are the sites where "kill
/// the child" is not "reclaim the workload", and each of them must have a
/// declared resource owner.
///
/// `browser_worker.rs` is why the two lists cannot be one. It is a
/// `ManagedService` — Chromium's lifecycle is its own, and its action and disk
/// budgets are browser-domain policy no `ProcessLimits` field expresses — and it
/// is simultaneously a process-tree owner whose memory and process count belong
/// to the controller. Classifying it as infrastructure to satisfy one rule would
/// have made the other rule wrong.
const TREE_OWNERS: &[(&str, &str)] = &[
    (
        "workspace_shell.rs",
        "the confined shell's process group / cgroup / job, foreground and background",
    ),
    (
        "background_shell.rs",
        "the watcher that owns a background command's controller after its turn ends",
    ),
    (
        "browser_worker.rs",
        "Chromium's renderer, GPU, network and utility children",
    ),
];

/// Each process-tree owner names a resource owner, and it is this one.
///
/// The invariant K4 turns on, as a source contract: **no agent-controlled native
/// process tree may run without a declared resource owner.** A new fan-out spawn
/// site added without one fails here, rather than shipping as a tree nothing
/// bounds and nothing can report on.
#[test]
fn every_owner_of_a_native_process_tree_declares_its_resource_owner() {
    let root = source_root();
    for (path, what) in TREE_OWNERS {
        let text = std::fs::read_to_string(root.join(path)).unwrap_or_default();
        assert!(
            text.contains("resource_control"),
            "{path} owns a native process tree ({what}) and does not reach the resource \
             controller; a tree with no declared resource owner is the gap K4 exists to close"
        );
        assert!(
            text.contains("terminate_tree"),
            "{path} owns a native process tree ({what}) and never reclaims it as a tree; \
             killing the direct child leaves the fan-out running"
        );
    }
}

/// A limit kill reaches the ledger as typed fields, never only as prose.
///
/// `ExitStatus::LimitExceeded` may be *constructed* in exactly one place —
/// [`ProcessExit::limit_exceeded`], which derives the human sentence from the
/// breach so the two can never disagree. Every other site has to go through it,
/// which is what makes "a resource kill always carries which limit, what was
/// configured and what was observed" a property of the code rather than of each
/// author's care.
///
/// The daemon genuinely used to parse that sentence back out with a marker
/// string, which is what a prose-only record forces a reader into.
#[test]
fn a_limit_exceeded_exit_is_only_ever_built_from_a_typed_breach() {
    let root = source_root();
    let mut offenders = Vec::new();
    walk_rust_files(&root, &mut |path, text| {
        // The definition itself, and the parser that turns a stored string back
        // into the enum, are the two legitimate mentions.
        let is_definition = path.ends_with("process_table.rs");
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.starts_with("//") || line.starts_with("///") {
                continue;
            }
            // A struct-literal field, which is the only way to build the exit
            // without going through the constructor.
            if line.contains("status: ExitStatus::LimitExceeded")
                || line.contains("status: crate::process_table::ExitStatus::LimitExceeded")
            {
                if is_definition {
                    continue;
                }
                offenders.push(format!("{path}:{}", number + 1));
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "these build a limit-exceeded exit by hand instead of through \
         `ProcessExit::limit_exceeded`, so nothing guarantees the typed breach is beside the \
         prose: {offenders:?}"
    );
}

/// Bounded capture stays bounded *while the child runs*.
///
/// `wait_with_output` and `read_to_end` both retain everything before returning
/// anything, so a command printing a gigabyte takes a gigabyte of this process's
/// heap with it — which is what the shell paths used to do. The bound has to be
/// applied as bytes arrive, and the pipes have to keep draining, or a child that
/// fills a 64 KiB pipe deadlocks against a reader that is not reading.
///
/// A source contract because the failure is invisible in a test: a bounded
/// implementation and an unbounded one produce the same output for every input
/// small enough to assert on.
#[test]
fn the_bounded_capture_paths_never_collect_a_whole_stream() {
    const BOUNDED: &[&str] = &[
        "workspace_shell.rs",
        "background_shell.rs",
        "output_cap.rs",
        "tools.rs",
        "verify.rs",
        "hooks.rs",
        "sandbox.rs",
    ];
    let root = source_root();
    let mut offenders = Vec::new();
    for path in BOUNDED {
        let full = root.join(path);
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            // Prose about the rule is how the rule stays explained.
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if trimmed.contains("wait_with_output(") || trimmed.contains(".read_to_end(") {
                offenders.push(format!("{path}:{}", number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these bounded-capture paths collect a whole stream before returning, so the app's \
         retained memory is whatever the child chose to print: {offenders:?}"
    );
}

/// Visit every `.rs` file under `root` with its path (relative) and contents.
fn walk_rust_files(root: &Path, visit: &mut dyn FnMut(&str, &str)) {
    fn walk(dir: &Path, root: &Path, visit: &mut dyn FnMut(&str, &str)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, visit);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            visit(&relative, &text);
        }
    }
    walk(root, root, visit);
}
