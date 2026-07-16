/**
 * Trust Scorecards (ROADMAP.md Phase 7, item 28) — English source strings,
 * spread into every locale's dictionary below with a REAL translation per
 * locale (see `de.ts`/`fr.ts`/etc.), matching this app's normal per-locale
 * i18n convention rather than the shared-English-only shortcut a few older
 * feature areas (`runs.ts`, `crew.ts`, …) still use.
 */
export const trustScorecardsLocale: Record<string, string> = {
  "AppMenu.trustScorecards": "Trust Scorecards",
  "TrustScorecards.title": "Trust Scorecards",
  "TrustScorecards.subtitle": "Compare quality, cost, privacy, security, reliability, and provenance across every model, connector, MCP server, skill, workflow, and plugin — with the exact evidence each score was derived from.",
  "TrustScorecards.close": "Close Trust Scorecards",
  "TrustScorecards.refresh": "Refresh scorecards",
  "TrustScorecards.loading": "Scoring entities…",
  "TrustScorecards.empty": "Nothing to score yet — connect a model, connector, MCP server, skill, workflow, or plugin first.",
  "TrustScorecards.filterAll": "All",
  "TrustScorecards.searchPlaceholder": "Search by name…",
  "TrustScorecards.columnName": "Name",
  "TrustScorecards.columnKind": "Kind",
  "TrustScorecards.columnDimension": "Dimension",
  "TrustScorecards.expandEvidence": "Show evidence",
  "TrustScorecards.selectForCompare": "Select {{name}} to compare",
  "TrustScorecards.compareButton": "Compare {{count}}",
  "TrustScorecards.compareTitle": "Comparing {{count}} trust profiles",
  "TrustScorecards.kind.model": "Model",
  "TrustScorecards.kind.connector": "Connector",
  "TrustScorecards.kind.mcp_server": "MCP server",
  "TrustScorecards.kind.skill": "Skill",
  "TrustScorecards.kind.workflow": "Workflow",
  "TrustScorecards.kind.plugin": "Plugin",
  "TrustScorecards.dimension.quality": "Quality",
  "TrustScorecards.dimension.cost": "Cost",
  "TrustScorecards.dimension.privacy": "Privacy",
  "TrustScorecards.dimension.security": "Security",
  "TrustScorecards.dimension.reliability": "Reliability",
  "TrustScorecards.dimension.provenance": "Provenance",
  "TrustScorecards.level.good": "Good",
  "TrustScorecards.level.fair": "Fair",
  "TrustScorecards.level.poor": "Poor",
  "TrustScorecards.level.unknown": "Insufficient evidence",
};
