/**
 * Deep Research Workspace (ROADMAP.md Phase 7) — English source-of-truth
 * strings, spread into `en.ts` and imported (then overridden with real
 * translations) by every other locale, same convention as
 * `dailyBrief.ts`/`inbox.ts`.
 */
export const deepResearchLocale: Record<string, string> = {
  "AppMenu.deepResearch": "Deep Research",
  "DeepResearchWorkspacePanel.title": "Deep Research Workspace",
  "DeepResearchWorkspacePanel.subtitle":
    "Plan and run a multi-step research question across the web, your workspace files, and knowledge stacks — every conclusion cites its evidence.",
  "DeepResearchWorkspacePanel.close": "Close deep research",
  "DeepResearchWorkspacePanel.questionLabel": "Research question",
  "DeepResearchWorkspacePanel.questionPlaceholder": "What do you want to research?",
  "DeepResearchWorkspacePanel.start": "Start research",
  "DeepResearchWorkspacePanel.cancel": "Cancel",
  "DeepResearchWorkspacePanel.planTitle": "Plan & source map",
  "DeepResearchWorkspacePanel.sourceMapSummary": "{{searched}} searched · {{skipped}} skipped",
  "DeepResearchWorkspacePanel.evidenceCount": "{{count}} evidence snippet(s)",
  "DeepResearchWorkspacePanel.stepStatus.queued": "Queued",
  "DeepResearchWorkspacePanel.stepStatus.active": "Searching…",
  "DeepResearchWorkspacePanel.stepStatus.searched": "Searched",
  "DeepResearchWorkspacePanel.stepStatus.skipped": "Skipped",
  "DeepResearchWorkspacePanel.stepStatus.error": "Error",
  "DeepResearchWorkspacePanel.runStatus.planning": "Planning",
  "DeepResearchWorkspacePanel.runStatus.researching": "Researching",
  "DeepResearchWorkspacePanel.runStatus.synthesizing": "Synthesizing",
  "DeepResearchWorkspacePanel.runStatus.done": "Done",
  "DeepResearchWorkspacePanel.runStatus.error": "Error",
  "DeepResearchWorkspacePanel.runStatus.cancelled": "Cancelled",
  "DeepResearchWorkspacePanel.reportTitle": "Report",
  "DeepResearchWorkspacePanel.noClaims": "No evidence-linked conclusions were produced for this run.",
  "DeepResearchWorkspacePanel.droppedClaims": "{{count}} claim(s) were discarded for citing no valid evidence.",
  "DeepResearchWorkspacePanel.openQuestionsTitle": "Open questions",
  "DeepResearchWorkspacePanel.emptyState": "Ask a research question above to get started.",
};
