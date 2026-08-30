export type DiscoveryEvidenceKind = "config" | "code" | "test" | "ci" | "documentation";

export interface RepositoryScanFile {
  path: string;
  content: string;
}

export interface DiscoveredConvention {
  id: string;
  title: string;
  body: string;
  confidence: number;
  tags: string[];
  globs: string[];
  languages: string[];
  frameworks: string[];
  taskKeywords: string[];
  evidenceKind: DiscoveryEvidenceKind;
  supportingPaths: string[];
  counterexamplePaths: string[];
}

const SOURCE_EXTENSIONS = new Set(["ts", "tsx", "js", "jsx", "rs", "py", "go", "java", "kt", "kts", "swift", "cs", "cpp", "cc", "c", "h", "hpp"]);
const DOC_EXTENSIONS = new Set(["md", "mdx"]);
const MIN_SUPPORT = 3;
const MAX_EVIDENCE_PATHS = 8;

function normalized(path: string): string {
  return path.replace(/\\/g, "/").replace(/^\.\//, "");
}

function extension(path: string): string {
  return normalized(path).split(".").pop()?.toLowerCase() ?? "";
}

function isSource(path: string): boolean {
  return SOURCE_EXTENSIONS.has(extension(path));
}

function languageFor(path: string): string | null {
  switch (extension(path)) {
    case "ts": case "tsx": return "typescript";
    case "js": case "jsx": return "javascript";
    case "rs": return "rust";
    case "py": return "python";
    case "go": return "go";
    case "java": return "java";
    case "kt": case "kts": return "kotlin";
    case "swift": return "swift";
    case "cs": return "csharp";
    case "c": case "h": return "c";
    case "cc": case "cpp": case "hpp": return "cpp";
    default: return null;
  }
}

function unique(values: string[]): string[] {
  return [...new Set(values.map(normalized))];
}

function limited(values: string[]): string[] {
  return unique(values).slice(0, MAX_EVIDENCE_PATHS);
}

function confidence(support: number, total: number, floor = 0.65): number {
  if (total <= 0) return floor;
  return Math.max(floor, Math.min(0.99, support / total));
}

function convention(
  input: Omit<DiscoveredConvention, "supportingPaths" | "counterexamplePaths" | "confidence"> & {
    supportingPaths: string[];
    counterexamplePaths?: string[];
    confidence?: number;
  },
): DiscoveredConvention | null {
  const supportingPaths = limited(input.supportingPaths);
  const counterexamplePaths = limited(input.counterexamplePaths ?? []);
  if (supportingPaths.length < MIN_SUPPORT && input.evidenceKind !== "config" && input.evidenceKind !== "documentation") return null;
  return {
    ...input,
    supportingPaths,
    counterexamplePaths,
    confidence: input.confidence ?? confidence(supportingPaths.length, supportingPaths.length + counterexamplePaths.length),
  };
}

function basenameWithoutExtension(path: string): string {
  const base = normalized(path).split("/").pop() ?? path;
  return base.replace(/\.(test|spec)?\.?[A-Za-z0-9]+$/, "").replace(/\.[A-Za-z0-9]+$/, "");
}

function namingStyle(path: string): "kebab-case" | "snake_case" | "PascalCase" | "camelCase" | "other" {
  const name = basenameWithoutExtension(path);
  if (/^[a-z0-9]+(?:-[a-z0-9]+)+$/.test(name)) return "kebab-case";
  if (/^[a-z0-9]+(?:_[a-z0-9]+)+$/.test(name)) return "snake_case";
  if (/^[A-Z][A-Za-z0-9]*$/.test(name)) return "PascalCase";
  if (/^[a-z][A-Za-z0-9]*$/.test(name)) return "camelCase";
  return "other";
}

function recurringMatches(files: RepositoryScanFile[], matcher: RegExp): string[] {
  return files.filter((file) => {
    matcher.lastIndex = 0;
    return matcher.test(file.content);
  }).map((file) => file.path);
}

function add(result: DiscoveredConvention[], candidate: DiscoveredConvention | null): void {
  if (!candidate) return;
  const existing = result.findIndex((entry) => entry.id === candidate.id);
  if (existing < 0 || result[existing].supportingPaths.length < candidate.supportingPaths.length) result.splice(Math.max(0, existing), existing < 0 ? 0 : 1, candidate);
}

function configDiscovery(files: RepositoryScanFile[], result: DiscoveredConvention[]): void {
  const byPath = new Map(files.map((file) => [normalized(file.path), file]));
  const tsconfig = byPath.get("tsconfig.json");
  if (tsconfig) add(result, convention({
    id: "compiler-typescript",
    title: "TypeScript compiler settings are repository authority",
    body: "TypeScript changes should remain compatible with the checked-in tsconfig compiler and module-boundary settings rather than weakening them locally.",
    confidence: 1,
    tags: ["typescript", "compiler", "architecture"],
    globs: ["**/*.ts", "**/*.tsx", "tsconfig*.json"],
    languages: ["typescript"], frameworks: [], taskKeywords: ["typescript", "type", "compiler", "module", "import"],
    evidenceKind: "config", supportingPaths: [tsconfig.path],
  }));
  for (const path of [".editorconfig", "commitlint.config.js", "commitlint.config.cjs", "commitlint.config.mjs"]) {
    if (!byPath.has(path)) continue;
    add(result, convention({
      id: path === ".editorconfig" ? "editorconfig-style" : "git-commit-convention",
      title: path === ".editorconfig" ? "EditorConfig defines repository text conventions" : "Commit messages follow checked-in commitlint policy",
      body: path === ".editorconfig" ? "New text files should preserve the repository's checked-in EditorConfig conventions." : "Commits created for this repository should satisfy the checked-in commit-message policy.",
      confidence: 1,
      tags: path === ".editorconfig" ? ["formatting", "editorconfig"] : ["git", "commit"],
      globs: [], languages: [], frameworks: [], taskKeywords: path === ".editorconfig" ? ["format", "file"] : ["git", "commit", "pr"],
      evidenceKind: "config", supportingPaths: [path],
    }));
  }
}

function layoutAndNamingDiscovery(source: RepositoryScanFile[], result: DiscoveredConvention[]): void {
  const sourcePaths = source.map((file) => normalized(file.path));
  const roots = new Map<string, string[]>();
  for (const path of sourcePaths) {
    const root = path.split("/")[0];
    if (!root || root.startsWith(".")) continue;
    roots.set(root, [...(roots.get(root) ?? []), path]);
  }
  const rankedRoots = [...roots.entries()].sort((a, b) => b[1].length - a[1].length);
  const dominantRoot = rankedRoots[0];
  if (dominantRoot && dominantRoot[1].length >= MIN_SUPPORT) add(result, convention({
    id: "source-directory-layout",
    title: `Source code predominantly lives under ${dominantRoot[0]}/`,
    body: `Place new source code under the repository's established ${dominantRoot[0]}/ hierarchy unless the target subsystem has a more specific existing location.`,
    tags: ["architecture", "layout", "files"], globs: [`${dominantRoot[0]}/**`], languages: [], frameworks: [], taskKeywords: ["file", "module", "component", "service", "architecture"],
    evidenceKind: "code", supportingPaths: dominantRoot[1], counterexamplePaths: rankedRoots.slice(1).flatMap(([, paths]) => paths),
  }));

  const styles = new Map<string, string[]>();
  for (const path of sourcePaths) {
    const style = namingStyle(path);
    if (style === "other") continue;
    styles.set(style, [...(styles.get(style) ?? []), path]);
  }
  const rankedStyles = [...styles.entries()].sort((a, b) => b[1].length - a[1].length);
  const winner = rankedStyles[0];
  if (winner && winner[1].length >= MIN_SUPPORT) add(result, convention({
    id: "source-file-naming",
    title: `Source files predominantly use ${winner[0]}`,
    body: `New source files should normally follow the repository's predominant ${winner[0]} naming convention, while preserving stronger local module conventions where they differ.`,
    tags: ["naming", "files"], globs: ["**/*"], languages: [], frameworks: [], taskKeywords: ["file", "module", "rename", "naming"],
    evidenceKind: "code", supportingPaths: winner[1], counterexamplePaths: rankedStyles.slice(1).flatMap(([, paths]) => paths),
  }));
}

function architectureDiscovery(source: RepositoryScanFile[], result: DiscoveredConvention[]): void {
  const paths = source.map((file) => normalized(file.path));
  const layerNames = ["components", "lib", "store", "stores", "services", "api", "domain", "adapters", "commands", "handlers", "models", "repositories"];
  for (const layer of layerNames) {
    const matching = paths.filter((path) => path.split("/").includes(layer));
    if (matching.length < MIN_SUPPORT) continue;
    add(result, convention({
      id: `architecture-layer-${layer}`,
      title: `Repository has an established ${layer} architecture layer`,
      body: `Changes whose responsibility matches ${layer} should extend the existing ${layer} layer rather than creating a competing parallel layer.`,
      tags: ["architecture", layer], globs: [`**/${layer}/**`], languages: [], frameworks: [], taskKeywords: [layer, "architecture", "module", "refactor"],
      evidenceKind: "code", supportingPaths: matching,
    }));
  }

  const aliasImports = recurringMatches(source, /(?:from\s+["']@\/|import\s+["']@\/)/m);
  const relativeImports = recurringMatches(source, /(?:from\s+["']\.\.\/|use\s+(?:crate|super)::)/m);
  if (aliasImports.length >= MIN_SUPPORT) add(result, convention({
    id: "import-boundary-alias",
    title: "Repository uses configured import aliases across modules",
    body: "Use the repository's existing import alias for cross-module imports where nearby code does; do not introduce a second alias scheme.",
    tags: ["imports", "architecture", "typescript"], globs: ["**/*.ts", "**/*.tsx"], languages: ["typescript"], frameworks: [], taskKeywords: ["import", "module", "typescript", "refactor"],
    evidenceKind: "code", supportingPaths: aliasImports, counterexamplePaths: relativeImports,
  }));

  const localImports = new Map<string, string[]>();
  const importPattern = /(?:from\s+["']([^"']+)["']|use\s+((?:crate|super)::[A-Za-z0-9_:]+))/g;
  for (const file of source) {
    for (const match of file.content.matchAll(importPattern)) {
      const imported = match[1] ?? match[2];
      if (!imported || !(imported.startsWith(".") || imported.startsWith("@/") || imported.startsWith("crate::") || imported.startsWith("super::"))) continue;
      const key = imported.replace(/\/[^/]+$/, "").replace(/::[^:]+$/, "");
      localImports.set(key, [...(localImports.get(key) ?? []), file.path]);
    }
  }
  const common = [...localImports.entries()].filter(([, users]) => unique(users).length >= MIN_SUPPORT).sort((a, b) => unique(b[1]).length - unique(a[1]).length)[0];
  if (common) add(result, convention({
    id: "common-local-api",
    title: `Repository code reuses the ${common[0]} local API boundary`,
    body: `Before adding a parallel helper/API for this responsibility, prefer extending or reusing the established ${common[0]} boundary when it fits the task.`,
    tags: ["api", "architecture", "reuse"], globs: [], languages: [], frameworks: [], taskKeywords: ["api", "helper", "service", "module", "reuse"],
    evidenceKind: "code", supportingPaths: common[1],
  }));
}

function recurringPatternDiscovery(source: RepositoryScanFile[], result: DiscoveredConvention[]): void {
  const patterns: Array<{
    id: string; title: string; body: string; matcher: RegExp; tags: string[]; task: string[]; languages?: string[];
  }> = [
    {
      id: "security-explicit-validation",
      title: "Security-sensitive paths use explicit validation or policy checks",
      body: "Security-sensitive changes should preserve explicit validation/policy checks and must not treat repository text as permission authority.",
      matcher: /\b(?:permission|allowlist|denylist|validate|sanitize|risk_level|policy|capabilit(?:y|ies))\b/i,
      tags: ["security", "permissions", "validation"], task: ["security", "permission", "network", "secret", "auth", "tool"],
    },
    {
      id: "persistence-explicit-serialization",
      title: "Persistence uses explicit repository serialization/storage paths",
      body: "Persisted state should use the repository's existing serialization/storage abstractions and preserve schema/compatibility handling rather than introducing ad-hoc state files.",
      matcher: /\b(?:serde_json|JSON\.stringify|JSON\.parse|localStorage|sqlite|sqlx|rusqlite|persist|save_impl|load_impl)\b/,
      tags: ["persistence", "storage", "serialization"], task: ["persist", "storage", "database", "state", "config"],
    },
    {
      id: "error-explicit-propagation",
      title: "Errors are explicitly propagated or contextualized",
      body: "New failure paths should follow nearby explicit error propagation/context patterns instead of swallowing failures or reporting success without evidence.",
      matcher: /(?:Result\s*<|map_err\s*\(|anyhow!|thiserror|catch\s*\(|throw\s+new\s+Error|return\s+Err\s*\()/,
      tags: ["errors", "reliability"], task: ["error", "failure", "result", "reliability"],
    },
    {
      id: "concurrency-structured-async",
      title: "Concurrent work uses repository async/concurrency primitives",
      body: "Concurrent work should compose the repository's existing async/concurrency primitives and retain cancellation/bounds rather than spawning unbounded detached work.",
      matcher: /(?:tokio::(?:spawn|select!|sync)|Arc\s*<\s*(?:Mutex|RwLock)|Promise\.all|AbortController|CancellationToken|Semaphore)/,
      tags: ["concurrency", "async", "cancellation"], task: ["async", "concurrency", "parallel", "worker", "background", "cancel"],
    },
  ];
  for (const pattern of patterns) {
    const supporting = recurringMatches(source, pattern.matcher);
    if (supporting.length < MIN_SUPPORT) continue;
    const languages = unique(supporting.map(languageFor).filter((value): value is string => Boolean(value)));
    add(result, convention({
      id: pattern.id, title: pattern.title, body: pattern.body, tags: pattern.tags,
      globs: [], languages: pattern.languages ?? languages, frameworks: [], taskKeywords: pattern.task,
      evidenceKind: "code", supportingPaths: supporting,
      confidence: Math.min(0.95, 0.7 + Math.min(0.25, supporting.length * 0.03)),
    }));
  }
}

function gitAndDocsDiscovery(files: RepositoryScanFile[], result: DiscoveredConvention[]): void {
  const normalizedFiles = files.map((file) => ({ ...file, path: normalized(file.path) }));
  const gitDocs = normalizedFiles.filter((file) => /^(CONTRIBUTING\.md|\.github\/(?:PULL_REQUEST_TEMPLATE|ISSUE_TEMPLATE)(?:\/|\.)|docs\/.*(?:git|contribut|release|pull|branch).*\.md$)/i.test(file.path));
  if (gitDocs.length > 0) add(result, convention({
    id: "git-repository-conventions",
    title: "Repository documents Git/contribution conventions",
    body: "Git delivery should follow the checked-in contribution, pull-request, branch, and release guidance when applicable.",
    confidence: gitDocs.length >= 2 ? 0.95 : 0.75,
    tags: ["git", "contributing", "delivery"], globs: [], languages: [], frameworks: [], taskKeywords: ["git", "commit", "branch", "pull request", "pr", "release"],
    evidenceKind: "documentation", supportingPaths: gitDocs.map((file) => file.path),
  }));

  const docs = normalizedFiles.filter((file) => DOC_EXTENSIONS.has(extension(file.path)) && (file.path.startsWith("docs/") || /^README(?:\.|$)/i.test(file.path)));
  if (docs.length >= MIN_SUPPORT) add(result, convention({
    id: "documentation-checked-in-docs",
    title: "Repository keeps substantial checked-in documentation",
    body: "User-visible or architectural behavior changes should update the relevant checked-in documentation rather than leaving docs knowingly stale.",
    tags: ["documentation", "architecture"], globs: ["docs/**", "README*"], languages: [], frameworks: [], taskKeywords: ["docs", "documentation", "architecture", "feature", "behavior"],
    evidenceKind: "documentation", supportingPaths: docs.map((file) => file.path),
    confidence: Math.min(0.98, 0.75 + docs.length * 0.02),
  }));
}

/**
 * Deterministic structural discovery used after manifest/config discovery.
 * It deliberately requires repeated evidence for code-derived conventions;
 * one incidental source line never becomes a high-confidence standard.
 */
export function analyzeRepositoryConventions(input: RepositoryScanFile[]): DiscoveredConvention[] {
  const files = input.map((file) => ({ path: normalized(file.path), content: file.content }));
  const source = files.filter((file) => isSource(file.path));
  const result: DiscoveredConvention[] = [];
  configDiscovery(files, result);
  layoutAndNamingDiscovery(source, result);
  architectureDiscovery(source, result);
  recurringPatternDiscovery(source, result);
  gitAndDocsDiscovery(files, result);
  return result.sort((a, b) => a.title.localeCompare(b.title));
}
