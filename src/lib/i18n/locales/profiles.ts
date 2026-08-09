/**
 * Local multi-profile identity (K23) — the Settings "Profiles" tab's strings.
 * English defaults; every other locale file imports this same slice and
 * overrides every key below with a real translation (see de.ts/fr.ts for the
 * pattern this repo follows for a new locale slice).
 */
export const profilesLocale: Record<string, string> = {
  "SettingsModal.tabProfiles": "Profiles",
  "ProfilesPanel.heading": "Local profiles",
  "ProfilesPanel.description":
    "Separate identities on this machine: each profile has its own sessions, run history, artifacts, packages, credentials and share of the machine. Switching restarts the app, because everything currently open belongs to the profile it started under. This is local isolation only — no account, no sign-in, nothing leaves this device.",
  "ProfilesPanel.dismissError": "Dismiss",
  "ProfilesPanel.activeBadge": "Active",
  "ProfilesPanel.share": "{{percent}}% share",
  "ProfilesPanel.switchButton": "Switch",
  "ProfilesPanel.switchConfirm":
    "Switch to {{name}} and restart Little Monkey now? Anything running in this profile stops.",
  "ProfilesPanel.deleteButton": "Delete {{name}}",
  "ProfilesPanel.deleteConfirm":
    "Delete {{name}} and everything in it — sessions, run history and artifacts? This cannot be undone.",
  "ProfilesPanel.newNameLabel": "New profile name",
  "ProfilesPanel.newNamePlaceholder": "e.g. Work",
  "ProfilesPanel.createButton": "Create",
  "ProfilesPanel.weightLabel": "Share weight",
  "ProfilesPanel.maxRunsLabel": "Max runs",
  "ProfilesPanel.maxMemoryLabel": "Max memory (MB)",
  "ProfilesPanel.maxRuntimeLabel": "Max run time (s)",
  "ProfilesPanel.unbounded": "unbounded",
  "ProfilesPanel.applyLimits": "Apply limits",
};
