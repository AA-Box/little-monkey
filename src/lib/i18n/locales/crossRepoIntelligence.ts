/**
 * Cross-Repo Code Intelligence (ROADMAP.md Phase 7) — English source of
 * truth for the `CrossRepoIntelligencePanel.*` / `AppMenu.crossRepoIntelligence`
 * key namespace. Copied and spread into every other locale file, then every
 * key below is overridden there with a real translation (see de.ts/fr.ts/
 * etc.) — mirrors the structure every other feature locale slice in this
 * directory uses.
 */
export const crossRepoIntelligenceLocale: Record<string, string> = {
  "AppMenu.crossRepoIntelligence": "Cross-Repo Intelligence",
  "CrossRepoIntelligencePanel.title": "Cross-Repo Code Intelligence",
  "CrossRepoIntelligencePanel.subtitle":
    "Search symbols across every attached repo, then trace impact — affected files, owners, tests, and migration steps.",
  "CrossRepoIntelligencePanel.close": "Close Cross-Repo Intelligence",
  "CrossRepoIntelligencePanel.rebuild": "Rebuild index",
  "CrossRepoIntelligencePanel.rebuilding": "Indexing…",
  "CrossRepoIntelligencePanel.builtAt": "Indexed {{time}} · {{symbolCount}} symbols across {{fileCount}} files",
  "CrossRepoIntelligencePanel.notBuiltYet": "Build the index to search symbols across your attached repos.",
  "CrossRepoIntelligencePanel.noWorkspace": "Open a workspace folder first.",
  "CrossRepoIntelligencePanel.buildError": "Could not build the index: {{error}}",
  "CrossRepoIntelligencePanel.searchLabel": "Search symbols",
  "CrossRepoIntelligencePanel.searchPlaceholder": "Symbol name (function, class, type…)",
  "CrossRepoIntelligencePanel.noMatches": "No symbols match \"{{query}}\".",
  "CrossRepoIntelligencePanel.matchesHint": "{{count}} matching symbol(s)",
  "CrossRepoIntelligencePanel.kind.function": "function",
  "CrossRepoIntelligencePanel.kind.method": "method",
  "CrossRepoIntelligencePanel.kind.class": "class",
  "CrossRepoIntelligencePanel.kind.interface": "interface",
  "CrossRepoIntelligencePanel.kind.type": "type",
  "CrossRepoIntelligencePanel.kind.const": "const",
  "CrossRepoIntelligencePanel.kind.enum": "enum",
  "CrossRepoIntelligencePanel.kind.struct": "struct",
  "CrossRepoIntelligencePanel.kind.trait": "trait",
  "CrossRepoIntelligencePanel.impact.title": "Impact of \"{{symbol}}\"",
  "CrossRepoIntelligencePanel.impact.loading": "Tracing impact…",
  "CrossRepoIntelligencePanel.impact.error": "Could not trace impact: {{error}}",
  "CrossRepoIntelligencePanel.impact.affectedRepos": "Affected repos",
  "CrossRepoIntelligencePanel.impact.affectedFiles": "Affected files",
  "CrossRepoIntelligencePanel.impact.definitions": "Definitions",
  "CrossRepoIntelligencePanel.impact.references": "References",
  "CrossRepoIntelligencePanel.impact.noReferences": "No references found.",
  "CrossRepoIntelligencePanel.impact.tests": "Likely tests",
  "CrossRepoIntelligencePanel.impact.noTests": "No matching test file found by naming convention.",
  "CrossRepoIntelligencePanel.impact.owners": "Owners",
  "CrossRepoIntelligencePanel.impact.unassigned": "Unassigned",
  "CrossRepoIntelligencePanel.impact.migrationSteps": "Likely migration steps",
  "CrossRepoIntelligencePanel.impact.clear": "Clear",
  "CrossRepoIntelligencePanel.footnote":
    "MVP scope: indexes the attached workspace roots (primary + secondary) using regex-based symbol extraction and text search — not a full multi-language AST/call-graph.",
};
