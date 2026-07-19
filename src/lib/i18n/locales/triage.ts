/**
 * Inbox Triage Agents (ROADMAP.md Phase 3) — English source strings, spread
 * into every locale's dictionary below with a REAL translation per locale
 * (see `de.ts`/`fr.ts`/etc.), matching this app's normal per-locale i18n
 * convention (the same one `issueToPr.ts` established).
 */
export const triageLocale: Record<string, string> = {
  "SettingsModal.tabTriage": "Inbox Triage",
  "TriagePanel.description": "Read-only ranked queues over GitHub issues/PRs, Slack channels, and Jira issues, with draft-only reply/comment/status-update generation. Nothing is posted, sent, or updated without your explicit approval.",
  "TriagePanel.nonGoalNotice": "Gmail and Outlook triage aren't supported: both only expose a real inbox through a registered OAuth application, which this token/keychain-only build doesn't use. Only GitHub, Slack, and Jira are implemented here.",
  "TriagePanel.sourcesHeading": "Queues to refresh",
  "TriagePanel.sourceGithub": "GitHub",
  "TriagePanel.sourceSlack": "Slack",
  "TriagePanel.sourceJira": "Jira",
  "TriagePanel.githubOwnerPlaceholder": "Owner (e.g. acme)",
  "TriagePanel.githubRepoPlaceholder": "Repo (e.g. widgets)",
  "TriagePanel.selectConnectorPlaceholder": "Select a connected account…",
  "TriagePanel.slackChannelPlaceholder": "Channel id (e.g. C0123456789)",
  "TriagePanel.jiraProjectPlaceholder": "Project key (e.g. PROJ)",
  "TriagePanel.addSourceButton": "Add",
  "TriagePanel.removeSourceButton": "Remove",
  "TriagePanel.refreshQueueButton": "Refresh queue",
  "TriagePanel.refreshingButton": "Refreshing…",
  "TriagePanel.queueHeading": "Queue",
  "TriagePanel.itemCountLabel": "{{count}} items",
  "TriagePanel.emptyQueueState": "No items yet. Add a queue above and refresh.",
  "TriagePanel.noSelectionState": "Select an item to see its details and draft a response.",
  "TriagePanel.rankScoreLabel": "Urgency score",
  "TriagePanel.openSourceLink": "Open source",
  "TriagePanel.actionReply": "Slack reply",
  "TriagePanel.actionComment": "GitHub comment",
  "TriagePanel.actionStatusUpdate": "Jira status update",
  "TriagePanel.noDraftYet": "No draft yet — generate one below.",
  "TriagePanel.noModelSelectedNotice": "Select a cloud AI provider and model in Settings → AI Providers to generate a draft.",
  "TriagePanel.generateDraftButton": "Generate draft",
  "TriagePanel.regenerateButton": "Regenerate",
  "TriagePanel.generatingButton": "Generating…",
  "TriagePanel.approveAndSendButton": "Approve & send",
  "TriagePanel.sendingButton": "Sending…",
  "TriagePanel.discardButton": "Discard",
  "TriagePanel.discardedNotice": "Discarded from this session's view.",
};
