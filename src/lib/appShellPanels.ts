/**
 * The center workspace can show exactly one feature surface at a time.
 * Right-sidebar tabs deliberately live outside this state so switching the
 * center surface never closes a terminal, browser, review, files, or task tab.
 */
export const FEATURE_PANEL_IDS = [
  "settings",
  "run-center",
  "browser-workbench",
  "design-to-app",
  "issue-to-pr",
  "production-debugging",
  "incident-commander",
  "security-autofix",
  "trust-scorecards",
  "sop-compiler",
  "mcp-generator",
  "connector-builder",
  "migration-agent",
  "global-search",
  "agent-inbox",
  "red-team-lab",
  "knowledge-graph",
  "spreadsheet-copilot",
  "evidence-board",
  "golden-dataset-builder",
  "daily-brief",
  "data-notebook",
  "synthetic-monitoring",
  "cross-repo-intelligence",
  "work-canvas",
  "pm-copilot",
  "deep-research",
  "brief-studio",
  "cross-repo-planner",
  "visual-edit-mode",
  "debate",
  "db-admin-guardrails",
  "api-contract-diff-lab",
  "workflow-test-harness",
] as const;

export type FeaturePanelId = (typeof FEATURE_PANEL_IDS)[number];
export type FeaturePanelState = FeaturePanelId | null;

export type FeaturePanelAction =
  | { type: "open"; panel: FeaturePanelId }
  | { type: "close"; panel: FeaturePanelId }
  | { type: "reset" };

/**
 * A stale close event from a panel that was just replaced must not close the
 * newly opened surface. This reducer makes that invariant explicit.
 */
export function featurePanelReducer(
  state: FeaturePanelState,
  action: FeaturePanelAction,
): FeaturePanelState {
  switch (action.type) {
    case "open":
      return action.panel;
    case "close":
      return state === action.panel ? null : state;
    case "reset":
      return null;
  }
}
