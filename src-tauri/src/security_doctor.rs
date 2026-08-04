//! Tauri-free security posture audit shared by the desktop Security Doctor
//! and `monkey security audit`.
//!
//! The audit is deliberately conservative. A normal run is read-only. The
//! optional fix pass can only tighten Unix permissions on known app-owned
//! paths or disable an enabled, clearly unsafe app-owned MCP/remote-listener
//! configuration. It never deletes files, follows symlinks, rewrites a
//! workspace, rotates credentials, or enables a capability.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;
#[cfg(unix)]
use walkdir::WalkDir;

use crate::sandbox::SandboxEnforcement;

pub const SECURITY_AUDIT_SCHEMA_VERSION: u32 = 1;
const MAX_DEEP_PATHS: usize = 8_192;
const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_CAPTURE_GRANT_MS: u64 = 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Pass,
    Info,
    Warning,
    Critical,
    Fixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecurityFinding {
    pub id: String,
    pub category: String,
    pub title: String,
    pub detail: String,
    pub status: FindingStatus,
    pub fixable: bool,
    pub path: Option<String>,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecuritySummary {
    pub passed: usize,
    pub informational: usize,
    pub warnings: usize,
    pub critical: usize,
    pub fixed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecurityAuditReport {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub deep: bool,
    pub fix_requested: bool,
    pub summary: SecuritySummary,
    pub findings: Vec<SecurityFinding>,
}

#[derive(Debug, Clone, Default)]
pub struct SecurityRuntimeSnapshot {
    pub browser_grants: Vec<BrowserGrantSnapshot>,
    pub browser_observed: bool,
    pub browser_error: Option<String>,
    pub companion_grants: Vec<CompanionGrantSnapshot>,
    pub companion_observed: bool,
    pub companion_error: Option<String>,
    pub native_skills: Vec<NativeSkillSnapshot>,
    pub native_skills_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserGrantSnapshot {
    pub session_id: String,
    pub run_id: String,
    pub allowed_origins: Vec<String>,
    pub allow_loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionGrantSnapshot {
    pub grant_id: String,
    pub kind: String,
    pub application_id: Option<String>,
    pub expires_at_ms: u64,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSkillSnapshot {
    pub command: String,
    pub source: String,
    pub enabled: bool,
    pub eligible: bool,
    pub missing_bins: Vec<String>,
    pub missing_env: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SecurityAuditRequest {
    pub app_data_dir: PathBuf,
    pub workspace: Option<PathBuf>,
    pub deep: bool,
    pub fix: bool,
    pub runtime: SecurityRuntimeSnapshot,
}

pub fn run_security_audit(request: &SecurityAuditRequest) -> Result<SecurityAuditReport, String> {
    let app_data = &request.app_data_dir;
    match fs::symlink_metadata(app_data) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(format!(
                "Little Monkey app data '{}' must be a real directory",
                app_data.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect Little Monkey app data '{}': {error}",
                app_data.display()
            ));
        }
    }

    let mut findings = Vec::new();
    audit_owned_permissions(app_data, request.deep, request.fix, &mut findings);
    audit_loopback_services(app_data, &mut findings);
    audit_remote_host(app_data, request.deep, request.fix, &mut findings);
    audit_mcp_origins(app_data, request.fix, &mut findings);
    audit_native_skills(&request.runtime, &mut findings);
    audit_runtime_grants(&request.runtime, &mut findings);
    audit_workspace_skill_root(request.workspace.as_deref(), &mut findings);
    audit_sandbox_enforcement(&mut findings);

    let summary = summarize(&findings);
    Ok(SecurityAuditReport {
        schema_version: SECURITY_AUDIT_SCHEMA_VERSION,
        generated_at_ms: now_ms(),
        deep: request.deep,
        fix_requested: request.fix,
        summary,
        findings,
    })
}

fn audit_owned_permissions(
    app_data: &Path,
    deep: bool,
    fix: bool,
    findings: &mut Vec<SecurityFinding>,
) {
    #[cfg(not(unix))]
    {
        let _ = (app_data, deep, fix);
        findings.push(finding(
            "storage.platform_acl",
            "storage",
            "Platform-managed access controls",
            "Unix mode-bit checks are not applicable on this platform. Little Monkey continues to rely on the operating system's per-user application-data ACLs.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
        return;
    }

    #[cfg(unix)]
    {
        let mut paths = BTreeSet::<PathBuf>::new();
        paths.insert(app_data.to_path_buf());
        for name in [
            "api_server.json",
            "mcp_servers.json",
            "providers.json",
            "web_settings.json",
            "memories.json",
            "sessions.json",
            "prompts.json",
            "automations.json",
            "profile-v1.sqlite3",
            "profile-v1.sqlite3-wal",
            "profile-v1.sqlite3-shm",
        ] {
            paths.insert(app_data.join(name));
        }
        for relative in [
            "daemon/config.json",
            "daemon/daemon-v1.sqlite3",
            "daemon/daemon.lock",
            "daemon/remote-host.json",
            "daemon/remote-server-cert.pem",
            "daemon/remote-server-key.pem",
            "native-skills-v1/global/.littlemonkey-skills-state-v1.json",
            "m7-companion-v1/companion-config-v1.json",
            "m7-companion-v1/image-gallery-v1.json",
        ] {
            paths.insert(app_data.join(relative));
        }
        let sensitive_roots = [
            app_data.join("daemon"),
            app_data.join("native-skills-v1"),
            app_data.join("browser-v1"),
            app_data.join("m7-companion-v1"),
            app_data.join("content-v1"),
        ];
        for root in &sensitive_roots {
            paths.insert(root.clone());
            if deep && root.exists() {
                for entry in WalkDir::new(root)
                    .follow_links(false)
                    .max_depth(16)
                    .into_iter()
                    .take(MAX_DEEP_PATHS)
                {
                    match entry {
                        Ok(entry) => {
                            paths.insert(entry.path().to_path_buf());
                        }
                        Err(error) => findings.push(finding(
                            &format!("storage.walk.{}", short_hash(error.to_string().as_bytes())),
                            "storage",
                            "Could not inspect a protected directory",
                            &error.to_string(),
                            FindingStatus::Warning,
                            false,
                            None,
                            Some("Check ownership and access permissions for the reported application-data path."),
                        )),
                    }
                }
            }
        }

        let mut checked = 0usize;
        let mut issues = 0usize;
        for path in paths {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    issues += 1;
                    findings.push(path_finding(
                        "storage.inspect",
                        "storage",
                        "Could not inspect an app-owned path",
                        &error.to_string(),
                        FindingStatus::Warning,
                        false,
                        &path,
                        Some("Restore ownership of this path to the current user."),
                    ));
                    continue;
                }
            };
            checked += 1;
            if metadata.file_type().is_symlink() {
                issues += 1;
                findings.push(path_finding(
                    "storage.symlink",
                    "storage",
                    "Symlink found in a protected app-data location",
                    "Security Doctor will not follow or modify this symlink.",
                    FindingStatus::Critical,
                    false,
                    &path,
                    Some("Inspect the link target manually, stop Little Monkey, and replace it with an owned regular file or directory if it is unexpected."),
                ));
                continue;
            }
            if !metadata.is_dir() && !metadata.is_file() {
                issues += 1;
                findings.push(path_finding(
                    "storage.special",
                    "storage",
                    "Special file found in protected app data",
                    "Only regular files and directories are expected in this location.",
                    FindingStatus::Critical,
                    false,
                    &path,
                    Some(
                        "Inspect this path manually. Security Doctor never removes special files.",
                    ),
                ));
                continue;
            }
            use std::os::unix::fs::PermissionsExt;
            let current = metadata.permissions().mode() & 0o777;
            let expected = if metadata.is_dir() { 0o700 } else { 0o600 };
            if current & 0o077 == 0 {
                continue;
            }
            issues += 1;
            let detail = format!(
                "Mode {:03o} allows group or other users to access this app-owned {}.",
                current,
                if metadata.is_dir() {
                    "directory"
                } else {
                    "file"
                }
            );
            if fix && path.starts_with(app_data) {
                match fs::set_permissions(&path, fs::Permissions::from_mode(expected)) {
                    Ok(()) => findings.push(path_finding(
                        "storage.mode",
                        "storage",
                        "Restricted an app-owned path",
                        &format!("{detail} Security Doctor changed it to {expected:03o}."),
                        FindingStatus::Fixed,
                        true,
                        &path,
                        None,
                    )),
                    Err(error) => findings.push(path_finding(
                        "storage.mode",
                        "storage",
                        "App-owned path permissions are too broad",
                        &format!("{detail} The safe fix failed: {error}"),
                        FindingStatus::Critical,
                        true,
                        &path,
                        Some("Restrict this path to the current user (0700 for directories or 0600 for files)."),
                    )),
                }
            } else {
                findings.push(path_finding(
                    "storage.mode",
                    "storage",
                    "App-owned path permissions are too broad",
                    &detail,
                    FindingStatus::Warning,
                    true,
                    &path,
                    Some("Run Security Doctor with safe fixes to restrict this path to the current user."),
                ));
            }
        }
        if issues == 0 {
            findings.push(finding(
                "storage.private",
                "storage",
                "App-owned sensitive paths are private",
                &format!(
                    "Checked {checked} existing path(s){}; none grant group or other-user access.",
                    if deep { " recursively" } else { "" }
                ),
                FindingStatus::Pass,
                false,
                None,
                None,
            ));
        }
    }
}

fn audit_loopback_services(app_data: &Path, findings: &mut Vec<SecurityFinding>) {
    findings.push(finding(
        "network.local_api_loopback",
        "network",
        "Local API is loopback-only",
        "The desktop and headless local API listener binds to 127.0.0.1; the saved configuration has no public bind-address option.",
        FindingStatus::Pass,
        false,
        None,
        None,
    ));
    match crate::server::load_config_impl(&app_data.join("api_server.json")) {
        Ok(config) if config.autostart && !config.require_token => findings.push(finding(
            "network.local_api_auth",
            "network",
            "Autostarted local API does not require a token",
            "The listener remains loopback-only, but any process running as this user can call it while it is active.",
            FindingStatus::Warning,
            false,
            Some(app_data.join("api_server.json").to_string_lossy().as_ref()),
            Some("Enable token authentication in Settings > API server."),
        )),
        Ok(config) if config.require_token => findings.push(finding(
            "network.local_api_auth",
            "network",
            "Local API authentication is enabled",
            "Saved tokens are stored as digests and requests require a bearer token.",
            FindingStatus::Pass,
            false,
            None,
            None,
        )),
        Ok(_) => findings.push(finding(
            "network.local_api_auth",
            "network",
            "Local API token authentication is optional",
            "The API is loopback-only and is not configured to autostart. Enable token authentication before leaving it running for other local applications.",
            FindingStatus::Info,
            false,
            None,
            None,
        )),
        Err(error) => findings.push(path_finding(
            "network.local_api_config",
            "network",
            "Local API configuration is invalid",
            &error,
            FindingStatus::Critical,
            false,
            &app_data.join("api_server.json"),
            Some("Repair the configuration in Settings before starting the API server."),
        )),
    }

    let daemon_config = app_data.join("daemon").join("config.json");
    match read_bounded_json(&daemon_config) {
        Ok(Some(value)) => {
            let webhook = value.get("webhook_port").and_then(Value::as_u64);
            if let Some(port) = webhook {
                findings.push(finding(
                    "network.webhook_loopback",
                    "network",
                    "Signed webhook listener is loopback-only",
                    &format!("The configured webhook listener on port {port} binds only to 127.0.0.1 and verifies each delivery signature."),
                    FindingStatus::Pass,
                    false,
                    None,
                    None,
                ));
            } else {
                findings.push(finding(
                    "network.webhook_disabled",
                    "network",
                    "Webhook listener is disabled",
                    "No daemon webhook port is configured.",
                    FindingStatus::Info,
                    false,
                    None,
                    None,
                ));
            }
        }
        Ok(None) => findings.push(finding(
            "network.webhook_not_installed",
            "network",
            "Background webhook listener is not installed",
            "No daemon configuration exists.",
            FindingStatus::Info,
            false,
            None,
            None,
        )),
        Err(error) => findings.push(path_finding(
            "network.webhook_config",
            "network",
            "Daemon configuration could not be audited",
            &error,
            FindingStatus::Warning,
            false,
            &daemon_config,
            Some("Repair or reinstall the background-agent service."),
        )),
    }
}

fn audit_remote_host(app_data: &Path, deep: bool, fix: bool, findings: &mut Vec<SecurityFinding>) {
    let config_path = app_data.join("daemon").join("remote-host.json");
    let mut value = match read_bounded_json(&config_path) {
        Ok(Some(value)) => value,
        Ok(None) => {
            findings.push(finding(
                "remote.not_configured",
                "remote",
                "Remote host is not configured",
                "No user-owned remote runner listener is enabled.",
                FindingStatus::Info,
                false,
                None,
                None,
            ));
            return;
        }
        Err(error) => {
            findings.push(path_finding(
                "remote.config_invalid",
                "remote",
                "Remote host configuration is unreadable",
                &error,
                FindingStatus::Critical,
                false,
                &config_path,
                Some("Disable the remote host from Settings or repair this app-owned JSON file."),
            ));
            return;
        }
    };
    let enabled = match value.get("enabled").and_then(Value::as_bool) {
        Some(enabled) => enabled,
        None => {
            findings.push(path_finding(
                "remote.enabled_invalid",
                "remote",
                "Remote host enabled state is invalid",
                "The configuration does not contain a Boolean enabled field.",
                FindingStatus::Critical,
                false,
                &config_path,
                Some("Reconfigure the remote host from Settings."),
            ));
            return;
        }
    };
    if !enabled {
        findings.push(finding(
            "remote.disabled",
            "remote",
            "Remote host is disabled",
            "The persisted listener configuration is inert until explicitly enabled again.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }

    let daemon_root = app_data.join("daemon");
    let mut unsafe_reasons = Vec::<String>::new();
    let advertise = value
        .get("advertise_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match Url::parse(advertise) {
        Ok(url)
            if url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && matches!(url.path(), "" | "/")
                && url.query().is_none()
                && url.fragment().is_none() => {}
        _ => unsafe_reasons
            .push("the advertised runner URL is not a credential-free HTTPS origin".to_string()),
    }
    let listen = value
        .get("listen")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if listen.parse::<std::net::SocketAddr>().is_err() {
        unsafe_reasons.push("the listener address is invalid".to_string());
    }

    let certificate_path = value
        .get("certificate_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let private_key_path = value
        .get("private_key_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    for (label, path) in [
        ("TLS certificate", certificate_path.as_deref()),
        ("TLS private key", private_key_path.as_deref()),
    ] {
        let Some(path) = path else {
            unsafe_reasons.push(format!("the {label} path is missing"));
            continue;
        };
        match owned_regular_file(path, &daemon_root) {
            Ok(()) => {}
            Err(error) => unsafe_reasons.push(format!("{label}: {error}")),
        }
    }
    #[cfg(unix)]
    if let Some(path) = private_key_path.as_deref() {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::symlink_metadata(path) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                unsafe_reasons.push(format!(
                    "the TLS private key mode {mode:03o} permits group or other-user access"
                ));
            }
        }
    }
    let configured_pin = value
        .get("certificate_sha256")
        .and_then(Value::as_str)
        .filter(|pin| pin.len() == 64 && pin.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if configured_pin.is_none() {
        unsafe_reasons.push("the TLS certificate fingerprint is missing or invalid".to_string());
    }

    if deep && unsafe_reasons.is_empty() {
        if let (Some(certificate_path), Some(expected)) =
            (certificate_path.as_deref(), configured_pin)
        {
            match fs::read(certificate_path)
                .map_err(|error| error.to_string())
                .and_then(|pem| certificate_fingerprint(&pem))
            {
                Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
                Ok(actual) => unsafe_reasons.push(format!(
                    "the TLS certificate pin changed (expected {expected}, found {actual})"
                )),
                Err(error) => unsafe_reasons.push(format!(
                    "the TLS certificate could not be fingerprinted: {error}"
                )),
            }
        }
    }

    if unsafe_reasons.is_empty() {
        findings.push(finding(
            "remote.tls",
            "remote",
            "Remote host uses an owned TLS identity",
            &format!(
                "The enabled listener at {listen} advertises {advertise}; its certificate and private key are regular files inside Little Monkey's daemon directory{}.",
                if deep { " and the certificate pin matches" } else { "" }
            ),
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }

    let detail = unsafe_reasons.join("; ");
    if fix {
        if let Some(object) = value.as_object_mut() {
            object.insert("enabled".to_string(), Value::Bool(false));
        }
        match atomic_write_private_json(&config_path, &value, app_data) {
            Ok(()) => findings.push(path_finding(
                "remote.disabled_unsafe",
                "remote",
                "Disabled an unsafe remote host",
                &format!("The listener was disabled because {detail}. No certificate, key, or pairing data was deleted."),
                FindingStatus::Fixed,
                true,
                &config_path,
                None,
            )),
            Err(error) => findings.push(path_finding(
                "remote.unsafe",
                "remote",
                "Enabled remote host is unsafe",
                &format!("{detail}. Security Doctor could not disable it: {error}"),
                FindingStatus::Critical,
                true,
                &config_path,
                Some("Disable and reconfigure the remote host before exposing it to a network."),
            )),
        }
    } else {
        findings.push(path_finding(
            "remote.unsafe",
            "remote",
            "Enabled remote host is unsafe",
            &detail,
            FindingStatus::Critical,
            true,
            &config_path,
            Some("Run the safe fix to disable this listener without deleting its configuration, then reconfigure TLS."),
        ));
    }
}

fn audit_mcp_origins(app_data: &Path, fix: bool, findings: &mut Vec<SecurityFinding>) {
    let path = app_data.join("mcp_servers.json");
    let mut config = match crate::mcp::load_config_impl(&path) {
        Ok(config) => config,
        Err(error) => {
            findings.push(path_finding(
                "mcp.config_invalid",
                "mcp",
                "MCP server configuration is invalid",
                &error,
                FindingStatus::Critical,
                false,
                &path,
                Some("Repair the MCP configuration in Settings."),
            ));
            return;
        }
    };
    let mut unsafe_enabled = BTreeMap::<String, String>::new();
    let mut safe_http = 0usize;
    for server in &config.servers {
        let crate::mcp::McpTransport::Http { url } = &server.transport else {
            continue;
        };
        let reason = insecure_mcp_reason(url);
        match (server.enabled, reason) {
            (true, Some(reason)) => {
                unsafe_enabled.insert(server.id.clone(), reason);
            }
            (false, Some(reason)) => findings.push(finding(
                &format!("mcp.insecure_disabled.{}", short_hash(server.id.as_bytes())),
                "mcp",
                "Disabled MCP server has an insecure origin",
                &format!(
                    "MCP server '{}' is currently inert: {reason}.",
                    server.label
                ),
                FindingStatus::Info,
                false,
                Some(path.to_string_lossy().as_ref()),
                Some("Use HTTPS or a loopback HTTP endpoint before enabling this server."),
            )),
            (_, None) => safe_http += 1,
        }
    }

    if unsafe_enabled.is_empty() {
        findings.push(finding(
            "mcp.origins",
            "mcp",
            "Enabled MCP HTTP origins are protected",
            &format!(
                "Checked {} enabled or configured HTTP transport(s); enabled endpoints use HTTPS or loopback HTTP.",
                safe_http
            ),
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }

    if fix {
        for server in &mut config.servers {
            if unsafe_enabled.contains_key(&server.id) {
                server.enabled = false;
            }
        }
        match serde_json::to_value(&config)
            .map_err(|error| error.to_string())
            .and_then(|value| atomic_write_private_json(&path, &value, app_data))
        {
            Ok(()) => {
                for (id, reason) in unsafe_enabled {
                    findings.push(finding(
                        &format!("mcp.disabled_unsafe.{}", short_hash(id.as_bytes())),
                        "mcp",
                        "Disabled an insecure MCP server",
                        &format!("Server '{id}' was disabled because {reason}. Its configuration and keychain credential were preserved."),
                        FindingStatus::Fixed,
                        true,
                        Some(path.to_string_lossy().as_ref()),
                        None,
                    ));
                }
            }
            Err(error) => findings.push(path_finding(
                "mcp.fix_failed",
                "mcp",
                "Unsafe MCP servers could not be disabled",
                &error,
                FindingStatus::Critical,
                true,
                &path,
                Some("Disable these servers in Settings before using MCP tools."),
            )),
        }
    } else {
        for (id, reason) in unsafe_enabled {
            findings.push(finding(
                &format!("mcp.insecure.{}", short_hash(id.as_bytes())),
                "mcp",
                "Enabled MCP server uses an insecure origin",
                &format!("Server '{id}' {reason}."),
                FindingStatus::Critical,
                true,
                Some(path.to_string_lossy().as_ref()),
                Some("Run the safe fix to disable it, then configure HTTPS or a loopback endpoint before re-enabling."),
            ));
        }
    }
}

fn audit_native_skills(runtime: &SecurityRuntimeSnapshot, findings: &mut Vec<SecurityFinding>) {
    if let Some(error) = &runtime.native_skills_error {
        findings.push(finding(
            "skills.discovery",
            "skills",
            "Native skill integrity check failed closed",
            error,
            FindingStatus::Critical,
            false,
            None,
            Some("Review the reported skill collision, symlink, manifest, or digest mismatch before invoking slash skills."),
        ));
        return;
    }
    let mut active = BTreeMap::<&str, &str>::new();
    let mut ineligible = 0usize;
    for skill in &runtime.native_skills {
        if skill.enabled {
            if let Some(existing) = active.insert(&skill.command, &skill.source) {
                findings.push(finding(
                    &format!("skills.collision.{}", short_hash(skill.command.as_bytes())),
                    "skills",
                    "Enabled slash skill command is ambiguous",
                    &format!("/{} is provided by both {existing} and {}.", skill.command, skill.source),
                    FindingStatus::Critical,
                    false,
                    None,
                    Some("Disable or rename one provider. Ambiguous slash commands are never executed."),
                ));
            }
        }
        if skill.enabled && !skill.eligible {
            ineligible += 1;
            findings.push(finding(
                &format!("skills.ineligible.{}", short_hash(skill.command.as_bytes())),
                "skills",
                "Enabled skill is ineligible on this machine",
                &format!(
                    "/{} from {} is missing binaries [{}] or environment entries [{}]. Environment values were not read or reported.",
                    skill.command,
                    skill.source,
                    skill.missing_bins.join(", "),
                    skill.missing_env.join(", ")
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Install the declared dependencies, provide the declared environment entries, or disable this skill."),
            ));
        }
    }
    if !findings
        .iter()
        .any(|finding| finding.id.starts_with("skills.collision"))
    {
        findings.push(finding(
            "skills.integrity",
            "skills",
            "Native skills passed integrity and collision checks",
            &format!(
                "Discovered {} skill(s); managed digests, reserved commands, symlinks, manifests, and active command uniqueness were checked. {ineligible} enabled skill(s) are currently ineligible.",
                runtime.native_skills.len()
            ),
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
    }
}

fn audit_runtime_grants(runtime: &SecurityRuntimeSnapshot, findings: &mut Vec<SecurityFinding>) {
    if let Some(error) = &runtime.browser_error {
        findings.push(finding(
            "grants.browser_unavailable",
            "grants",
            "Browser grants could not be inspected",
            error,
            FindingStatus::Warning,
            false,
            None,
            Some("Stop active browser-verification runs and retry the audit."),
        ));
    } else if runtime.browser_grants.is_empty() && !runtime.browser_observed {
        findings.push(finding(
            "grants.browser_unobservable",
            "grants",
            "Live browser grants are not visible to this process",
            "Use Security Doctor in the running desktop app to inspect its in-memory browser sessions.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
    } else if runtime.browser_grants.is_empty() {
        findings.push(finding(
            "grants.browser_none",
            "grants",
            "No active browser-control grants",
            "Browser sessions are disposable and no run currently holds an origin grant.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
    } else {
        for grant in &runtime.browser_grants {
            let insecure = grant
                .allowed_origins
                .iter()
                .filter(|origin| {
                    Url::parse(origin).is_ok_and(|url| {
                        url.scheme() == "http" && !url.host_str().is_some_and(is_loopback_host)
                    })
                })
                .count();
            let status = if insecure > 0 || grant.allow_loopback {
                FindingStatus::Warning
            } else {
                FindingStatus::Info
            };
            findings.push(finding(
                &format!("grants.browser.{}", short_hash(grant.session_id.as_bytes())),
                "grants",
                "Active browser origin grant",
                &format!(
                    "Run {} session {} can navigate to {} exact origin(s); loopback access is {} and {} granted origin(s) use remote plaintext HTTP.",
                    grant.run_id,
                    grant.session_id,
                    grant.allowed_origins.len(),
                    if grant.allow_loopback { "allowed" } else { "blocked" },
                    insecure
                ),
                status,
                false,
                None,
                Some("Stop the browser session when verification is complete; prefer HTTPS origins and grant loopback only when required."),
            ));
        }
    }

    if let Some(error) = &runtime.companion_error {
        findings.push(finding(
            "grants.companion_unavailable",
            "grants",
            "Companion capture grants could not be inspected",
            error,
            FindingStatus::Warning,
            false,
            None,
            Some("Use the companion emergency stop, then retry the audit."),
        ));
        return;
    }
    if runtime.companion_grants.is_empty() && !runtime.companion_observed {
        findings.push(finding(
            "grants.companion_unobservable",
            "grants",
            "Live companion grants are not visible to this process",
            "Use Security Doctor in the running desktop app to inspect its in-memory capture grants.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
        return;
    }
    let now = now_ms();
    let active = runtime
        .companion_grants
        .iter()
        .filter(|grant| grant.active && grant.expires_at_ms > now)
        .collect::<Vec<_>>();
    if active.is_empty() {
        findings.push(finding(
            "grants.companion_none",
            "grants",
            "No active companion capture grants",
            "Screen, window, microphone, meeting, file, and text capture all require a short-lived explicit grant.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }
    for grant in active {
        let remaining = grant.expires_at_ms.saturating_sub(now);
        let broad = matches!(grant.kind.as_str(), "screen" | "microphone" | "meeting")
            || (grant.kind == "window" && grant.application_id.is_none());
        let invalid_lifetime = remaining > MAX_CAPTURE_GRANT_MS;
        findings.push(finding(
            &format!("grants.companion.{}", short_hash(grant.grant_id.as_bytes())),
            "grants",
            "Active companion capture grant",
            &format!(
                "{} capture is active for at most {} more second(s){}.",
                grant.kind,
                remaining / 1_000,
                grant.application_id.as_ref().map(|id| format!(" and is scoped to {id}")).unwrap_or_default()
            ),
            if invalid_lifetime {
                FindingStatus::Critical
            } else if broad {
                FindingStatus::Warning
            } else {
                FindingStatus::Info
            },
            false,
            None,
            Some("Revoke the grant in Desktop companion or use Emergency stop when capture is no longer needed."),
        ));
    }
}

fn audit_workspace_skill_root(workspace: Option<&Path>, findings: &mut Vec<SecurityFinding>) {
    let Some(workspace) = workspace else {
        findings.push(finding(
            "skills.workspace_none",
            "skills",
            "No workspace skill root is in scope",
            "Open a workspace to include .littlemonkey/skills in the integrity audit.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
        return;
    };
    let root = workspace.join(".littlemonkey").join("skills");
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => findings.push(path_finding(
            "skills.workspace_inspect",
            "skills",
            "Workspace skill root could not be inspected",
            &error.to_string(),
            FindingStatus::Warning,
            false,
            &root,
            Some("Check workspace ownership. Security Doctor never changes workspace permissions."),
        )),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => findings.push(path_finding(
            "skills.workspace_root",
            "skills",
            "Workspace skill root is not a real directory",
            "Workspace skills fail closed when this path is a symlink, file, or special node.",
            FindingStatus::Critical,
            false,
            &root,
            Some("Replace the path with an owned directory after manually reviewing its current target or contents."),
        )),
        Ok(_) => {}
    }
}

/// Reports whether this machine can actually enforce the sandbox it offers.
///
/// K3's acceptance says "a platform without enforcement reports itself as
/// unenforced in Security Doctor", and nothing did: the audit had no isolation
/// check of any kind. Post-run reporting was already honest — a run comes back
/// labelled `ProcessOnly` — but that is after the fact, and the one place a user
/// goes to ask "what is protecting me" said nothing about the boundary that is
/// absent on two of three platforms.
///
/// A `Warning` and not `Critical`: the sandbox is opt-in, so a machine with no
/// kernel boundary is a real limit on a feature the user chose to invoke rather
/// than a live compromise. It is also not `Info`, because
/// `probeGeneratedMcpArtifact` runs **model-authored** MCP server code through
/// this path — the case where the difference between a scrubbed environment and a
/// kernel boundary matters most.
fn audit_sandbox_enforcement(findings: &mut Vec<SecurityFinding>) {
    match crate::sandbox::sandbox_enforcement() {
        SandboxEnforcement::OsEnforced => findings.push(finding(
            "isolation.os_enforced",
            "isolation",
            "Sandboxed runs are confined by the OS",
            "Sandbox runs execute under a generated macOS Seatbelt profile: deny-by-default, \
             writes confined to the run directory, and network denied unless the run opts in. \
             The agent's own shell tool is a separate path and is not sandboxed.",
            FindingStatus::Pass,
            false,
            None,
            None,
        )),
        SandboxEnforcement::ProcessOnly => findings.push(finding(
            "isolation.process_only",
            "isolation",
            "This platform has no OS sandbox",
            "Sandbox runs get a copied workspace, a restricted working directory and a scrubbed \
             environment, but no kernel boundary — a command can still read or write your real \
             files by absolute path. Generated MCP server code is probed through this path, so \
             treat a sandboxed run here as untrusted-code-with-guardrails, not as containment.",
            FindingStatus::Warning,
            false,
            None,
            Some(
                "Review commands and generated MCP servers before running them here. OS \
                 enforcement on this platform (Landlock and seccomp on Linux, a restricted token \
                 and job object on Windows) is not implemented yet — see K3 in \
                 docs/agent-os-roadmap.md.",
            ),
        )),
        SandboxEnforcement::Unavailable => findings.push(finding(
            "isolation.unavailable",
            "isolation",
            "The OS sandbox mechanism is missing",
            "This platform sandboxes through /usr/bin/sandbox-exec and it is not present, so a \
             sandboxed run will fail to start rather than run unconfined. Nothing runs with less \
             isolation than it reports.",
            FindingStatus::Warning,
            false,
            Some("/usr/bin/sandbox-exec"),
            Some(
                "Restore the system binary. Until then the Sandbox panel cannot run; the agent's \
                 own tools are unaffected because they never used it.",
            ),
        )),
    }
}

fn insecure_mcp_reason(raw: &str) -> Option<String> {
    let url = match Url::parse(raw) {
        Ok(url) => url,
        Err(error) => return Some(format!("has an invalid URL: {error}")),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Some("embeds credentials in its URL".to_string());
    }
    match url.scheme() {
        "https" if url.host_str().is_some() => None,
        "http" if url.host_str().is_some_and(is_loopback_host) => None,
        "http" => Some("uses plaintext HTTP to a non-loopback host".to_string()),
        scheme => Some(format!("uses unsupported scheme '{scheme}'")),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn owned_regular_file(path: &Path, owned_root: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "'{}' is not a regular non-symlink file",
            path.display()
        ));
    }
    let canonical_path = path
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize '{}': {error}", path.display()))?;
    let canonical_root = owned_root
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize '{}': {error}", owned_root.display()))?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(format!(
            "'{}' is outside the app-owned daemon directory",
            path.display()
        ));
    }
    Ok(())
}

fn read_bounded_json(path: &Path) -> Result<Option<Value>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not inspect '{}': {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "'{}' is not a regular non-symlink file",
            path.display()
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "'{}' exceeds the configuration size limit",
            path.display()
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("Could not read '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Invalid JSON in '{}': {error}", path.display()))
}

fn atomic_write_private_json(path: &Path, value: &Value, app_data: &Path) -> Result<(), String> {
    if !path.starts_with(app_data) {
        return Err("refusing to modify a path outside Little Monkey app data".to_string());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("refusing to replace a symlink or non-regular configuration".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "configuration has no parent directory".to_string())?;
    let parent_meta = fs::symlink_metadata(parent)
        .map_err(|error| format!("Could not inspect '{}': {error}", parent.display()))?;
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        return Err("configuration parent is not a real directory".to_string());
    }
    let temporary = parent.join(format!(".security-doctor-{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("Could not create safe-fix file: {error}"))?;
    set_private_file_permissions(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|error| format!("Could not write safe-fix file: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not sync safe-fix file: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not publish safe fix: {error}"))?;
        set_private_file_permissions(path)?;
        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Could not sync configuration directory: {error}"))?;
        Ok::<(), String>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not protect '{}': {error}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn certificate_fingerprint(pem: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(pem).map_err(|_| "certificate PEM is not UTF-8".to_string())?;
    let begin = "-----BEGIN CERTIFICATE-----";
    let end = "-----END CERTIFICATE-----";
    let start = text
        .find(begin)
        .ok_or_else(|| "certificate PEM block is missing".to_string())?
        + begin.len();
    let finish = text[start..]
        .find(end)
        .ok_or_else(|| "certificate PEM block is incomplete".to_string())?
        + start;
    let encoded = text[start..finish]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let der = STANDARD
        .decode(encoded)
        .map_err(|error| format!("certificate PEM base64 is invalid: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(der)))
}

fn summarize(findings: &[SecurityFinding]) -> SecuritySummary {
    let mut summary = SecuritySummary::default();
    for finding in findings {
        match finding.status {
            FindingStatus::Pass => summary.passed += 1,
            FindingStatus::Info => summary.informational += 1,
            FindingStatus::Warning => summary.warnings += 1,
            FindingStatus::Critical => summary.critical += 1,
            FindingStatus::Fixed => summary.fixed += 1,
        }
    }
    summary
}

fn finding(
    id: &str,
    category: &str,
    title: &str,
    detail: &str,
    status: FindingStatus,
    fixable: bool,
    path: Option<&str>,
    remediation: Option<&str>,
) -> SecurityFinding {
    SecurityFinding {
        id: id.to_string(),
        category: category.to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        status,
        fixable,
        path: path.map(str::to_string),
        remediation: remediation.map(str::to_string),
    }
}

#[allow(clippy::too_many_arguments)]
fn path_finding(
    id_prefix: &str,
    category: &str,
    title: &str,
    detail: &str,
    status: FindingStatus,
    fixable: bool,
    path: &Path,
    remediation: Option<&str>,
) -> SecurityFinding {
    let text = path.to_string_lossy();
    finding(
        &format!("{id_prefix}.{}", short_hash(text.as_bytes())),
        category,
        title,
        detail,
        status,
        fixable,
        Some(&text),
        remediation,
    )
}

fn short_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))[..12].to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-security-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn request(root: &Path) -> SecurityAuditRequest {
        SecurityAuditRequest {
            app_data_dir: root.to_path_buf(),
            workspace: None,
            deep: false,
            fix: false,
            runtime: SecurityRuntimeSnapshot::default(),
        }
    }

    /// K3's acceptance clause, which nothing implemented: "a platform without
    /// enforcement reports itself as unenforced in Security Doctor". The audit had
    /// no isolation check at all — post-run labelling was honest, but the one
    /// screen a user consults to ask what is protecting them said nothing about a
    /// boundary that is absent on two of three platforms.
    #[test]
    fn the_audit_reports_this_platforms_isolation_and_matches_the_probe() {
        let temp = TestDirectory::new("isolation");
        let report = run_security_audit(&request(&temp.0)).unwrap();

        let isolation: Vec<&SecurityFinding> = report
            .findings
            .iter()
            .filter(|item| item.category == "isolation")
            .collect();
        assert_eq!(
            isolation.len(),
            1,
            "exactly one isolation finding, whatever the platform"
        );
        let finding = isolation[0];

        // Tied to the probe rather than to a hardcoded platform expectation, so
        // the audit and the Sandbox panel cannot drift into disagreeing about the
        // same machine.
        let (expected_id, expected_status) = match crate::sandbox::sandbox_enforcement() {
            SandboxEnforcement::OsEnforced => ("isolation.os_enforced", FindingStatus::Pass),
            SandboxEnforcement::ProcessOnly => ("isolation.process_only", FindingStatus::Warning),
            SandboxEnforcement::Unavailable => ("isolation.unavailable", FindingStatus::Warning),
        };
        assert_eq!(finding.id, expected_id);
        assert_eq!(finding.status, expected_status);

        // A warning with no remediation is a dead end for the user, and the
        // unenforced states are precisely the ones that need one.
        if expected_status == FindingStatus::Warning {
            assert!(finding.remediation.is_some());
            assert_eq!(report.summary.warnings.min(1), 1);
        }
    }

    #[test]
    fn insecure_remote_mcp_is_disabled_without_deleting_configuration() {
        let temp = TestDirectory::new("mcp-fix");
        let path = temp.0.join("mcp_servers.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "servers": [{
                    "id":"remote",
                    "label":"Remote",
                    "transport":{"type":"http","url":"http://example.com/mcp"},
                    "enabled":true
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut audit = request(&temp.0);
        audit.fix = true;
        let report = run_security_audit(&audit).unwrap();
        assert!(report.findings.iter().any(|finding| {
            finding.id.starts_with("mcp.disabled_unsafe") && finding.status == FindingStatus::Fixed
        }));
        let saved = crate::mcp::load_config_impl(&path).unwrap();
        assert!(!saved.servers[0].enabled);
        assert_eq!(saved.servers[0].id, "remote");
    }

    #[test]
    fn unsafe_remote_host_is_only_disabled() {
        let temp = TestDirectory::new("remote-fix");
        let daemon = temp.0.join("daemon");
        fs::create_dir_all(&daemon).unwrap();
        let path = daemon.join("remote-host.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "protocol_version":1,
                "runner_id":"runner-test",
                "listen":"0.0.0.0:9443",
                "advertise_url":"http://example.com",
                "certificate_path":daemon.join("missing-cert.pem"),
                "private_key_path":daemon.join("missing-key.pem"),
                "certificate_sha256":"00",
                "enabled":true
            }))
            .unwrap(),
        )
        .unwrap();
        let mut audit = request(&temp.0);
        audit.fix = true;
        let report = run_security_audit(&audit).unwrap();
        assert!(report.findings.iter().any(|finding| {
            finding.id.starts_with("remote.disabled_unsafe")
                && finding.status == FindingStatus::Fixed
        }));
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.get("enabled").and_then(Value::as_bool), Some(false));
        assert_eq!(
            saved.get("runner_id").and_then(Value::as_str),
            Some("runner-test")
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_fix_restricts_known_app_owned_files() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TestDirectory::new("mode-fix");
        let path = temp.0.join("api_server.json");
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let mut audit = request(&temp.0);
        audit.fix = true;
        let report = run_security_audit(&audit).unwrap();
        assert!(report.summary.fixed >= 1);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn tampered_or_colliding_skill_failure_is_critical() {
        let temp = TestDirectory::new("skill-error");
        let mut audit = request(&temp.0);
        audit.runtime.native_skills_error = Some(
            "skill conflict: managed skill /review changed outside the approved install flow"
                .to_string(),
        );
        let report = run_security_audit(&audit).unwrap();
        assert!(report.findings.iter().any(|finding| {
            finding.id == "skills.discovery" && finding.status == FindingStatus::Critical
        }));
    }

    #[test]
    fn broad_runtime_grants_are_visible() {
        let temp = TestDirectory::new("grants");
        let mut audit = request(&temp.0);
        audit.runtime.browser_grants.push(BrowserGrantSnapshot {
            session_id: "browser-test".to_string(),
            run_id: "run-test".to_string(),
            allowed_origins: vec!["http://127.0.0.1:3000".to_string()],
            allow_loopback: true,
        });
        audit.runtime.companion_grants.push(CompanionGrantSnapshot {
            grant_id: "capture-test".to_string(),
            kind: "screen".to_string(),
            application_id: None,
            expires_at_ms: now_ms() + 30_000,
            active: true,
        });
        let report = run_security_audit(&audit).unwrap();
        assert!(report.summary.warnings >= 2);
    }
}
