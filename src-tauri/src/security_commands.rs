//! Thin desktop bridge for the Tauri-free Security Doctor engine.

use std::path::PathBuf;

use crate::browser_worker::BrowserCommandState;
use crate::m4_commands::M4CommandState;
use crate::m7_companion::{CaptureKind, M7CompanionState};
use crate::native_skill_commands::NativeSkillsCommandState;
use crate::native_skills::{ExternalSignedSkill, SkillSource};
use crate::security_doctor::{
    append_findings, run_security_audit, BrowserGrantSnapshot, CompanionGrantSnapshot,
    DaemonSecurityState, NativeSkillSnapshot, SecurityAuditReport, SecurityAuditRequest,
    SecurityRuntimeSnapshot, VoicePrivacySnapshot,
};
use crate::AppState;

/// The `DaemonSecurityState` shape this build knows how to read.
///
/// Checked rather than assumed: a desktop app talking to a newer bundled CLI
/// would otherwise deserialize the fields it recognizes and silently drop a
/// whole category of check, which is exactly the failure this whole path exists
/// to remove. A mismatch becomes one visible finding instead.
const DAEMON_STATE_SCHEMA_VERSION: u32 = 1;

#[tauri::command]
pub async fn security_audit(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    m4: tauri::State<'_, M4CommandState>,
    browser: tauri::State<'_, BrowserCommandState>,
    companion: tauri::State<'_, M7CompanionState>,
    app: tauri::State<'_, AppState>,
    deep: bool,
    fix: bool,
) -> Result<SecurityAuditReport, String> {
    if fix && window.label() != "main" {
        return Err(
            "Security Doctor safe fixes are only available from the main window".to_string(),
        );
    }
    let app_data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the app data directory".to_string())?;
    let workspace = primary_workspace(&app)?;
    let mut runtime = SecurityRuntimeSnapshot::default();

    match browser.security_grants() {
        Ok(grants) => {
            runtime.browser_observed = true;
            runtime.browser_grants = grants
                .into_iter()
                .map(|grant| BrowserGrantSnapshot {
                    session_id: grant.session_id,
                    run_id: grant.run_id,
                    allowed_origins: grant.allowed_origins,
                    allow_loopback: grant.allow_loopback,
                })
                .collect();
        }
        Err(error) => runtime.browser_error = Some(error),
    }
    match companion.security_grants() {
        Ok(grants) => {
            runtime.companion_observed = true;
            runtime.companion_grants = grants
                .into_iter()
                .map(|grant| CompanionGrantSnapshot {
                    grant_id: grant.grant_id,
                    kind: capture_kind_label(grant.kind).to_string(),
                    application_id: grant.application_id,
                    expires_at_ms: grant.expires_at_ms,
                    active: grant.active,
                })
                .collect();
        }
        Err(error) => runtime.companion_error = Some(error),
    }
    // Voice settings are this machine's own, not a device grant, so they reach
    // the audit through the companion state rather than the device store. A
    // read that fails leaves `None`, which the audit reads as "not observed"
    // rather than as "nothing is listening".
    if let Ok(voice) = companion.security_voice_privacy() {
        runtime.voice = Some(VoicePrivacySnapshot {
            wake_phrase_enabled: voice.wake_phrase_enabled,
            always_listening: voice.always_listening,
            local_only: voice.local_only,
        });
    }

    let package_skills = match m4.packages.active_skills() {
        Ok(skills) => skills
            .into_iter()
            .map(|skill| ExternalSignedSkill {
                package_id: skill.package_id,
                name: skill.name,
                description: skill.description,
                command: skill.command,
                version: skill.version.to_string(),
                instructions: skill.instructions,
                sha256: skill.content_sha256,
                permissions: skill
                    .permissions
                    .into_iter()
                    .map(|permission| permission.permission_id)
                    .collect(),
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            runtime.native_skills_error = Some(error.to_string());
            Vec::new()
        }
    };
    // Devices, messaging accounts, phone numbers and peers live in databases
    // the daemon owns, which this process cannot open. Before this the desktop
    // panel simply ran none of those checks and reported a clean page, which is
    // worse than reporting nothing: an operator cannot tell a check that passed
    // from one that never ran. Read over the same typed bridge every other
    // daemon-backed panel uses — a fixed argument list this file builds, never
    // anything the frontend supplies.
    let daemon_findings = match daemon_security_state().await {
        Ok(state) => state.apply(&mut runtime),
        Err(error) => vec![crate::security_doctor::SecurityFinding {
            id: "daemon.state_unavailable".to_string(),
            category: "storage".to_string(),
            title: "Part of this audit could not run".to_string(),
            detail: format!(
                "Paired devices, messaging accounts, phone numbers and peers could not be \
                 inspected, so nothing on this page reflects them: {error}"
            ),
            status: crate::security_doctor::FindingStatus::Warning,
            fixable: false,
            path: None,
            remediation: Some(
                "Start or repair the background service from Settings, then run the audit again."
                    .to_string(),
            ),
        }],
    };

    let native_manager = native.manager.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if runtime.native_skills_error.is_none() {
            match native_manager.discover(workspace.as_deref(), &package_skills) {
                Ok(skills) => {
                    runtime.native_skills = skills
                        .into_iter()
                        .map(|skill| NativeSkillSnapshot {
                            command: skill.command,
                            source: match skill.source {
                                SkillSource::Global { path } => format!("global:{path}"),
                                SkillSource::Workspace { path } => format!("workspace:{path}"),
                                SkillSource::SignedPackage { package_id } => {
                                    format!("package:{package_id}")
                                }
                            },
                            enabled: skill.enabled,
                            eligible: skill.eligibility.eligible,
                            missing_bins: skill.eligibility.missing_bins,
                            missing_env: skill.eligibility.missing_env,
                        })
                        .collect();
                }
                Err(error) => runtime.native_skills_error = Some(error.to_string()),
            }
        }
        run_security_audit(&SecurityAuditRequest {
            app_data_dir,
            workspace,
            deep,
            fix,
            runtime,
        })
    })
    .await
    .map_err(|error| format!("Security Doctor worker failed: {error}"))
    .and_then(|report| {
        let mut report = report?;
        // After the audit, so the summary the panel shows counts them.
        append_findings(&mut report, daemon_findings);
        Ok(report)
    })
}

/// Ask the bundled CLI for the half of the audit only it can see.
///
/// A fixed argument list built here. The frontend passes no arguments to this
/// path and never could — the whole point of the typed bridge is that React
/// asks for a *capability*, not for a command line.
async fn daemon_security_state() -> Result<DaemonSecurityState, String> {
    let output =
        crate::daemon_commands::command(vec!["security".into(), "daemon-state".into()]).await?;
    let state: DaemonSecurityState = serde_json::from_str(output.trim())
        .map_err(|error| format!("Could not read the background service's report: {error}"))?;
    // Newer than this build understands. Refused rather than partially read:
    // a category silently missing from a security page is the failure mode this
    // whole path was added to close.
    if state.schema_version > DAEMON_STATE_SCHEMA_VERSION {
        return Err(format!(
            "the background service reports a newer format (v{}) than this app understands (v{DAEMON_STATE_SCHEMA_VERSION})",
            state.schema_version
        ));
    }
    Ok(state)
}

fn primary_workspace(state: &AppState) -> Result<Option<PathBuf>, String> {
    let roots = state
        .workspace_roots
        .lock()
        .map_err(|_| "Workspace roots lock poisoned".to_string())?;
    let Some(primary) = roots.first() else {
        return Ok(None);
    };
    primary.path.canonicalize().map(Some).map_err(|error| {
        format!(
            "Primary workspace '{}' is no longer valid: {error}",
            primary.path.display()
        )
    })
}

fn capture_kind_label(kind: CaptureKind) -> &'static str {
    match kind {
        CaptureKind::Text => "text",
        CaptureKind::File => "file",
        CaptureKind::Window => "window",
        CaptureKind::Screen => "screen",
        CaptureKind::Microphone => "microphone",
        CaptureKind::Meeting => "meeting",
    }
}
