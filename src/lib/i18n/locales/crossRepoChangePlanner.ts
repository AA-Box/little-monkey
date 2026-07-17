/**
 * Cross-Repo Change Planner (ROADMAP.md Phase 7, item 12) — English source
 * strings, spread into every locale's dictionary below with a REAL
 * translation per locale (see `de.ts`/`fr.ts`/etc.), matching this app's
 * per-feature-file i18n convention (see `issueToPr.ts`).
 */
export const crossRepoChangePlannerLocale: Record<string, string> = {
  "AppMenu.crossRepoChangePlanner": "Cross-Repo Change Planner",
  "CrossRepoChangePlanner.title": "Cross-Repo Change Planner",
  "CrossRepoChangePlanner.subtitle": "Describe a coordinated change once. Review an ordered, per-root plan with risk and rollback notes, then approve it before anything touches a repository.",
  "CrossRepoChangePlanner.close": "Close Cross-Repo Change Planner",
  "CrossRepoChangePlanner.noRootsWarning": "No workspace roots are attached. Open a primary folder (and optionally attach secondary folders) before planning a cross-repo change.",
  "CrossRepoChangePlanner.descriptionLabel": "Describe the coordinated change",
  "CrossRepoChangePlanner.descriptionPlaceholder": "e.g. Rename the `widgetId` field to `widgetKey` across the API, the web client, and the docs.",
  "CrossRepoChangePlanner.generateButton": "Generate plan",
  "CrossRepoChangePlanner.statusDraft": "Draft — not yet approved",
  "CrossRepoChangePlanner.statusApproved": "Approved",
  "CrossRepoChangePlanner.approveButton": "Approve plan",
  "CrossRepoChangePlanner.approveGateNote": "Nothing touches a repository until you approve this plan. Edit any step's text or reorder steps first if needed.",
  "CrossRepoChangePlanner.startOverButton": "Start over",
  "CrossRepoChangePlanner.moveUp": "Move step up",
  "CrossRepoChangePlanner.moveDown": "Move step down",
  "CrossRepoChangePlanner.dependsOn": "Depends on: {{roots}}",
  "CrossRepoChangePlanner.summaryLabel": "Summary",
  "CrossRepoChangePlanner.changesLabel": "What changes",
  "CrossRepoChangePlanner.risksLabel": "What could break",
  "CrossRepoChangePlanner.rollbackLabel": "Rollback",
  "CrossRepoChangePlanner.branchSectionHeading": "Branch creation",
  "CrossRepoChangePlanner.branchCreated": "Branch created: {{branch}}",
  "CrossRepoChangePlanner.repositorySlugLabel": "GitHub repository",
  "CrossRepoChangePlanner.baseRefLabel": "Base ref",
  "CrossRepoChangePlanner.branchPrefixLabel": "Owned branch prefix",
  "CrossRepoChangePlanner.labelLabel": "Task label",
  "CrossRepoChangePlanner.createBranchButton": "Create branch",
  "CrossRepoChangePlanner.approveFirstHint": "Approve the plan before creating a branch for this step.",
  "CrossRepoChangePlanner.confirmTypePhrase": "Type {{phrase}} to confirm",
  "CrossRepoChangePlanner.confirmCancel": "Cancel",
  "CrossRepoChangePlanner.confirmExecute": "Confirm",
  "CrossRepoChangePlanner.pushFollowUpNote": "This only creates local owned branches. Pushing a branch and opening a draft PR stays a manual follow-up in Settings → Git delivery, with its own confirm-and-type-the-phrase step, exactly like the Issue-to-PR flow.",
};
