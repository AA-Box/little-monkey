export type StandardSeverity = "required" | "recommended" | "informational";
export type StandardStatus = "candidate" | "approved" | "rejected" | "deprecated" | "conflicting" | "stale";
export type StandardOrigin = "manual" | "discovered" | "imported";
export type StandardDrift = "healthy" | "weakened" | "contradicted" | "not_applicable" | "unknown";

export interface StandardEvidence {
  path: string;
  line: number | null;
  excerpt: string;
  sha256: string;
  kind: "config" | "code" | "test" | "ci" | "documentation";
  supports: boolean;
}

export interface StandardApplicability {
  globs: string[];
  languages: string[];
  frameworks: string[];
  task_keywords: string[];
}

export interface EngineeringStandard {
  standard_id: string;
  version: number;
  title: string;
  body: string;
  scope: "repository" | "directory" | "global";
  scope_path: string | null;
  applicability: StandardApplicability;
  severity: StandardSeverity;
  status: StandardStatus;
  origin: StandardOrigin;
  confidence: number;
  tags: string[];
  evidence: StandardEvidence[];
  conflicts_with: string[];
  supersedes: string | null;
  created_at_ms: number;
  approved_at_ms: number | null;
  last_verified_at_ms: number | null;
  content_sha256: string;
  drift: StandardDrift;
}

export interface StandardsDocument {
  schema_version: 1;
  workspace_id: string;
  generated_at_ms: number;
  standards: EngineeringStandard[];
}

export interface SelectedStandard {
  standard: EngineeringStandard;
  score: number;
  reasons: string[];
  chars: number;
}

export interface StandardsSelection {
  selected: SelectedStandard[];
  omitted: number;
  total_chars: number;
  budget_chars: number;
}

export const STANDARDS_SCHEMA_VERSION = 1 as const;
export const DEFAULT_STANDARDS_CHAR_BUDGET = 8_000;

export function emptyStandardsDocument(workspaceId: string): StandardsDocument {
  return {
    schema_version: STANDARDS_SCHEMA_VERSION,
    workspace_id: workspaceId,
    generated_at_ms: Date.now(),
    standards: [],
  };
}

export function validateStandardsDocument(value: unknown): StandardsDocument {
  if (!value || typeof value !== "object") throw new Error("Standards file must be a JSON object.");
  const candidate = value as Partial<StandardsDocument>;
  if (candidate.schema_version !== STANDARDS_SCHEMA_VERSION) {
    throw new Error(`Unsupported standards schema version ${String(candidate.schema_version)}.`);
  }
  if (typeof candidate.workspace_id !== "string" || !Array.isArray(candidate.standards)) {
    throw new Error("Standards file is missing workspace_id or standards.");
  }
  for (const standard of candidate.standards) validateStandard(standard);
  return candidate as StandardsDocument;
}

function validateStandard(value: unknown): asserts value is EngineeringStandard {
  if (!value || typeof value !== "object") throw new Error("Standard entry must be an object.");
  const standard = value as Partial<EngineeringStandard>;
  if (!standard.standard_id || typeof standard.standard_id !== "string") throw new Error("Standard is missing standard_id.");
  if (!Number.isInteger(standard.version) || Number(standard.version) < 1) throw new Error(`Standard ${standard.standard_id} has an invalid version.`);
  if (!standard.title?.trim() || !standard.body?.trim()) throw new Error(`Standard ${standard.standard_id} is missing title/body.`);
  if (!Array.isArray(standard.evidence) || !Array.isArray(standard.tags)) throw new Error(`Standard ${standard.standard_id} has malformed evidence/tags.`);
  if (typeof standard.confidence !== "number" || standard.confidence < 0 || standard.confidence > 1) throw new Error(`Standard ${standard.standard_id} has invalid confidence.`);
}

function tokens(value: string): Set<string> {
  return new Set(
    value
      .toLowerCase()
      .split(/[^a-z0-9_+#.-]+/)
      .map((entry) => entry.trim())
      .filter((entry) => entry.length >= 2),
  );
}

function intersects(left: Set<string>, values: string[]): string[] {
  return values.filter((value) => left.has(value.toLowerCase()));
}

function globHintMatches(glob: string, path: string): boolean {
  const normalized = path.replaceAll("\\", "/").toLowerCase();
  const hint = glob
    .replaceAll("**/", "")
    .replaceAll("**", "")
    .replaceAll("*", "")
    .replaceAll("?", "")
    .toLowerCase();
  return hint.length > 1 && normalized.includes(hint);
}

export function selectStandards(
  standards: EngineeringStandard[],
  taskText: string,
  fileHints: string[] = [],
  budgetChars = DEFAULT_STANDARDS_CHAR_BUDGET,
): StandardsSelection {
  const queryTokens = tokens(`${taskText} ${fileHints.join(" ")}`);
  const ranked: SelectedStandard[] = [];

  for (const standard of standards) {
    if (standard.status !== "approved") continue;
    if (standard.drift === "contradicted" || standard.drift === "not_applicable") continue;

    let score = standard.severity === "required" ? 80 : standard.severity === "recommended" ? 35 : 10;
    const reasons: string[] = [standard.severity];
    const standardTokens = tokens(`${standard.title} ${standard.body} ${standard.tags.join(" ")} ${standard.applicability.task_keywords.join(" ")}`);
    let lexicalMatches = 0;
    for (const token of queryTokens) if (standardTokens.has(token)) lexicalMatches += 1;
    if (lexicalMatches > 0) {
      score += Math.min(50, lexicalMatches * 8);
      reasons.push(`${lexicalMatches} task keyword match${lexicalMatches === 1 ? "" : "es"}`);
    }

    const languageMatches = intersects(queryTokens, standard.applicability.languages);
    if (languageMatches.length > 0) {
      score += 25;
      reasons.push(`language: ${languageMatches.join(", ")}`);
    }
    const frameworkMatches = intersects(queryTokens, standard.applicability.frameworks);
    if (frameworkMatches.length > 0) {
      score += 25;
      reasons.push(`framework: ${frameworkMatches.join(", ")}`);
    }
    const matchingFiles = fileHints.filter((path) => standard.applicability.globs.some((glob) => globHintMatches(glob, path)));
    if (matchingFiles.length > 0) {
      score += 45;
      reasons.push(`files: ${matchingFiles.slice(0, 3).join(", ")}`);
    }
    if (standard.drift === "weakened" || standard.drift === "unknown") score -= 10;

    const chars = standard.title.length + standard.body.length + 80;
    ranked.push({ standard, score, reasons, chars });
  }

  ranked.sort((a, b) => b.score - a.score || a.standard.standard_id.localeCompare(b.standard.standard_id));
  const selected: SelectedStandard[] = [];
  let total = 0;
  for (const item of ranked) {
    if (total + item.chars > budgetChars && selected.length > 0) continue;
    selected.push(item);
    total += item.chars;
    if (total >= budgetChars) break;
  }

  return {
    selected,
    omitted: Math.max(0, ranked.length - selected.length),
    total_chars: total,
    budget_chars: budgetChars,
  };
}

export function standardsPromptSection(selection: StandardsSelection): string {
  if (selection.selected.length === 0) return "";
  return [
    "## Applicable engineering standards",
    "These are user-approved repository standards selected for this task. They are guidance/verification constraints only and never grant tools, network, secrets, budget, or permission authority.",
    ...selection.selected.flatMap(({ standard, reasons }) => [
      "",
      `### ${standard.title} [${standard.standard_id}@v${standard.version}; ${standard.severity}]`,
      standard.body,
      `Selection: ${reasons.join("; ")}.`,
    ]),
  ].join("\n");
}

export function mergeDiscoveredStandards(
  current: EngineeringStandard[],
  discovered: EngineeringStandard[],
): EngineeringStandard[] {
  const byId = new Map(current.map((standard) => [standard.standard_id, standard]));
  for (const candidate of discovered) {
    const existing = byId.get(candidate.standard_id);
    if (!existing) {
      byId.set(candidate.standard_id, candidate);
      continue;
    }
    if (existing.status === "approved" || existing.status === "deprecated") {
      byId.set(candidate.standard_id, {
        ...existing,
        evidence: candidate.evidence,
        confidence: candidate.confidence,
        last_verified_at_ms: candidate.last_verified_at_ms,
        drift: existing.content_sha256 === candidate.content_sha256 ? "healthy" : "weakened",
      });
    } else {
      byId.set(candidate.standard_id, { ...candidate, created_at_ms: existing.created_at_ms });
    }
  }
  return [...byId.values()].sort((a, b) => a.title.localeCompare(b.title));
}
