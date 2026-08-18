use std::fs;
use std::path::PathBuf;

use little_monkey_lib::security_doctor::{
    append_findings, run_security_audit, DaemonSecurityState, DeviceGrantSnapshot, FindingStatus,
    SecurityAuditRequest, SecurityFinding, SecurityRuntimeSnapshot, SECURITY_AUDIT_SCHEMA_VERSION,
};
use uuid::Uuid;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "little-monkey-security-contract-{}",
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

#[test]
fn public_report_contract_is_versioned_and_safe_fix_is_idempotent() {
    let temp = TestDirectory::new();
    let mcp_path = temp.0.join("mcp_servers.json");
    fs::write(
        &mcp_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "servers": [{
                "id": "unsafe-remote",
                "label": "Unsafe remote",
                "transport": {"type": "http", "url": "http://example.com/mcp"},
                "enabled": true
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let request = SecurityAuditRequest {
        app_data_dir: temp.0.clone(),
        workspace: None,
        deep: true,
        fix: true,
        runtime: SecurityRuntimeSnapshot::default(),
    };
    let fixed = run_security_audit(&request).unwrap();
    assert_eq!(fixed.schema_version, SECURITY_AUDIT_SCHEMA_VERSION);
    assert!(fixed.findings.iter().any(|finding| {
        finding.id.starts_with("mcp.disabled_unsafe") && finding.status == FindingStatus::Fixed
    }));

    let saved = little_monkey_lib::mcp::load_config_impl(&mcp_path).unwrap();
    assert!(!saved.servers[0].enabled);

    let second = run_security_audit(&request).unwrap();
    assert_eq!(second.summary.critical, 0);
    assert!(second.findings.iter().any(|finding| {
        finding.id.starts_with("mcp.insecure_disabled") && finding.status == FindingStatus::Info
    }));
}

/// The wire form the desktop reads from the bundled CLI.
///
/// This envelope is the only thing standing between the desktop Security Doctor
/// and a page that silently omits devices, messaging accounts, phone numbers and
/// peers. The field names are the contract — camelCase, because that is what
/// every other typed bridge payload in this app uses and what the frontend's own
/// `SecurityAuditReport` interface is written against — so they are pinned here
/// rather than left to a derive nobody would notice changing.
#[test]
fn the_daemon_security_state_wire_form_is_camel_case_and_round_trips() {
    let state = DaemonSecurityState {
        schema_version: 1,
        devices: vec![DeviceGrantSnapshot {
            device_id: "device-1".to_string(),
            device_name: "Phone".to_string(),
            granted_physical: vec!["camera".to_string()],
            effective_physical: vec!["camera".to_string()],
            revoked: false,
            last_seen_at_ms: Some(42),
            push_registered: true,
        }],
        device_commands: Vec::new(),
        device_state_observed: true,
        device_state_error: None,
        push: None,
        transport: None,
        findings: vec![SecurityFinding {
            id: "channels.open_direct.acct-1".to_string(),
            category: "channels".to_string(),
            title: "Anyone who finds this account can talk to the agent".to_string(),
            detail: "Work (Telegram) accepts a direct message from any sender.".to_string(),
            status: FindingStatus::Critical,
            fixable: false,
            path: None,
            remediation: Some("Set direct messages to ask for pairing.".to_string()),
        }],
    };

    let encoded = serde_json::to_value(&state).unwrap();
    for key in [
        "schemaVersion",
        "devices",
        "deviceCommands",
        "deviceStateObserved",
        "findings",
    ] {
        assert!(encoded.get(key).is_some(), "missing '{key}' in {encoded}");
    }
    assert_eq!(
        encoded["devices"][0]["grantedPhysical"][0],
        serde_json::json!("camera")
    );
    assert_eq!(
        encoded["findings"][0]["category"],
        serde_json::json!("channels")
    );

    let decoded: DaemonSecurityState = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, state);
}

/// A daemon-owned finding reaches the report *and* the summary above it.
///
/// The summary and the list are rendered from the same report by the same
/// panel, so a finding appended without its count is the one defect a reader
/// cannot see: both halves look plausible alone. This is why `append_findings`
/// is shared rather than reimplemented per caller.
#[test]
fn daemon_findings_are_counted_into_the_summary_the_panel_shows() {
    let temp = TestDirectory::new();
    let mut runtime = SecurityRuntimeSnapshot::default();
    let state = DaemonSecurityState {
        schema_version: 1,
        device_state_observed: true,
        findings: vec![
            SecurityFinding {
                id: "telephony.open_inbound.tel-1".to_string(),
                category: "telephony".to_string(),
                title: "A number answers calls from anyone".to_string(),
                detail: "detail".to_string(),
                status: FindingStatus::Warning,
                fixable: false,
                path: None,
                remediation: None,
            },
            SecurityFinding {
                id: "peers.broad_grant".to_string(),
                category: "peers".to_string(),
                title: "A peer holds a broad grant".to_string(),
                detail: "detail".to_string(),
                status: FindingStatus::Critical,
                fixable: false,
                path: None,
                remediation: None,
            },
        ],
        ..DaemonSecurityState::default()
    };
    let daemon_findings = state.apply(&mut runtime);
    assert!(
        runtime.device_state_observed,
        "the input half must reach the snapshot the library audits"
    );

    let mut report = run_security_audit(&SecurityAuditRequest {
        app_data_dir: temp.0.clone(),
        workspace: None,
        deep: false,
        fix: false,
        runtime,
    })
    .unwrap();
    let before = report.summary.clone();
    append_findings(&mut report, daemon_findings);

    assert_eq!(report.summary.warnings, before.warnings + 1);
    assert_eq!(report.summary.critical, before.critical + 1);
    let categories: Vec<&str> = report
        .findings
        .iter()
        .map(|finding| finding.category.as_str())
        .collect();
    assert!(categories.contains(&"telephony"), "{categories:?}");
    assert!(categories.contains(&"peers"), "{categories:?}");
    // And the totals still add up to the list, which is the invariant a
    // separately-maintained counter breaks.
    let counted = report.summary.passed
        + report.summary.informational
        + report.summary.warnings
        + report.summary.critical
        + report.summary.fixed;
    assert_eq!(counted, report.findings.len());
}
