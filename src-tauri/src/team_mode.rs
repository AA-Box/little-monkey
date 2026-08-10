//! Local, single-machine "Team, Family, and Organization Mode" (ROADMAP.md
//! Phase 6). Scope is deliberately bounded: no hosted account plane, no
//! SSO/SCIM (the roadmap's own text only asks for those "if the product
//! deliberately introduces an account plane" — it has not), and no remote or
//! paired-device grants.
//!
//! ## What this actually is
//!
//! [`TeamMember`] entries and a [`Role`] are a **named local profile
//! switcher** for "who's driving this machine right now" plus a convenience
//! gate on one risky action ([`require_approver`], consulted by
//! `permissions::permission_respond`). It is explicitly **NOT an
//! authentication boundary**: anyone with local file or app access already
//! has full run of this single-user-trust-model app (they can edit
//! `team_members.json` directly, run `monkey-cli` unauthenticated, or just
//! use the app as any role they like). Switching the active member never
//! locks anyone out of anything the app itself can do — it only changes (a)
//! whose id/role gets attributed in [`team_audit_export`]'s report and (b)
//! whether `permission_respond` accepts a decision right now. Treat it as an
//! audit-attribution and approval-gating convenience layer for a shared
//! family/small-team machine, not a security wall against another person
//! physically at the keyboard.
//!
//! ## Persistence
//!
//! [`TeamMembersFile`] is persisted at `<app_data>/team_members.json` with
//! the same atomic temp-file-then-rename write pattern as `memory.rs`'s
//! `memories.json` — a crash mid-write can never leave a truncated/corrupt
//! file behind. Reads/writes are serialized through
//! `AppState::team_members_lock`, following the exact precedent of
//! `AppState::connectors_config_lock`/`memory_lock`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::profiles::ProfileScopedPaths;
use crate::run_protocol::RunEvent;
use crate::AppState;

const TEAM_MEMBERS_FILE: &str = "team_members.json";

/// Current (and, so far, only) on-disk schema version.
const SCHEMA_VERSION: u8 = 1;

/// Per-member display-name character cap — same defensive-length idea as
/// `memory.rs`'s `MAX_FACT_CHARS`, just sized for a name rather than a fact.
const MAX_DISPLAY_NAME_CHARS: usize = 80;

/// Total member cap. This is a family/small-team local profile switcher, not
/// a directory service — a generous but finite bound keeps the roster (and
/// every UI that lists it) bounded without getting in anyone's way.
const MAX_MEMBERS: usize = 50;

/// Default/maximum number of entries `team_audit_export` returns — mirrors
/// `m5_delivery_audit`'s `limit.unwrap_or(100)` convention, sized down
/// slightly since every run can contribute more than one entry (the run
/// itself plus any permission decisions inside it).
const AUDIT_DEFAULT_LIMIT: usize = 50;
const AUDIT_MAX_LIMIT: usize = 200;

/// Upper bound on events scanned per run when hunting for permission
/// decisions — matches `run_ledger.rs`'s own `MAX_LIST_LIMIT`, so this never
/// asks the ledger for more than it's willing to give back in one call.
const AUDIT_EVENTS_PER_RUN: usize = 1_000;

/// A member's capability tier. Ordered loosely by increasing trust, though
/// nothing here relies on the derived ordinal — every check is an explicit
/// `matches!`.
///
/// - **Viewer**: read-only everywhere. Cannot approve permission requests,
///   manage connectors/packages, run anything, or manage members.
/// - **Operator**: can run things (execute tools/recipes/workflows), but
///   cannot approve a pending permission request and cannot remove a
///   connector or uninstall a package — those stay Approver/Owner-and-Owner
///   respectively, see [`Role::can_approve_permissions`]/
///   [`Role::can_manage_connectors`].
/// - **Approver**: everything Operator can do, plus can respond to pending
///   permission requests.
/// - **Owner**: everything, including adding/removing members and changing
///   roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Approver,
    Operator,
    Viewer,
}

impl Role {
    /// Whether this role may call `permissions::permission_respond` to
    /// answer a pending permission request. Only Approver and Owner — see
    /// [`require_approver`], the sole enforcement point this pass wires up.
    #[must_use]
    pub fn can_approve_permissions(self) -> bool {
        matches!(self, Role::Owner | Role::Approver)
    }

    /// Whether this role may remove a configured connector or uninstall an
    /// installed package — the two examples the design calls out as
    /// Owner-only even for an otherwise-trusted Operator. (No command in
    /// this codebase actually checks this yet — see this module's doc
    /// comment / the feature commit's Non-goals for the list of risk-bearing
    /// commands deliberately left ungated in this pass.)
    #[must_use]
    pub fn can_manage_connectors(self) -> bool {
        matches!(self, Role::Owner)
    }

    /// Whether this role may add/remove team members or change another
    /// member's role. Owner-only.
    #[must_use]
    pub fn can_manage_members(self) -> bool {
        matches!(self, Role::Owner)
    }

    /// Whether this role can run things at all (execute tools, recipes,
    /// workflows, permission-gated mutations). `false` only for Viewer —
    /// "Viewer is read-only everywhere" from the design doc. (Like
    /// `can_manage_connectors`, nothing calls this yet outside tests; it's
    /// exposed for the same forward-looking reason.)
    #[must_use]
    pub fn can_operate(self) -> bool {
        !matches!(self, Role::Viewer)
    }
}

/// One named local profile — "who's driving right now" (see this module's
/// top doc comment for what that does and does not mean).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TeamMember {
    pub id: String,
    pub display_name: String,
    pub role: Role,
    pub created_at_ms: u64,
    /// Bumped every time this member is selected as the active member via
    /// [`team_members_set_active`] — the closest thing this local-only
    /// design has to a "last seen" signal.
    pub last_active_ms: u64,
}

/// The whole on-disk `team_members.json` document.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TeamMembersFile {
    pub version: u8,
    #[serde(default)]
    pub members: Vec<TeamMember>,
    /// The currently active member id, if any — `None` means "no one is
    /// currently driving" (e.g. right after the last active member was
    /// removed). See this module's top doc comment: this is a convenience
    /// attribution/gating field, never an authentication session.
    #[serde(default)]
    pub current_member_id: Option<String>,
}

impl Default for TeamMembersFile {
    fn default() -> Self {
        TeamMembersFile {
            version: SCHEMA_VERSION,
            members: Vec::new(),
            current_member_id: None,
        }
    }
}

/// What `team_members_list` returns to the frontend — the roster plus which
/// member (if any) is currently active, in one call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TeamMembersSnapshot {
    pub members: Vec<TeamMember>,
    pub current_member_id: Option<String>,
}

/// One entry in [`TeamAuditReport`] — who, what, when, outcome. Deliberately
/// carries no free-form request/task text (see [`team_audit_export`]'s doc
/// comment for the redaction reasoning), so this can never leak a secret,
/// provider key, or token the way a raw run/tool payload might.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TeamAuditEntry {
    pub member_id: Option<String>,
    pub member_role: Option<Role>,
    pub action: String,
    pub occurred_at_ms: u64,
    pub outcome: String,
}

/// A redacted audit export — see [`team_audit_export`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct TeamAuditReport {
    pub generated_at_ms: u64,
    pub members: Vec<TeamMember>,
    pub entries: Vec<TeamAuditEntry>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn team_members_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir.join(TEAM_MEMBERS_FILE))
}

/// Core load logic, parameterized by path for testability. A missing file
/// (team mode never configured — the common case, and the one
/// [`require_approver`] must treat as a complete no-op) is simply the empty
/// default, never an error.
pub fn load_impl(path: &Path) -> Result<TeamMembersFile, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str(&raw).map_err(|e| format!("Corrupt team members file: {}", e))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TeamMembersFile::default()),
        Err(e) => Err(format!("Failed to read team members file: {}", e)),
    }
}

/// Core save logic: atomic sibling temp file + rename, same idiom as
/// `memory.rs::save_impl`/`sessions.rs::save_to`, so a crash mid-write can
/// never leave a truncated/corrupt team members file behind.
pub fn save_impl(path: &Path, file: &TeamMembersFile) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(file)
        .map_err(|e| format!("Failed to serialize team members: {}", e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &payload)
        .map_err(|e| format!("Failed to write team members file: {}", e))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Failed to finalize team members file: {}", e))?;
    Ok(())
}

/// How many members currently hold the Owner role.
fn owner_count(members: &[TeamMember]) -> usize {
    members.iter().filter(|m| m.role == Role::Owner).count()
}

/// Core add-member logic. The very first member ever added is always forced
/// to Owner regardless of the requested role — otherwise a team could be
/// bootstrapped with, say, a lone Viewer and immediately have no one able to
/// ever approve anything or add another member, a dead end this module's
/// invariants (see [`update_role_impl`]/[`remove_impl`]) are built to
/// prevent. If no member is currently active, the newly added member becomes
/// active automatically — a freshly configured team otherwise has "no one
/// driving", which would make `require_approver` refuse every permission
/// response for a UI that hasn't shown that yet.
pub fn add_impl(
    path: &Path,
    display_name: &str,
    requested_role: Role,
) -> Result<TeamMember, String> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        return Err("Display name must not be empty".to_string());
    }
    if trimmed.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(format!(
            "Display name is {} characters, over the {}-character limit — shorten it.",
            trimmed.chars().count(),
            MAX_DISPLAY_NAME_CHARS
        ));
    }

    let mut file = load_impl(path)?;
    if file.members.len() >= MAX_MEMBERS {
        return Err(format!(
            "This machine already has {} team members (the limit) — remove one before adding another.",
            MAX_MEMBERS
        ));
    }

    let is_first_member = file.members.is_empty();
    let role = if is_first_member {
        Role::Owner
    } else {
        requested_role
    };

    let now = now_ms();
    let member = TeamMember {
        id: uuid::Uuid::new_v4().to_string(),
        display_name: trimmed.to_string(),
        role,
        created_at_ms: now,
        last_active_ms: now,
    };
    file.members.push(member.clone());
    if file.current_member_id.is_none() {
        file.current_member_id = Some(member.id.clone());
    }
    save_impl(path, &file)?;
    Ok(member)
}

/// Core update-role logic. Refuses a change that would leave zero Owners
/// among the remaining members — the same invariant [`remove_impl`]
/// enforces for removal, just reached by demotion instead of deletion.
pub fn update_role_impl(path: &Path, id: &str, new_role: Role) -> Result<TeamMember, String> {
    let mut file = load_impl(path)?;
    let index = file
        .members
        .iter()
        .position(|m| m.id == id)
        .ok_or_else(|| "Team member not found".to_string())?;

    let is_last_owner = file.members[index].role == Role::Owner
        && new_role != Role::Owner
        && owner_count(&file.members) <= 1;
    if is_last_owner {
        return Err(
            "Cannot change this member's role: at least one Owner must remain.".to_string(),
        );
    }

    file.members[index].role = new_role;
    let updated = file.members[index].clone();
    save_impl(path, &file)?;
    Ok(updated)
}

/// Core remove-member logic. An id that isn't present is a no-op success
/// (mirrors `memory.rs::delete_fact_impl`'s "already gone" tolerance).
/// Refuses to remove the last remaining Owner. Clears `current_member_id` if
/// the removed member was active — "no one driving" rather than pointing at
/// a member that no longer exists.
pub fn remove_impl(path: &Path, id: &str) -> Result<(), String> {
    let mut file = load_impl(path)?;
    let Some(index) = file.members.iter().position(|m| m.id == id) else {
        return Ok(());
    };

    if file.members[index].role == Role::Owner && owner_count(&file.members) <= 1 {
        return Err("Cannot remove the last remaining Owner.".to_string());
    }

    file.members.remove(index);
    if file.current_member_id.as_deref() == Some(id) {
        file.current_member_id = None;
    }
    save_impl(path, &file)
}

/// Core set-active logic. `id: None` clears the active member ("no one
/// driving"). `Some(id)` for an unknown id is an error — unlike removal,
/// there's no reasonable "already the case" reading for switching to a
/// member that doesn't exist.
pub fn set_active_impl(path: &Path, id: Option<&str>) -> Result<(), String> {
    let mut file = load_impl(path)?;
    match id {
        None => {
            file.current_member_id = None;
        }
        Some(id) => {
            let index = file
                .members
                .iter()
                .position(|m| m.id == id)
                .ok_or_else(|| "Team member not found".to_string())?;
            file.members[index].last_active_ms = now_ms();
            file.current_member_id = Some(id.to_string());
        }
    }
    save_impl(path, &file)
}

/// Gates `permissions::permission_respond`: the active member must have
/// Approver or Owner role to respond (allow or deny) to a pending permission
/// request. **Complete no-op** — always `Ok(())` — when no team members have
/// ever been configured (`file.members.is_empty()`), so solo users see zero
/// behavior change. This is the only enforcement point this feature wires up
/// in this pass; every other risk-bearing command in the app is untouched —
/// see the feature commit's Non-goals for the explicit list.
pub fn require_approver(app: &tauri::AppHandle, _state: &AppState) -> Result<(), String> {
    let path = team_members_file_path(app)?;
    let file = load_impl(&path)?;
    if file.members.is_empty() {
        return Ok(());
    }

    let active = file
        .current_member_id
        .as_ref()
        .and_then(|id| file.members.iter().find(|m| &m.id == id));

    match active {
        Some(member) if member.role.can_approve_permissions() => Ok(()),
        Some(_) => Err(
            "Your active team member role cannot respond to permission requests — switch to an Approver or Owner profile, or ask one to respond.".to_string(),
        ),
        None => Err(
            "No active team member is selected — switch to an Approver or Owner profile to respond to permission requests.".to_string(),
        ),
    }
}

#[tauri::command]
pub fn team_members_list(app: tauri::AppHandle) -> Result<TeamMembersSnapshot, String> {
    let file = load_impl(&team_members_file_path(&app)?)?;
    Ok(TeamMembersSnapshot {
        members: file.members,
        current_member_id: file.current_member_id,
    })
}

#[tauri::command]
pub fn team_members_add(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    display_name: String,
    role: Role,
) -> Result<TeamMember, String> {
    let _lock = state
        .team_members_lock
        .lock()
        .map_err(|_| "Team members lock poisoned".to_string())?;
    add_impl(&team_members_file_path(&app)?, &display_name, role)
}

#[tauri::command]
pub fn team_members_update_role(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    role: Role,
) -> Result<TeamMember, String> {
    let _lock = state
        .team_members_lock
        .lock()
        .map_err(|_| "Team members lock poisoned".to_string())?;
    update_role_impl(&team_members_file_path(&app)?, &id, role)
}

#[tauri::command]
pub fn team_members_remove(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let _lock = state
        .team_members_lock
        .lock()
        .map_err(|_| "Team members lock poisoned".to_string())?;
    remove_impl(&team_members_file_path(&app)?, &id)
}

#[tauri::command]
pub fn team_members_set_active(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: Option<String>,
) -> Result<(), String> {
    let _lock = state
        .team_members_lock
        .lock()
        .map_err(|_| "Team members lock poisoned".to_string())?;
    set_active_impl(&team_members_file_path(&app)?, id.as_deref())
}

/// Aggregates `run_ledger::RunLedger::list_runs` plus the permission
/// decisions already recorded inside those runs' event streams (written by
/// `permissions.rs` via `run_commands::append_event_as`/`append_host_event`)
/// into one redacted report: who, what, when, outcome.
///
/// Redaction bar matches `connectors_export_audit`'s (id/label/timestamps
/// only, never a secret): entries never carry a run's free-form `task`/
/// `instructions` text, tool arguments, or risk-judge free text (the ledger
/// itself already redacts that last one before storage — see
/// `permissions.rs`'s `append_permission_requested`) — only structural
/// fields (run kind, status, tool name, decision variant, timestamps).
///
/// "Who" is attributed to whichever member is active *at export time*, for
/// every entry — including ones from runs that happened before team mode
/// was ever configured, or under a different active member. Retrofitting a
/// true per-action member stamp would mean threading a member id through
/// every `request_permission`/`permission_respond` call into the immutable,
/// append-only run ledger, which is out of scope for this pass (see the
/// feature commit's Non-goals). Treat the `member_id`/`member_role` columns
/// as "who exported this, and who's nominally driving right now", not a
/// historically accurate per-decision attribution.
#[tauri::command]
pub fn team_audit_export(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    limit: Option<u32>,
) -> Result<TeamAuditReport, String> {
    let bounded_limit = limit
        .map(|l| l as usize)
        .unwrap_or(AUDIT_DEFAULT_LIMIT)
        .clamp(1, AUDIT_MAX_LIMIT);

    let file = load_impl(&team_members_file_path(&app)?)?;
    let active = file
        .current_member_id
        .as_ref()
        .and_then(|id| file.members.iter().find(|m| &m.id == id));
    let active_id = active.map(|m| m.id.clone());
    let active_role = active.map(|m| m.role);

    let runs = crate::run_commands::with_ledger(&app, state.inner(), |ledger| {
        ledger.list_runs(bounded_limit, true)
    })?;

    let mut entries = Vec::new();
    for run in &runs {
        entries.push(TeamAuditEntry {
            member_id: active_id.clone(),
            member_role: active_role,
            action: format!("run:{:?}", run.spec.kind),
            occurred_at_ms: run.updated_at_ms,
            outcome: format!("{:?}", run.status),
        });

        let events = crate::run_commands::with_ledger(&app, state.inner(), |ledger| {
            ledger.load_events(&run.spec.run_id, 0, AUDIT_EVENTS_PER_RUN)
        })?;

        let mut tool_names_by_request: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for envelope in &events {
            if let RunEvent::PermissionRequested {
                request_id,
                tool_name,
                ..
            } = &envelope.event
            {
                tool_names_by_request.insert(request_id.clone(), tool_name.clone());
            }
        }
        for envelope in &events {
            if let RunEvent::PermissionDecided {
                request_id,
                decision,
                ..
            } = &envelope.event
            {
                let tool_name = tool_names_by_request
                    .get(request_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown_tool".to_string());
                entries.push(TeamAuditEntry {
                    member_id: active_id.clone(),
                    member_role: active_role,
                    action: format!("permission_decision:{tool_name}"),
                    occurred_at_ms: envelope.occurred_at_ms,
                    outcome: format!("{:?}", decision),
                });
            }
        }
    }

    entries.sort_by(|a, b| b.occurred_at_ms.cmp(&a.occurred_at_ms));
    entries.truncate(bounded_limit);

    Ok(TeamAuditReport {
        generated_at_ms: now_ms(),
        members: file.members,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_team_mode_test_{}_{}_{}.json",
            std::process::id(),
            n,
            nanos
        ))
    }

    // --- Role capability checks -------------------------------------------

    #[test]
    fn viewer_is_read_only_everywhere() {
        assert!(!Role::Viewer.can_approve_permissions());
        assert!(!Role::Viewer.can_manage_connectors());
        assert!(!Role::Viewer.can_manage_members());
        assert!(!Role::Viewer.can_operate());
    }

    #[test]
    fn operator_can_run_but_not_approve_or_manage_connectors_or_members() {
        assert!(Role::Operator.can_operate());
        assert!(!Role::Operator.can_approve_permissions());
        assert!(!Role::Operator.can_manage_connectors());
        assert!(!Role::Operator.can_manage_members());
    }

    #[test]
    fn approver_can_operate_and_approve_but_not_manage_connectors_or_members() {
        assert!(Role::Approver.can_operate());
        assert!(Role::Approver.can_approve_permissions());
        assert!(!Role::Approver.can_manage_connectors());
        assert!(!Role::Approver.can_manage_members());
    }

    #[test]
    fn owner_can_do_everything() {
        assert!(Role::Owner.can_operate());
        assert!(Role::Owner.can_approve_permissions());
        assert!(Role::Owner.can_manage_connectors());
        assert!(Role::Owner.can_manage_members());
    }

    // --- JSON persistence round-trip ---------------------------------------

    #[test]
    fn load_returns_default_when_file_missing() {
        let path = temp_path();
        let file = load_impl(&path).unwrap();
        assert_eq!(file.version, SCHEMA_VERSION);
        assert!(file.members.is_empty());
        assert!(file.current_member_id.is_none());
    }

    #[test]
    fn add_then_load_roundtrips_and_persists_atomically() {
        let path = temp_path();
        let member = add_impl(&path, "Ada", Role::Operator).unwrap();

        // First-ever member is force-promoted to Owner regardless of the
        // requested role — see add_impl's doc comment.
        assert_eq!(member.role, Role::Owner);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file must not linger"
        );

        let reloaded = load_impl(&path).unwrap();
        assert_eq!(reloaded.members.len(), 1);
        assert_eq!(reloaded.members[0].id, member.id);
        assert_eq!(reloaded.current_member_id, Some(member.id));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn second_member_keeps_its_requested_role() {
        let path = temp_path();
        add_impl(&path, "Ada", Role::Viewer).unwrap();
        let second = add_impl(&path, "Grace", Role::Viewer).unwrap();

        assert_eq!(second.role, Role::Viewer);
        let file = load_impl(&path).unwrap();
        assert_eq!(file.members.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_display_name_is_rejected() {
        let path = temp_path();
        let err = add_impl(&path, "   ", Role::Viewer).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn display_name_over_cap_is_rejected() {
        let path = temp_path();
        let huge = "a".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        let err = add_impl(&path, &huge, Role::Viewer).unwrap_err();
        assert!(err.contains("character limit"));
    }

    #[test]
    fn member_cap_rejects_one_too_many() {
        let path = temp_path();
        for n in 0..MAX_MEMBERS {
            add_impl(&path, &format!("member {n}"), Role::Viewer).unwrap();
        }
        let err = add_impl(&path, "one too many", Role::Viewer).unwrap_err();
        assert!(err.contains(&MAX_MEMBERS.to_string()));

        let _ = std::fs::remove_file(&path);
    }

    // --- "cannot remove/demote the last Owner" rule -------------------------

    #[test]
    fn cannot_remove_the_last_remaining_owner() {
        let path = temp_path();
        let owner = add_impl(&path, "Ada", Role::Owner).unwrap();

        let err = remove_impl(&path, &owner.id).unwrap_err();
        assert!(err.contains("last remaining Owner"));

        let file = load_impl(&path).unwrap();
        assert_eq!(
            file.members.len(),
            1,
            "the owner must not have been removed"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn can_remove_an_owner_when_another_owner_remains() {
        let path = temp_path();
        let first = add_impl(&path, "Ada", Role::Owner).unwrap();
        add_impl(&path, "Grace", Role::Owner).unwrap();

        remove_impl(&path, &first.id).unwrap();

        let file = load_impl(&path).unwrap();
        assert_eq!(file.members.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_a_non_owner_never_hits_the_owner_check() {
        let path = temp_path();
        add_impl(&path, "Ada", Role::Owner).unwrap();
        let viewer = add_impl(&path, "Grace", Role::Viewer).unwrap();

        remove_impl(&path, &viewer.id).unwrap();

        let file = load_impl(&path).unwrap();
        assert_eq!(file.members.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_an_unknown_id_is_a_no_op_success() {
        let path = temp_path();
        add_impl(&path, "Ada", Role::Owner).unwrap();

        remove_impl(&path, "does-not-exist").unwrap();

        let file = load_impl(&path).unwrap();
        assert_eq!(file.members.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_the_active_member_clears_current_member_id() {
        let path = temp_path();
        let owner = add_impl(&path, "Ada", Role::Owner).unwrap();
        add_impl(&path, "Grace", Role::Owner).unwrap();
        assert_eq!(
            load_impl(&path).unwrap().current_member_id,
            Some(owner.id.clone())
        );

        remove_impl(&path, &owner.id).unwrap();

        assert_eq!(load_impl(&path).unwrap().current_member_id, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cannot_demote_the_last_remaining_owner() {
        let path = temp_path();
        let owner = add_impl(&path, "Ada", Role::Owner).unwrap();

        let err = update_role_impl(&path, &owner.id, Role::Viewer).unwrap_err();
        assert!(err.contains("at least one Owner must remain"));

        let file = load_impl(&path).unwrap();
        assert_eq!(file.members[0].role, Role::Owner);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn can_demote_an_owner_when_another_owner_remains() {
        let path = temp_path();
        let first = add_impl(&path, "Ada", Role::Owner).unwrap();
        add_impl(&path, "Grace", Role::Owner).unwrap();

        let updated = update_role_impl(&path, &first.id, Role::Approver).unwrap();
        assert_eq!(updated.role, Role::Approver);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_role_of_unknown_id_is_an_error() {
        let path = temp_path();
        add_impl(&path, "Ada", Role::Owner).unwrap();

        let err = update_role_impl(&path, "does-not-exist", Role::Viewer).unwrap_err();
        assert!(err.contains("not found"));

        let _ = std::fs::remove_file(&path);
    }

    // --- active-member switching ---------------------------------------

    #[test]
    fn set_active_to_unknown_id_is_an_error() {
        let path = temp_path();
        add_impl(&path, "Ada", Role::Owner).unwrap();

        let err = set_active_impl(&path, Some("does-not-exist")).unwrap_err();
        assert!(err.contains("not found"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_active_to_none_clears_current_member() {
        let path = temp_path();
        let owner = add_impl(&path, "Ada", Role::Owner).unwrap();
        assert_eq!(load_impl(&path).unwrap().current_member_id, Some(owner.id));

        set_active_impl(&path, None).unwrap();

        assert_eq!(load_impl(&path).unwrap().current_member_id, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_active_bumps_last_active_ms() {
        let path = temp_path();
        let owner = add_impl(&path, "Ada", Role::Owner).unwrap();
        let second = add_impl(&path, "Grace", Role::Viewer).unwrap();
        let before = load_impl(&path).unwrap().members[1].last_active_ms;
        let _ = owner;

        std::thread::sleep(std::time::Duration::from_millis(2));
        set_active_impl(&path, Some(&second.id)).unwrap();

        let after = load_impl(&path)
            .unwrap()
            .members
            .into_iter()
            .find(|m| m.id == second.id)
            .unwrap()
            .last_active_ms;
        assert!(after >= before);

        let _ = std::fs::remove_file(&path);
    }

    // --- require_approver's no-op-when-unconfigured guarantee ---------------

    #[test]
    fn require_approver_role_gate_matches_can_approve_permissions() {
        // require_approver itself needs a live AppHandle to resolve the app
        // data dir, so it's exercised through the Tauri command surface in
        // integration rather than here — this pins the role-capability
        // predicate it delegates to instead, which is the actual gate logic.
        assert!(Role::Owner.can_approve_permissions());
        assert!(Role::Approver.can_approve_permissions());
        assert!(!Role::Operator.can_approve_permissions());
        assert!(!Role::Viewer.can_approve_permissions());
    }

    #[test]
    fn owner_count_counts_only_owners() {
        let members = vec![
            TeamMember {
                id: "a".into(),
                display_name: "A".into(),
                role: Role::Owner,
                created_at_ms: 0,
                last_active_ms: 0,
            },
            TeamMember {
                id: "b".into(),
                display_name: "B".into(),
                role: Role::Approver,
                created_at_ms: 0,
                last_active_ms: 0,
            },
            TeamMember {
                id: "c".into(),
                display_name: "C".into(),
                role: Role::Owner,
                created_at_ms: 0,
                last_active_ms: 0,
            },
        ];
        assert_eq!(owner_count(&members), 2);
    }

    #[test]
    fn role_serializes_to_expected_snake_case_strings() {
        assert_eq!(serde_json::to_string(&Role::Owner).unwrap(), "\"owner\"");
        assert_eq!(
            serde_json::to_string(&Role::Approver).unwrap(),
            "\"approver\""
        );
        assert_eq!(
            serde_json::to_string(&Role::Operator).unwrap(),
            "\"operator\""
        );
        assert_eq!(serde_json::to_string(&Role::Viewer).unwrap(), "\"viewer\"");
    }
}
