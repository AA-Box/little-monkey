/**
 * Agent-Ready Spec Scorer (ROADMAP.md Phase 7, item 4) — English source
 * strings, spread into every locale's dictionary below with a REAL
 * translation per locale (see `de.ts`/`fr.ts`/etc.), matching this app's
 * normal per-locale i18n convention (see `issueToPr.ts`'s doc comment for
 * the same convention on the panel this feature extends).
 */
export const specScorerLocale: Record<string, string> = {
  "SpecScorer.scoringLabel": "Checking how agent-ready this issue is…",
  "SpecScorer.bannerHeading": "This issue may be too vague for an autonomous run",
  "SpecScorer.bannerIntro": "Agent-readiness score: {{score}}/100 — {{summary}}",
  "SpecScorer.missingInfoHeading": "Answer these before starting an autonomous run:",
  "SpecScorer.dimensionsHeading": "Score breakdown",
  "SpecScorer.dimension.clarity": "Clarity",
  "SpecScorer.dimension.scope": "Scope",
  "SpecScorer.dimension.missingContext": "Missing context",
  "SpecScorer.dimension.testability": "Testability",
  "SpecScorer.dimension.dependencies": "Dependencies",
  "SpecScorer.dimension.agentReadiness": "Agent readiness",
  "SpecScorer.readyNote": "Agent-readiness check passed ({{score}}/100).",
  "SpecScorer.advisoryNote": "Advisory only — you can still start a run regardless.",
  "SpecScorer.rescoreButton": "Re-check",
  "SpecScorer.errorNote": "Could not check this issue's agent-readiness right now.",
};
