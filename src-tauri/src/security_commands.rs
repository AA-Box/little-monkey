//! Thin desktop bridge for the Tauri-free Security Doctor engine.

use std::path::PathBuf;

use crate::browser_worker::BrowserCommandState;
use crate::m4_commands::M4CommandState;
use crate::m7_companion::{CaptureKind, M7CompanionState};
use crate::native_skill_commands::NativeSkillsCommandState;
use crate::native_skills::{ExternalSignedSkill, SkillSource};
use crate::security_doctor::{
    run_security_audit, BrowserGrantSnapshot, CompanionGrantSnapshot, NativeSkillSnapshot,
    SecurityAuditReport, SecurityAuditRequest, SecurityRuntimeSnapshot, VoicePrivacySnapshot,
};
use crate::AppState;

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
    .map_err(|error| format!("Security Doctor worker failed: {error}"))?
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
