import { useEffect, useState } from "react";
import { Download, Plus, Trash2, UserCheck } from "lucide-react";
import { Button, StatusPill, type PillTone } from "../ui";
import { useT } from "../../lib/i18n";
import { useTeamModeStore, type TeamAuditReport, type TeamMember, type TeamRole } from "../../store/teamModeStore";

const ROLES: TeamRole[] = ["owner", "approver", "operator", "viewer"];

const ROLE_TONE: Record<TeamRole, PillTone> = {
  owner: "success",
  approver: "neutral",
  operator: "warning",
  viewer: "neutral",
};

function formatTimestamp(ms: number): string {
  return new Date(ms).toLocaleString();
}

/** One member row: role badge, a role-change select (Owner-gated by the
 * backend, not this component — an unauthorized change simply comes back as
 * an error from `updateRole`/`removeMember`), and a confirm-gated remove. */
function MemberRow({
  member,
  isActive,
  roleLabel,
  onRoleChange,
  onRemove,
}: {
  member: TeamMember;
  isActive: boolean;
  roleLabel: (role: TeamRole) => string;
  onRoleChange: (role: TeamRole) => void;
  onRemove: () => void;
}) {
  const { t } = useT();
  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border bg-background p-3 sm:flex-row sm:items-center sm:justify-between">
      <div className="flex min-w-0 items-center gap-2">
        {isActive && <UserCheck size={14} className="shrink-0 text-accent" aria-label={t("TeamModePanel.activeBadge")} />}
        <div className="min-w-0">
          <p className="truncate text-sm font-medium text-foreground">{member.display_name}</p>
          <p className="truncate text-[11px] text-faint">
            {t("TeamModePanel.lastActive", { date: formatTimestamp(member.last_active_ms) })}
          </p>
        </div>
        <StatusPill tone={ROLE_TONE[member.role]}>{roleLabel(member.role)}</StatusPill>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <select
          value={member.role}
          onChange={(event) => onRoleChange(event.target.value as TeamRole)}
          className="h-8 rounded-md border border-border bg-surface px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
          aria-label={t("TeamModePanel.roleSelectAriaLabel", { name: member.display_name })}
        >
          {ROLES.map((role) => (
            <option key={role} value={role}>
              {roleLabel(role)}
            </option>
          ))}
        </select>
        <Button
          size="sm"
          variant="ghost"
          onClick={onRemove}
          className="text-muted hover:text-danger"
          aria-label={t("TeamModePanel.removeButton", { name: member.display_name })}
        >
          <Trash2 size={12} />
        </Button>
      </div>
    </div>
  );
}

/**
 * Settings "Team" tab: local Team/Family/Organization Mode (ROADMAP.md Phase
 * 6). A named local profile switcher over `team_mode.rs` — member roster
 * with role badges, an active-member switcher, and a redacted audit export.
 * See `team_mode.rs`'s module doc for what "active member" does and does not
 * guarantee (it is explicitly not an authentication boundary).
 */
export function TeamModePanel() {
  const { t } = useT();
  const members = useTeamModeStore((s) => s.members);
  const currentMemberId = useTeamModeStore((s) => s.currentMemberId);
  const busy = useTeamModeStore((s) => s.busy);
  const error = useTeamModeStore((s) => s.error);
  const clearError = useTeamModeStore((s) => s.clearError);
  const refresh = useTeamModeStore((s) => s.refresh);
  const addMember = useTeamModeStore((s) => s.addMember);
  const updateRole = useTeamModeStore((s) => s.updateRole);
  const removeMember = useTeamModeStore((s) => s.removeMember);
  const setActive = useTeamModeStore((s) => s.setActive);
  const exportAudit = useTeamModeStore((s) => s.exportAudit);

  const [newName, setNewName] = useState("");
  const [newRole, setNewRole] = useState<TeamRole>("viewer");
  const [auditReport, setAuditReport] = useState<TeamAuditReport | null>(null);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const roleLabel = (role: TeamRole): string => {
    switch (role) {
      case "owner":
        return t("TeamModePanel.roleOwner");
      case "approver":
        return t("TeamModePanel.roleApprover");
      case "operator":
        return t("TeamModePanel.roleOperator");
      case "viewer":
        return t("TeamModePanel.roleViewer");
    }
  };

  async function handleAdd() {
    try {
      await addMember(newName, newRole);
      setNewName("");
      setNewRole("viewer");
    } catch {
      // Error surfaced via the store's `error` field below.
    }
  }

  async function handleRemove(member: TeamMember) {
    if (!window.confirm(t("TeamModePanel.removeConfirm", { name: member.display_name }))) return;
    try {
      await removeMember(member.id);
    } catch {
      // Error surfaced via the store's `error` field below.
    }
  }

  async function handleExport() {
    try {
      const report = await exportAudit();
      setAuditReport(report);
    } catch {
      // Error surfaced via the store's `error` field below.
    }
  }

  return (
    <div className="flex flex-col gap-4 py-2">
      <section>
        <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
          {t("TeamModePanel.membersHeading")}
        </h3>
        <p className="mb-3 text-xs text-muted">{t("TeamModePanel.membersDescription")}</p>

        {error && (
          <div className="mb-3 flex items-center justify-between gap-2 rounded-md bg-danger-soft px-2.5 py-1.5 text-xs text-danger">
            <span>{error}</span>
            <button type="button" onClick={clearError} className="shrink-0 underline">
              {t("TeamModePanel.dismissError")}
            </button>
          </div>
        )}

        <div className="mb-3 flex flex-col gap-1.5">
          <label className="text-xs font-medium text-muted" htmlFor="team-active-member">
            {t("TeamModePanel.activeSwitcherLabel")}
          </label>
          <select
            id="team-active-member"
            value={currentMemberId ?? ""}
            onChange={(event) => void setActive(event.target.value === "" ? null : event.target.value)}
            disabled={busy}
            className="h-8 w-full max-w-xs rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
          >
            <option value="">{t("TeamModePanel.activeSwitcherNone")}</option>
            {members.map((member) => (
              <option key={member.id} value={member.id}>
                {member.display_name} — {roleLabel(member.role)}
              </option>
            ))}
          </select>
        </div>

        <div className="flex flex-col gap-2">
          {members.length === 0 ? (
            <p className="text-xs text-faint">{t("TeamModePanel.membersEmpty")}</p>
          ) : (
            members.map((member) => (
              <MemberRow
                key={member.id}
                member={member}
                isActive={member.id === currentMemberId}
                roleLabel={roleLabel}
                onRoleChange={(role) => void updateRole(member.id, role)}
                onRemove={() => void handleRemove(member)}
              />
            ))
          )}
        </div>

        <div className="mt-3 flex flex-col gap-1.5 rounded-lg border border-dashed border-border p-2.5 sm:flex-row sm:items-end sm:gap-2">
          <div className="flex-1">
            <label className="text-xs font-medium text-muted" htmlFor="team-new-name">
              {t("TeamModePanel.addNameLabel")}
            </label>
            <input
              id="team-new-name"
              type="text"
              value={newName}
              onChange={(event) => setNewName(event.target.value)}
              placeholder={t("TeamModePanel.addNamePlaceholder")}
              className="mt-1 h-8 w-full rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            />
          </div>
          <div>
            <label className="text-xs font-medium text-muted" htmlFor="team-new-role">
              {t("TeamModePanel.addRoleLabel")}
            </label>
            <select
              id="team-new-role"
              value={newRole}
              onChange={(event) => setNewRole(event.target.value as TeamRole)}
              className="mt-1 h-8 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            >
              {ROLES.map((role) => (
                <option key={role} value={role}>
                  {roleLabel(role)}
                </option>
              ))}
            </select>
          </div>
          <Button size="sm" variant="primary" onClick={() => void handleAdd()} disabled={busy || newName.trim().length === 0}>
            <Plus size={12} />
            {t("TeamModePanel.addButton")}
          </Button>
        </div>
        {members.length === 0 && <p className="mt-1.5 text-[11px] text-faint">{t("TeamModePanel.firstMemberIsOwnerHint")}</p>}
      </section>

      <section>
        <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
          {t("TeamModePanel.auditHeading")}
        </h3>
        <p className="mb-2 text-xs text-muted">{t("TeamModePanel.auditDescription")}</p>
        <Button size="sm" onClick={() => void handleExport()} disabled={busy}>
          <Download size={12} />
          {t("TeamModePanel.exportButton")}
        </Button>
        {auditReport && (
          <pre className="mt-2 max-h-64 overflow-auto rounded-md bg-background p-2 text-[10px] text-muted">
            {JSON.stringify(auditReport, null, 2)}
          </pre>
        )}
      </section>
    </div>
  );
}
