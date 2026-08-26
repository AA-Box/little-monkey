use little_monkey_lib::execution_target::{
    ExecutionTarget, RequiredCapabilities, RunRequest, SshRunnerConfig, SshRunnerTarget,
    TargetRunHandle, TargetRunStatus, WorkspacePolicy, WorkspaceTransfer,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TestTempDir {
    path: PathBuf,
}

impl TestTempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "little-monkey-ssh-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create SSH E2E temp directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set when SSH E2E is required"))
}

fn wait_for_status(
    target: &dyn ExecutionTarget,
    handle: &TargetRunHandle,
    wanted: &[TargetRunStatus],
    timeout: Duration,
) -> TargetRunStatus {
    let deadline = Instant::now() + timeout;
    loop {
        let status = target.status(handle).expect("SSH runner status");
        if wanted.contains(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "SSH runner did not reach {wanted:?}; last status was {status:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn target(id: &str, known_hosts: &Path, key_file: &Path, port: u16) -> SshRunnerTarget {
    SshRunnerTarget::new(
        id.to_string(),
        "SSH acceptance runner".to_string(),
        SshRunnerConfig {
            host: "127.0.0.1".to_string(),
            user: Some("monkey".to_string()),
            port: Some(port),
            key_file: Some(key_file.to_path_buf()),
            known_hosts: known_hosts.to_path_buf(),
            jump_host: None,
            runner_binary: "monkey".to_string(),
        },
        PathBuf::from("/unused/ssh-runner-client-state"),
    )
    .expect("construct SSH target")
}

fn submit(
    target: &SshRunnerTarget,
    snapshot: little_monkey_lib::execution_target::ExecutionTargetSnapshot,
    root: &Path,
    workspace_id: &str,
    run_id: &str,
    command: Vec<String>,
    wall_time_ms: u64,
) -> TargetRunHandle {
    let transfer =
        WorkspaceTransfer::from_workspace(root, workspace_id).expect("workspace transfer");
    let workspace = target
        .prepare_workspace(&transfer, WorkspacePolicy::Persistent)
        .expect("prepare SSH workspace");
    target
        .submit_run(RunRequest {
            run_id: run_id.to_string(),
            target: snapshot,
            required_capabilities: RequiredCapabilities {
                shell: true,
                disposable_workspace: true,
                ..RequiredCapabilities::default()
            },
            workspace,
            command,
            environment: BTreeMap::new(),
            wall_time_ms,
            max_artifact_bytes: 4 * 1024 * 1024,
            workspace_transfer: Some(transfer),
            input_files: Vec::new(),
            run_spec: None,
        })
        .expect("submit SSH runner task")
}

#[test]
fn real_ssh_runner_covers_transfer_reconnect_result_pause_resume_and_cancel() {
    if std::env::var("LITTLE_MONKEY_REQUIRE_RUNNER_SSH_E2E").as_deref() != Ok("1") {
        eprintln!("skipping real SSH runner acceptance test; set LITTLE_MONKEY_REQUIRE_RUNNER_SSH_E2E=1 to require it");
        return;
    }

    let known_hosts = PathBuf::from(required_env("LITTLE_MONKEY_RUNNER_SSH_KNOWN_HOSTS"));
    let key_file = PathBuf::from(required_env("LITTLE_MONKEY_RUNNER_SSH_KEY_FILE"));
    let port = required_env("LITTLE_MONKEY_RUNNER_SSH_PORT")
        .parse::<u16>()
        .expect("valid SSH fixture port");

    assert!(known_hosts.is_absolute());
    assert!(key_file.is_absolute());

    let first = target("ssh-acceptance", &known_hosts, &key_file, port);
    let snapshot = first.probe().expect("probe real SSH runner");
    assert_eq!(snapshot.identity.platform, "linux");
    assert!(snapshot.identity.verified_identity.is_some());
    assert!(snapshot.identity.capabilities.shell);
    assert!(snapshot.identity.capabilities.disposable_workspace);

    let workspace = TestTempDir::new("reconnect");
    std::fs::write(workspace.path().join("input.txt"), b"from-client\n").unwrap();
    let handle = submit(
        &first,
        snapshot.clone(),
        workspace.path(),
        "ssh-reconnect-workspace",
        "ssh-reconnect-run",
        vec![
            "sh".to_string(),
            "-lc".to_string(),
            "sleep 2; cat input.txt > result.txt; printf 'remote\\n' >> result.txt".to_string(),
        ],
        20_000,
    );
    wait_for_status(
        &first,
        &handle,
        &[TargetRunStatus::Queued, TargetRunStatus::Running],
        Duration::from_secs(5),
    );

    // Closing the control connection must not imply cancellation. A fresh SSH
    // transport attaches to the durable runner state and observes the same run.
    drop(first);
    let reconnected = target("ssh-acceptance", &known_hosts, &key_file, port);
    reconnected.probe().expect("re-probe after transport loss");
    assert_eq!(
        wait_for_status(
            &reconnected,
            &handle,
            &[TargetRunStatus::Succeeded],
            Duration::from_secs(15),
        ),
        TargetRunStatus::Succeeded
    );
    let result = reconnected
        .workspace_result(&handle)
        .expect("retrieve SSH workspace result after reconnect");
    let result_file = result
        .new_files
        .iter()
        .find(|file| file.path == "result.txt")
        .expect("remote result file");
    assert_eq!(result_file.bytes, b"from-client\nremote\n");
    assert!(reconnected
        .artifacts(&handle)
        .expect("SSH artifacts")
        .iter()
        .any(|artifact| {
            artifact.label == "result.txt" && artifact.sha256 == result_file.sha256
        }));

    let pause_workspace = TestTempDir::new("pause");
    std::fs::write(pause_workspace.path().join("input.txt"), b"pause\n").unwrap();
    let pause_handle = submit(
        &reconnected,
        snapshot.clone(),
        pause_workspace.path(),
        "ssh-pause-workspace",
        "ssh-pause-run",
        vec![
            "sh".to_string(),
            "-lc".to_string(),
            "sleep 3; printf 'resumed\\n' > resumed.txt".to_string(),
        ],
        20_000,
    );
    wait_for_status(
        &reconnected,
        &pause_handle,
        &[TargetRunStatus::Running],
        Duration::from_secs(5),
    );
    reconnected.pause(&pause_handle).expect("pause SSH run");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        reconnected
            .status(&pause_handle)
            .expect("paused SSH status"),
        TargetRunStatus::Running,
        "paused runner processes remain non-terminal"
    );
    reconnected.resume(&pause_handle).expect("resume SSH run");
    assert_eq!(
        wait_for_status(
            &reconnected,
            &pause_handle,
            &[TargetRunStatus::Succeeded],
            Duration::from_secs(15),
        ),
        TargetRunStatus::Succeeded
    );

    let cancel_workspace = TestTempDir::new("cancel");
    std::fs::write(cancel_workspace.path().join("input.txt"), b"cancel\n").unwrap();
    let cancel_handle = submit(
        &reconnected,
        snapshot,
        cancel_workspace.path(),
        "ssh-cancel-workspace",
        "ssh-cancel-run",
        vec![
            "sh".to_string(),
            "-lc".to_string(),
            "sleep 30; printf 'should-not-exist\\n' > cancelled.txt".to_string(),
        ],
        40_000,
    );
    wait_for_status(
        &reconnected,
        &cancel_handle,
        &[TargetRunStatus::Running],
        Duration::from_secs(5),
    );
    reconnected.cancel(&cancel_handle).expect("cancel SSH run");
    assert_eq!(
        wait_for_status(
            &reconnected,
            &cancel_handle,
            &[TargetRunStatus::Cancelled],
            Duration::from_secs(10),
        ),
        TargetRunStatus::Cancelled
    );
}
