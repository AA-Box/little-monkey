/**
 * Knowledge Graph Explorer (ROADMAP.md Phase 7, item 10) — English source
 * strings, spread into every locale's dictionary below with a REAL
 * translation per locale (see `de.ts`/`fr.ts`/etc.), matching this app's
 * per-locale i18n convention.
 */
export const knowledgeGraphExplorerLocale: Record<string, string> = {
  "AppMenu.knowledgeGraphExplorer": "Knowledge Graph Explorer",
  "KnowledgeGraphExplorer.title": "Knowledge Graph Explorer",
  "KnowledgeGraphExplorer.subtitle": "Builds an entity/relationship graph from your knowledge stacks and the current chat, then answers \"how is X related to Y?\" with the source evidence behind it.",
  "KnowledgeGraphExplorer.close": "Close Knowledge Graph Explorer",
  "KnowledgeGraphExplorer.sourcesLabel": "Sources:",
  "KnowledgeGraphExplorer.noStacks": "No knowledge stacks yet — add one in Settings, or build from the current chat below.",
  "KnowledgeGraphExplorer.includeSession": "Include current chat ({{title}})",
  "KnowledgeGraphExplorer.buildButton": "Build graph",
  "KnowledgeGraphExplorer.partialBuildHeading": "Some sources could not be included in this build:",
  "KnowledgeGraphExplorer.queryLabel": "Ask a relationship question",
  "KnowledgeGraphExplorer.queryPlaceholder": "How is Alice related to auth.ts?",
  "KnowledgeGraphExplorer.askButton": "Ask",
  "KnowledgeGraphExplorer.emptyGraph": "No graph yet — pick sources above and click \"Build graph\".",
  "KnowledgeGraphExplorer.evidenceHeading": "Evidence",
  "KnowledgeGraphExplorer.evidenceEmpty": "Ask a relationship question above to see the path and its source evidence here.",
  "KnowledgeGraphExplorer.noEvidence": "No evidence spans were recorded for this path.",
};
