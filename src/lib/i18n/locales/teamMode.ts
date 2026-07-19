/**
 * Team, Family, and Organization Mode (ROADMAP.md Phase 6) — the Settings
 * "Team" tab's strings. English defaults; every other locale file imports
 * this same slice and overrides every key below with a real translation
 * (see de.ts/fr.ts for the pattern this repo follows for a new locale
 * slice).
 */
export const teamModeLocale: Record<string, string> = {
  "SettingsModal.tabTeamMode": "Team",
  "TeamModePanel.membersHeading": "Team members",
  "TeamModePanel.membersDescription":
    "A named local profile switcher for who's driving this machine right now — not an authentication boundary. Anyone with local access to this app already has full run of it; this only changes audit attribution and who can respond to permission requests.",
  "TeamModePanel.dismissError": "Dismiss",
  "TeamModePanel.activeSwitcherLabel": "Active member",
  "TeamModePanel.activeSwitcherNone": "No one selected",
  "TeamModePanel.membersEmpty": "No team members configured yet — this app behaves exactly as it does for a solo user until you add one.",
  "TeamModePanel.addNameLabel": "Display name",
  "TeamModePanel.addNamePlaceholder": "e.g. Alex",
  "TeamModePanel.addRoleLabel": "Role",
  "TeamModePanel.addButton": "Add member",
  "TeamModePanel.firstMemberIsOwnerHint": "The first member you add always becomes Owner, regardless of the role selected.",
  "TeamModePanel.auditHeading": "Audit export",
  "TeamModePanel.auditDescription": "A redacted report of recent runs and permission decisions — no provider keys, tokens, or other secrets are ever included.",
  "TeamModePanel.exportButton": "Export audit report",
  "TeamModePanel.roleOwner": "Owner",
  "TeamModePanel.roleApprover": "Approver",
  "TeamModePanel.roleOperator": "Operator",
  "TeamModePanel.roleViewer": "Viewer",
  "TeamModePanel.activeBadge": "Active",
  "TeamModePanel.lastActive": "Last active {{date}}",
  "TeamModePanel.roleSelectAriaLabel": "Role for {{name}}",
  "TeamModePanel.removeButton": "Remove {{name}}",
  "TeamModePanel.removeConfirm": "Remove {{name}} from this team? This cannot be undone.",
};
