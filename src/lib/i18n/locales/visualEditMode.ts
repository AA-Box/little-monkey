/**
 * Visual Design Edit Mode (ROADMAP.md Phase 7: "Market-Defining
 * Differentiators") — English source of truth for the
 * `VisualEditModePanel.*` / `AppMenu.visualEditMode` key namespace. Copied
 * and spread into every other locale file, then every key below is
 * overridden there with a real translation (see de.ts/fr.ts/etc.) — mirrors
 * the structure every other feature locale slice in this directory uses.
 */
export const visualEditModeLocale: Record<string, string> = {
  "AppMenu.visualEditMode": "Visual Design Edit Mode",
  "VisualEditModePanel.title": "Visual Design Edit Mode",
  "VisualEditModePanel.subtitle": "Pick an element in Browser Workbench, describe the change, review the source patch",
  "VisualEditModePanel.close": "Close Visual Design Edit Mode",
  "VisualEditModePanel.pickElement.title": "1. Pick an element",
  "VisualEditModePanel.pickElement.description": "Uses an already-open Browser Workbench session — start one from the sidebar first, navigate to your page, then select it here.",
  "VisualEditModePanel.pickElement.session": "Browser session",
  "VisualEditModePanel.pickElement.noSessions": "No open Browser Workbench sessions",
  "VisualEditModePanel.pickElement.refreshSessions": "Refresh sessions",
  "VisualEditModePanel.pickElement.selector": "CSS selector",
  "VisualEditModePanel.pickElement.selectorPlaceholder": "e.g. button.cta, #submit-button",
  "VisualEditModePanel.pickElement.capture": "Capture element",
  "VisualEditModePanel.describeChange": "2. Describe your change",
  "VisualEditModePanel.describeChangePlaceholder": "e.g. \"make this button larger\" or \"change this to blue\"",
  "VisualEditModePanel.generate": "Generate patch",
  "VisualEditModePanel.generating": "Searching source files and asking the model for a patch…",
  "VisualEditModePanel.empty": "No visual edits yet — capture an element above and describe a change to get started.",
  "VisualEditModePanel.before": "Before",
  "VisualEditModePanel.after": "After",
  "VisualEditModePanel.noScreenshot": "No screenshot captured",
  "VisualEditModePanel.accept": "Accept",
  "VisualEditModePanel.reject": "Reject",
  "VisualEditModePanel.replay": "Replay",
  "VisualEditModePanel.dismiss": "Dismiss",
  "VisualEditModePanel.acceptedNote": "Written to {{file}}",
  "VisualEditModePanel.status.generating": "generating",
  "VisualEditModePanel.status.pending": "pending review",
  "VisualEditModePanel.status.accepted": "accepted",
  "VisualEditModePanel.status.rejected": "rejected",
  "VisualEditModePanel.status.error": "error",
};
