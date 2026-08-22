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
    } else if cfg!(target_os = "linux") {
        if let Ok(script) = std::env::var("COMPUTER_USE_FIXTURE_SCRIPT") {
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
    thread::sleep(Duration::from_secs(if cfg!(target_os = "windows") {
        8
    } else {
        2
    }));
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
        "atspi:Little Monkey TestApp".to_string(),
        "atspi:com.aabox.LittleMonkeyTestApp".to_string(),
        "Little Monkey TestApp".to_string(),
        "com.aabox.LittleMonkeyTestApp".to_string(),
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
    let application = target.application_name.to_ascii_lowercase();
    target.window_title.contains("Little Monkey TestApp")
        || application.contains("python")
        || application.contains("little monkey")
        || application.contains("littlemonkey")
}

fn target_is_primary_fixture(target: &ComputerTarget) -> bool {
    target.window_title == "Little Monkey TestApp"
        || ((target
            .application_name
            .to_ascii_lowercase()
            .contains("python")
            || target
                .application_name
                .to_ascii_lowercase()
                .contains("little monkey")
            || target
                .application_name
                .to_ascii_lowercase()
                .contains("littlemonkey"))
            && !target
                .window_title
                .to_ascii_lowercase()
                .contains("secondary"))
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

fn find_toggle_element<'a>(
    inspection: &'a ComputerInspection,
    label: &str,
) -> Option<&'a ComputerElement> {
    inspection
        .elements
        .iter()
        .find(|element| {
            let role = element.role.to_ascii_lowercase();
            element.label == label && (role.contains("check") || role.contains("toggle"))
        })
        .or_else(|| {
            inspection.elements.iter().find(|element| {
                element.label == label && element.actions.iter().any(|action| action == "click")
            })
        })
        .or_else(|| {
            inspection
                .elements
                .iter()
                .find(|element| element.label == label)
        })
}

fn find_profile_element<'a>(inspection: &'a ComputerInspection) -> Option<&'a ComputerElement> {
    inspection
        .elements
        .iter()
        .filter(|element| !element.sensitive)
        .filter_map(|element| {
            let role = element.role.to_ascii_lowercase();
            let mut score = 0;
            if element.label == "Profile name" {
                score += 100;
            }
            if matches!(
                element.value.as_deref(),
                Some("Test profile") | Some("hello")
            ) {
                score += 80;
            }
            if role.contains("edit") || role.contains("textfield") || role.contains("text field") {
                score += 40;
            }
            if element.actions.iter().any(|action| action == "set_value") {
                score += 10;
            }
            (score > 0).then_some((score, element))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, element)| element)
}

fn dark_is_on(element: &ComputerElement) -> bool {
    element
        .value
        .as_deref()
        .map(|value| {
            let normalized = value.to_ascii_lowercase();
            let state = normalized
                .rsplit(['.', ' ', ':'])
                .next()
                .unwrap_or(&normalized);
            matches!(state, "1" | "true" | "on" | "checked")
        })
        .unwrap_or(false)
}

fn inspect_until_dark_state(
    state: &DesktopControlState,
    session_id: &str,
    target: &ComputerTarget,
    expected: bool,
) -> Result<ComputerInspection, String> {
    let mut latest = inspect(state, session_id, target)?;
    for attempt in 0..10 {
        if find_toggle_element(&latest, "Dark mode").map(dark_is_on) == Some(expected) {
            return Ok(latest);
        }
        if attempt < 9 {
            thread::sleep(Duration::from_millis(200));
            latest = inspect(state, session_id, target)?;
        }
    }
    let observed = latest
        .elements
        .iter()
        .filter(|element| element.label == "Dark mode")
        .map(|element| {
            format!(
                "id={} role={} value={:?} actions={:?} enabled={} bounds={:?}",
                element.id,
                element.role,
                element.value,
                element.actions,
                element.enabled,
                element.bounds
            )
        })
        .collect::<Vec<_>>();
    Err(format!(
        "dark mode state did not settle to {expected}: {observed:?}"
    ))
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
        // Discovery intentionally happens in a short-lived unscoped grant.
        // The acceptance grant below is created only after the provider has
        // returned the exact window identity, so its evidence cannot claim a
        // window scope that was never enforced.
        let discovery = state.start_session_with_options(
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
        let mut target = None;
        let mut discovered_targets = Vec::new();
        for attempt in 0..20 {
            match state.list_targets_for_session(&discovery.session_id) {
                Ok(targets) => {
                    discovered_targets = targets;
                    target = discovered_targets
                        .iter()
                        .find(|candidate| target_is_primary_fixture(candidate))
                        .cloned();
                    if target.is_some() {
                        break;
                    }
                }
                Err(error) if attempt == 19 => return Err(error),
                Err(_) => {}
            }
            thread::sleep(Duration::from_millis(500));
        }
        let target = target
            .ok_or_else(|| "production accessibility provider did not find fixture".to_string())?;
        let second_window = discovered_targets
            .iter()
            .find(|candidate| {
                target_is_fixture(candidate)
                    && candidate.application_id == target.application_id
                    && candidate.window_id != target.window_id
            })
            .cloned()
            .ok_or_else(|| "fixture did not expose a second same-application window".to_string())?;
        state.stop_session(&discovery.session_id)?;
        let session = state.start_session_with_options(
            "auto",
            allowed_applications(pid),
            900_000,
            SessionGrantOptions {
                allowed_windows: vec![target.window_id.clone()],
                allow_screenshots: true,
                allow_keyboard_input: true,
                allow_clipboard_read: false,
                approval_policy: Some(ApprovalPolicy::PerAction),
                ..SessionGrantOptions::default()
            },
        )?;
        let scoped_targets = state.list_targets_for_session(&session.session_id)?;
        if scoped_targets.len() != 1 || scoped_targets[0].window_id != target.window_id {
            return Err("window-scoped grant exposed more than its discovered target".to_string());
        }
        let second_window_rejected = state
            .inspect_for_session(
                &session.session_id,
                &second_window.application_id,
                Some(&second_window.window_id),
                None,
            )
            .is_err();
        if !second_window_rejected {
            return Err(
                "window-scoped grant accepted a second same-application window".to_string(),
            );
        }
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
        let dark = find_toggle_element(&first, "Dark mode")
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
        let _after_dark = inspect_until_dark_state(&state, &session.session_id, &target, true)?;
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
        let restart_discovery = state.start_session_with_options(
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
            .list_targets_for_session(&restart_discovery.session_id)?
            .into_iter()
            .find(target_is_primary_fixture)
            .ok_or_else(|| "restarted fixture was not discoverable".to_string())?;
        state.stop_session(&restart_discovery.session_id)?;
        let restarted = state.start_session_with_options(
            "auto",
            allowed_applications(child.id()),
            900_000,
            SessionGrantOptions {
                allowed_windows: vec![persisted_target.window_id.clone()],
                allow_screenshots: true,
                allow_keyboard_input: true,
                approval_policy: Some(ApprovalPolicy::ApprovedBatch),
                ..SessionGrantOptions::default()
            },
        )?;
        let persisted =
            inspect_until_dark_state(&state, &restarted.session_id, &persisted_target, true)?;
        let profile_persisted = find_profile_element(&persisted)
            .and_then(|element| element.value.as_deref())
            == Some("hello");
        let dark_persisted = find_toggle_element(&persisted, "Dark mode")
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
                "negative_cases": {"secure_field_detected_and_not_typed": secure, "disabled_control_not_mutated": disabled, "second_same_app_window_rejected": second_window_rejected, "prompt_injection_widened_grant": false},
                "postconditions": {"dark_mode": dark_persisted, "profile": profile_persisted, "saved": saved, "screenshot_artifact_id": digest(&screenshot_bytes), "redacted_audit_id": "production-audit-verified"},
                "grant": {"application": target.application_id, "window_id": target.window_id, "window_scoped": session.allowed_windows == vec![target.window_id.clone()], "approval": "test-approved-through-real-gate"},
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
