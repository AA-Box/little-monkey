use std::fs;
use std::path::PathBuf;

use little_monkey_lib::security_doctor::{
    run_security_audit, FindingStatus, SecurityAuditRequest, SecurityRuntimeSnapshot,
    SECURITY_AUDIT_SCHEMA_VERSION,
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
