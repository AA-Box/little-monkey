/**
 * SOP-to-Agent Compiler (ROADMAP.md Phase 7, item 24) — English source
 * strings, spread into every locale's dictionary below with a REAL
 * translation per locale (see `de.ts`/`fr.ts`/etc.), matching this app's
 * normal per-locale i18n convention (`issueToPr.ts` is the reference this
 * mirrors).
 */
export const sopCompilerLocale: Record<string, string> = {
  "AppMenu.sopCompiler": "SOP-to-Agent Compiler",
  "SopCompiler.title": "SOP-to-Agent Compiler",
  "SopCompiler.subtitle": "Paste or import an SOP, runbook, checklist, or training document and compile it into a draft workflow with inputs, policy gates, tests, and evidence requirements.",
  "SopCompiler.close": "Close SOP-to-Agent Compiler",
  "SopCompiler.sourceLabel": "SOP / runbook / checklist text",
  "SopCompiler.sourcePlaceholder": "Paste the SOP, runbook, checklist, or training document text here…",
  "SopCompiler.importButton": "Import file",
  "SopCompiler.compileButton": "Compile",
  "SopCompiler.inactiveNotice": "Compiling never runs anything — the result is a draft you review and test before it can be approved.",
  "SopCompiler.draftsHeading": "Compiled drafts",
  "SopCompiler.emptyDrafts": "No compiled drafts yet. Paste or import an SOP above and compile it to see a draft here.",
  "SopCompiler.statusDraft": "Draft",
  "SopCompiler.statusSentForReview": "Sent for review",
  "SopCompiler.stepsHeading": "Steps",
  "SopCompiler.noStepsExtracted": "No steps were extracted.",
  "SopCompiler.inputsHeading": "Required inputs",
  "SopCompiler.required": "required",
  "SopCompiler.optional": "optional",
  "SopCompiler.gatesHeading": "Policy / permission gates",
  "SopCompiler.testsHeading": "Acceptance / test checklist",
  "SopCompiler.expectedPrefix": "Expected:",
  "SopCompiler.evidenceHeading": "Required evidence",
  "SopCompiler.nonGoalsNote": "This compiled workflow stays inactive until you review it and send it to Skill Proposals, where it remains quarantined until explicitly approved — nothing here installs or activates it directly.",
  "SopCompiler.sendToReviewButton": "Send to Skill Proposals for review",
  "SopCompiler.alreadySentButton": "Already sent for review",
  "SopCompiler.discardButton": "Discard",
};
