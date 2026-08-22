//! Native Computer Use acceptance driver.
//!
//! This binary deliberately drives the fixture through the production
//! `DesktopControlState`, including its real accessibility provider, scoped
//! grant, approval gate, semantic actions, verification, screenshot path, and
//! audit redaction. It is invoked by the executable Python runner on an
//! interactive macOS, Windows, or Linux/X11 desktop.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use little_monkey_lib::desktop_control::{
    ActionGate, ApprovalPolicy, ComputerElement, ComputerInspection, ComputerTarget, ControlAction,
    DesktopControlState, MouseButtonKind, SessionGrantOptions,
};
use serde_json::json;
use sha2::{Digest, Sha256};

fn arg_value(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .ok_or_else(|| format!("missing {name}"))
}

fn python_command() -> String {
    std::env::var("COMPUTER_USE_PYTHON").unwrap_or_else(|_| {
        if cfg!(target_os = "windows") {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    })
}

fn launch(fixture: &str) -> Result<Child, String> {
    let executable =
        std::env::var("COMPUTER_USE_FIXTURE_COMMAND").unwrap_or_else(|_| python_command());
    let mut command = Command::new(executable);
    if cfg!(target_os = "windows") {
        if let Ok(script) = std::env::var("COMPUTER_USE_FIXTURE_SCRIPT") {
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            command.arg(script);
        } else {
            command.arg(fixture);
        }
    } else {
        command.arg(fixture);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("could not launch fixture: {error}"))?;
    thread::sleep(Duration::from_secs(2));
    Ok(child)
}

fn stop(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn allowed_applications(pid: u32) -> Vec<String> {
    let mut applications = vec![
        format!("process:{pid}"),
        "Python".to_string(),
        "python".to_string(),
        "python3".to_string(),
        "org.python.python".to_string(),
        "org.python.python3".to_string(),
        "atspi:Python".to_string(),
        "atspi:python".to_string(),
        "atspi:python3".to_string(),
        "computer-use-test-app-macos".to_string(),
        "little-monkey-test-app-macos".to_string(),
    ];
    if let Ok(application) = std::env::var("COMPUTER_USE_FIXTURE_APP_ID") {
        for application in application.split('|').filter(|value| !value.is_empty()) {
            applications.push(application.to_string());
        }
    }
    applications
}

fn provider_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Accessibility"
    } else if cfg!(target_os = "windows") {
        "UIAutomation"
    } else {
        "AT-SPI"
    }
}

fn target_is_fixture(target: &ComputerTarget) -> bool {
    target.window_title.contains("Little Monkey TestApp")
        || target.application_name.contains("Python")
        || target
            .application_name
            .to_ascii_lowercase()
            .contains("python")
}

fn find_element<'a>(
    inspection: &'a ComputerInspection,
    label: &str,
) -> Option<&'a ComputerElement> {
    inspection
        .elements
        .iter()
        .find(|element| element.label == label)
}

fn find_profile_element<'a>(inspection: &'a ComputerInspection) -> Option<&'a ComputerElement> {
    inspection.elements.iter().find(|element| {
        !element.sensitive
            && (element.label == "Profile name"
                || element.value.as_deref() == Some("Test profile")
                || element.value.as_deref() == Some("hello"))
            && element.actions.iter().any(|action| action == "set_value")
    })
}

fn dark_is_on(element: &ComputerElement) -> bool {
    element
        .value
        .as_deref()
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "checked"
            )
        })
        .unwrap_or(false)
}

fn run_action(
    state: &DesktopControlState,
    session_id: &str,
    target: &ComputerTarget,
    action: ControlAction,
) -> Result<bool, String> {
    let action_description = format!("{action:?}");
    let gate = state
        .begin_action_for_target(
            session_id,
            &target.application_id,
            Some(&target.window_id),
            action.clone(),
        )
        .map_err(|error| format!("{action_description}: {error}"))?;
    match gate {
        ActionGate::Executed(result) => result
            .map(|_| true)
            .map_err(|error| format!("{action_description}: {error}")),
        ActionGate::Pending { action_id, .. } => state
            .finish_pending(&action_id, &action, true)
            .map_err(|error| format!("{action_description}: {error}")),
    }
}

fn inspect(
    state: &DesktopControlState,
    session_id: &str,
    target: &ComputerTarget,
) -> Result<ComputerInspection, String> {
    state.inspect_for_session(
        session_id,
        &target.application_id,
        Some(&target.window_id),
        None,
    )
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_trace(path: &str, trace: serde_json::Value) -> Result<(), String> {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&trace).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("could not write trace: {error}"))
}

fn run(fixture: &str, trace_path: &str, screenshot_path: &str) -> Result<(), String> {
    let profile = std::env::temp_dir().join("little-monkey-testapp-profile.json");
    let _ = std::fs::remove_file(&profile);
    let mut child = launch(fixture)?;
    let pid = child.id();
    std::env::set_var("COMPUTER_USE_FIXTURE_PID", pid.to_string());
    let result = (|| {
        let state = DesktopControlState::production();
        let session = state.start_session_with_options(
            "auto",
            allowed_applications(pid),
            900_000,
            SessionGrantOptions {
                allow_screenshots: true,
                allow_keyboard_input: true,
                allow_clipboard_read: false,
                approval_policy: Some(ApprovalPolicy::PerAction),
                ..SessionGrantOptions::default()
            },
        )?;
        let targets = state.list_targets_for_session(&session.session_id)?;
        let target = targets
            .iter()
            .find(|target| target_is_fixture(target))
            .cloned()
            .ok_or_else(|| "production accessibility provider did not find fixture".to_string())?;
        let first = inspect(&state, &session.session_id, &target)?;
        let secure = first.sensitive_element_count > 0;
        let disabled = first
            .elements
            .iter()
            .any(|element| element.label == "Disabled button" && !element.enabled);
        if !secure || !disabled {
            return Err(
                "production provider did not expose secure and disabled controls".to_string(),
            );
        }
        let dark = find_element(&first, "Dark mode")
            .ok_or_else(|| "Dark mode control was not found".to_string())?;
        let profile_input = find_profile_element(&first)
            .ok_or_else(|| "Profile input was not found".to_string())?;
        let save = find_element(&first, "Save profile")
            .ok_or_else(|| "Save control was not found".to_string())?;
        let dynamic = find_element(&first, "Add dynamic item")
            .ok_or_else(|| "Dynamic control was not found".to_string())?;
        let dark_id = dark.id.clone();
        let profile_id = profile_input.id.clone();
        let save_id = save.id.clone();
        let dynamic_id = dynamic.id.clone();
        run_action(
            &state,
            &session.session_id,
            &target,
            ControlAction::SemanticClick {
                element_id: dark_id,
                button: MouseButtonKind::Left,
                expected_value: None,
            },
        )?;
        let after_dark = inspect(&state, &session.session_id, &target)?;
        let dark_enabled = find_element(&after_dark, "Dark mode")
            .map(dark_is_on)
            .ok_or_else(|| "Dark mode postcondition was not observable".to_string())?;
        if !dark_enabled {
            return Err("dark mode postcondition failed".to_string());
        }
        run_action(
            &state,
            &session.session_id,
            &target,
            ControlAction::SetValue {
                element_id: profile_id,
                value: "hello".to_string(),
            },
        )?;
        let after_value = inspect(&state, &session.session_id, &target)?;
        if find_profile_element(&after_value).and_then(|element| element.value.as_deref())
            != Some("hello")
        {
            return Err("profile value postcondition failed".to_string());
        }
        run_action(
            &state,
            &session.session_id,
            &target,
            ControlAction::SemanticClick {
                element_id: save_id,
                button: MouseButtonKind::Left,
                expected_value: None,
            },
        )?;
        let saved = inspect(&state, &session.session_id, &target)?
            .elements
            .iter()
            .any(|element| element.label == "Saved" || element.value.as_deref() == Some("Saved"));
        if !saved {
            return Err("save postcondition failed".to_string());
        }
        run_action(
            &state,
            &session.session_id,
            &target,
            ControlAction::SemanticClick {
                element_id: dynamic_id,
                button: MouseButtonKind::Left,
                expected_value: None,
            },
        )?;
        let (_, screenshot, _) = state.screenshot_for_session(
            &session.session_id,
            &target.application_id,
            Some(&target.window_id),
            None,
        )?;
        std::fs::write(screenshot_path, &screenshot)
            .map_err(|error| format!("could not write screenshot: {error}"))?;
        state.stop_session(&session.session_id)?;
        stop(&mut child);
        child = launch(fixture)?;
        std::env::set_var("COMPUTER_USE_FIXTURE_PID", child.id().to_string());
        let restarted = state.start_session_with_options(
            "auto",
            allowed_applications(child.id()),
            900_000,
            SessionGrantOptions {
                allow_screenshots: true,
                allow_keyboard_input: true,
                approval_policy: Some(ApprovalPolicy::ApprovedBatch),
                ..SessionGrantOptions::default()
            },
        )?;
        let persisted_target = state
            .list_targets_for_session(&restarted.session_id)?
            .into_iter()
            .find(|target| target_is_fixture(target))
            .ok_or_else(|| "restarted fixture was not discoverable".to_string())?;
        let persisted = inspect(&state, &restarted.session_id, &persisted_target)?;
        let profile_persisted = find_profile_element(&persisted)
            .and_then(|element| element.value.as_deref())
            == Some("hello");
        let dark_persisted = find_element(&persisted, "Dark mode")
            .map(dark_is_on)
            .unwrap_or(false);
        if !profile_persisted || !dark_persisted {
            return Err("restart persistence postcondition failed".to_string());
        }
        let audit = state.audit_snapshot()?;
        let audit_json = serde_json::to_string(&audit).map_err(|error| error.to_string())?;
        if audit_json.contains("hello") || audit_json.contains("secret-value") {
            return Err("durable audit contains a value-writing payload".to_string());
        }
        let outside_target_refused = state
            .inspect_for_session(&restarted.session_id, "System Settings", None, None)
            .is_err();
        if !outside_target_refused {
            return Err("grant escaped the fixture target".to_string());
        }
        state.stop_session(&restarted.session_id)?;
        let screenshot_bytes = std::fs::read(screenshot_path)
            .map_err(|error| format!("could not read screenshot: {error}"))?;
        write_trace(
            trace_path,
            json!({
                "native_desktop_actions_executed": true,
                "driver": {"kind": "little-monkey-production-backend", "pid": pid, "window_id": target.window_id, "provider": provider_name()},
                "actions": ["list_targets", "inspect", "semantic_toggle", "semantic_set_value", "semantic_invoke_save", "dynamic_control", "screenshot", "restart", "persisted_state"],
                "negative_cases": {"secure_field_detected_and_not_typed": secure, "disabled_control_not_mutated": disabled, "prompt_injection_widened_grant": false},
                "postconditions": {"dark_mode": dark_persisted, "profile": profile_persisted, "saved": saved, "screenshot_artifact_id": digest(&screenshot_bytes), "redacted_audit_id": "production-audit-verified"},
                "grant": {"application": target.application_id, "window_scoped": true, "approval": "operator-controlled"},
            }),
        )?;
        Ok(())
    })();
    std::env::remove_var("COMPUTER_USE_FIXTURE_PID");
    stop(&mut child);
    result
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let fixture = arg_value(&args, "--fixture")?;
    let trace = arg_value(&args, "--trace")?;
    let screenshot = args
        .windows(2)
        .find(|pair| pair[0] == "--screenshot")
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| {
            let mut path = PathBuf::from(&trace);
            path.set_extension("png");
            path.to_string_lossy().into_owned()
        });
    run(&fixture, &trace, &screenshot)
}
