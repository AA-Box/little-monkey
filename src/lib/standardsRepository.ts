import { exists, mkdir, readDir, readTextFile, writeTextFile } from "@tauri-apps/plugin-fs";
import {
  emptyStandardsDocument,
  mergeDiscoveredStandards,
  validateStandardsDocument,
  type EngineeringStandard,
  type StandardEvidence,
  type StandardsDocument,
} from "./standards";

const MAX_SCAN_FILES = 300;
const MAX_SCAN_DEPTH = 4;
const MAX_EVIDENCE_BYTES = 256 * 1024;
const STANDARD_DIR = ".little-monkey/standards";
const STANDARD_FILE = "index.json";

function join(root: string, child: string): string {
  const separator = root.includes("\\") && !root.includes("/") ? "\\" : "/";
  return `${root.replace(/[\\/]$/, "")}${separator}${child.replaceAll("/", separator)}`;
}

export function standardsFilePath(workspacePath: string): string {
  return join(workspacePath, `${STANDARD_DIR}/${STANDARD_FILE}`);
}

async function sha256(text: string): Promise<string> {
  const bytes = new TextEncoder().encode(text);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function lineOf(content: string, needle: string): number | null {
  const index = content.indexOf(needle);
  if (index < 0) return null;
  return content.slice(0, index).split("\n").length;
}

async function readEvidence(
  workspacePath: string,
  relativePath: string,
  needle: string,
  kind: StandardEvidence["kind"],
  supports = true,
): Promise<StandardEvidence | null> {
  const path = join(workspacePath, relativePath);
  try {
    if (!(await exists(path))) return null;
    const content = await readTextFile(path);
    if (new TextEncoder().encode(content).byteLength > MAX_EVIDENCE_BYTES) return null;
    const line = lineOf(content, needle);
    if (needle && line === null) return null;
    const lines = content.split(/\r?\n/);
    const excerpt = line ? lines[Math.max(0, line - 1)]?.trim().slice(0, 500) ?? needle : content.slice(0, 500).trim();
    return {
      path: relativePath.replaceAll("\\", "/"),
      line,
      excerpt,
      sha256: await sha256(content),
      kind,
      supports,
    };
  } catch {
    return null;
  }
}

async function contentDigest(title: string, body: string, evidence: StandardEvidence[]): Promise<string> {
  return sha256(JSON.stringify({ title, body, evidence: evidence.map(({ path, sha256, supports }) => ({ path, sha256, supports })) }));
}

async function standard(
  id: string,
  title: string,
  body: string,
  evidence: StandardEvidence[],
  options: Partial<Pick<EngineeringStandard, "severity" | "confidence" | "tags" | "applicability">> = {},
): Promise<EngineeringStandard> {
  const now = Date.now();
  return {
    standard_id: id,
    version: 1,
    title,
    body,
    scope: "repository",
    scope_path: null,
    applicability: options.applicability ?? { globs: [], languages: [], frameworks: [], task_keywords: [] },
    severity: options.severity ?? "recommended",
    status: "candidate",
    origin: "discovered",
    confidence: options.confidence ?? 0.9,
    tags: options.tags ?? [],
    evidence,
    conflicts_with: [],
    supersedes: null,
    created_at_ms: now,
    approved_at_ms: null,
    last_verified_at_ms: now,
    content_sha256: await contentDigest(title, body, evidence),
    drift: "healthy",
  };
}

async function packageJsonStandards(workspacePath: string): Promise<EngineeringStandard[]> {
  const path = join(workspacePath, "package.json");
  if (!(await exists(path))) return [];
  let raw = "";
  let pkg: Record<string, unknown>;
  try {
    raw = await readTextFile(path);
    pkg = JSON.parse(raw) as Record<string, unknown>;
  } catch {
    return [];
  }
  const dependencies = {
    ...(typeof pkg.dependencies === "object" && pkg.dependencies ? pkg.dependencies as Record<string, string> : {}),
    ...(typeof pkg.devDependencies === "object" && pkg.devDependencies ? pkg.devDependencies as Record<string, string> : {}),
  };
  const scripts = typeof pkg.scripts === "object" && pkg.scripts ? pkg.scripts as Record<string, string> : {};
  const result: EngineeringStandard[] = [];

  for (const [framework, label] of [["react", "React"], ["vue", "Vue"], ["svelte", "Svelte"], ["next", "Next.js"]] as const) {
    if (!dependencies[framework]) continue;
    const evidence = await readEvidence(workspacePath, "package.json", `\"${framework}\"`, "config");
    if (evidence) result.push(await standard(
      `framework-${framework}`,
      `${label} is a repository framework`,
      `Changes to UI code should follow the repository's existing ${label} patterns instead of introducing a competing UI framework.`,
      [evidence],
      { confidence: 1, tags: ["framework", framework, "ui"], applicability: { globs: ["src/**"], languages: ["typescript", "javascript"], frameworks: [framework], task_keywords: ["ui", "component", framework] } },
    ));
  }

  const testCandidates = [["vitest", "Vitest", "vitest"], ["jest", "Jest", "jest"], ["@playwright/test", "Playwright", "playwright"]] as const;
  for (const [dependency, label, keyword] of testCandidates) {
    if (!dependencies[dependency]) continue;
    const evidence = await readEvidence(workspacePath, "package.json", `\"${dependency}\"`, "config");
    if (evidence) result.push(await standard(
      `testing-${keyword}`,
      `Use ${label} for matching tests`,
      `When adding or updating tests in the areas covered by the existing test setup, use ${label} and preserve nearby test conventions.`,
      [evidence],
      { severity: "recommended", confidence: 1, tags: ["testing", keyword], applicability: { globs: ["**/*.test.*", "**/*.spec.*"], languages: ["typescript", "javascript"], frameworks: [keyword], task_keywords: ["test", "tests", "spec", keyword] } },
    ));
  }

  for (const [scriptName, command] of Object.entries(scripts)) {
    if (!/^(test|lint|typecheck|check|build)$/.test(scriptName) || typeof command !== "string") continue;
    const evidence = await readEvidence(workspacePath, "package.json", `\"${scriptName}\"`, "config");
    if (evidence) result.push(await standard(
      `verification-npm-${scriptName}`,
      `Repository defines ${scriptName} verification`,
      `For changes that can affect this check, run the repository's \`${scriptName}\` script (\`${command}\`) or the equivalent configured verification before claiming completion.`,
      [evidence],
      { severity: scriptName === "test" ? "required" : "recommended", confidence: 1, tags: ["verification", scriptName], applicability: { globs: [], languages: ["typescript", "javascript"], frameworks: [], task_keywords: [scriptName, "verify", "verification"] } },
    ));
  }
  return result;
}

async function configStandards(workspacePath: string): Promise<EngineeringStandard[]> {
  const definitions: Array<{
    file: string;
    needle: string;
    id: string;
    title: string;
    body: string;
    tags: string[];
    languages: string[];
  }> = [
    { file: "rustfmt.toml", needle: "", id: "format-rustfmt", title: "Rust formatting is repository-configured", body: "Rust changes should remain compatible with the checked-in rustfmt configuration and be formatted before completion.", tags: ["rust", "formatting"], languages: ["rust"] },
    { file: ".rustfmt.toml", needle: "", id: "format-rustfmt", title: "Rust formatting is repository-configured", body: "Rust changes should remain compatible with the checked-in rustfmt configuration and be formatted before completion.", tags: ["rust", "formatting"], languages: ["rust"] },
    { file: "biome.json", needle: "", id: "format-biome", title: "Biome owns JavaScript/TypeScript formatting or linting", body: "JavaScript/TypeScript changes should preserve the checked-in Biome rules instead of introducing an independent formatter/linter policy.", tags: ["biome", "formatting", "lint"], languages: ["typescript", "javascript"] },
    { file: ".prettierrc", needle: "", id: "format-prettier", title: "Prettier formatting is repository-configured", body: "JavaScript/TypeScript changes should remain compatible with the repository's Prettier configuration.", tags: ["prettier", "formatting"], languages: ["typescript", "javascript"] },
    { file: "eslint.config.js", needle: "", id: "lint-eslint", title: "ESLint rules are repository-configured", body: "JavaScript/TypeScript changes should satisfy the checked-in ESLint configuration and avoid adding local exceptions without evidence they are necessary.", tags: ["eslint", "lint"], languages: ["typescript", "javascript"] },
    { file: "eslint.config.mjs", needle: "", id: "lint-eslint", title: "ESLint rules are repository-configured", body: "JavaScript/TypeScript changes should satisfy the checked-in ESLint configuration and avoid adding local exceptions without evidence they are necessary.", tags: ["eslint", "lint"], languages: ["typescript", "javascript"] },
    { file: "pyproject.toml", needle: "[tool.ruff", id: "lint-ruff", title: "Ruff rules are repository-configured", body: "Python changes should satisfy the repository's Ruff configuration.", tags: ["python", "ruff", "lint"], languages: ["python"] },
    { file: "pyproject.toml", needle: "[tool.pytest", id: "testing-pytest", title: "Pytest is repository-configured", body: "Python tests should use the repository's Pytest configuration and nearby fixture conventions.", tags: ["python", "pytest", "testing"], languages: ["python"] },
  ];
  const result: EngineeringStandard[] = [];
  const seen = new Set<string>();
  for (const definition of definitions) {
    if (seen.has(definition.id)) continue;
    const evidence = await readEvidence(workspacePath, definition.file, definition.needle, "config");
    if (!evidence) continue;
    seen.add(definition.id);
    result.push(await standard(definition.id, definition.title, definition.body, [evidence], {
      confidence: 1,
      tags: definition.tags,
      applicability: { globs: [], languages: definition.languages, frameworks: [], task_keywords: definition.tags },
    }));
  }

  const cargo = await readEvidence(workspacePath, "Cargo.toml", "[package]", "config");
  if (cargo) result.push(await standard(
    "rust-cargo-workflow",
    "Rust code follows Cargo project conventions",
    "Rust changes should preserve the existing Cargo workspace/package structure and use the repository's Cargo-based build/test workflow.",
    [cargo],
    { confidence: 1, tags: ["rust", "cargo"], applicability: { globs: ["**/*.rs", "Cargo.toml"], languages: ["rust"], frameworks: ["cargo"], task_keywords: ["rust", "cargo"] } },
  ));
  return result;
}

async function ciStandards(workspacePath: string): Promise<EngineeringStandard[]> {
  const workflowDir = join(workspacePath, ".github/workflows");
  if (!(await exists(workflowDir))) return [];
  let entries: Awaited<ReturnType<typeof readDir>> = [];
  try { entries = await readDir(workflowDir); } catch { return []; }
  const evidence: StandardEvidence[] = [];
  for (const entry of entries.slice(0, 30)) {
    if (!entry.isFile || !/\.ya?ml$/i.test(entry.name)) continue;
    const item = await readEvidence(workspacePath, `.github/workflows/${entry.name}`, "", "ci");
    if (item) evidence.push(item);
  }
  if (evidence.length === 0) return [];
  return [await standard(
    "ci-checked-in-workflows",
    "Preserve checked-in CI expectations",
    "Changes should remain compatible with the repository's checked-in CI workflows; when a relevant workflow command can be run locally, use it or an equivalent configured verification before completion.",
    evidence,
    { severity: "recommended", confidence: 1, tags: ["ci", "verification"], applicability: { globs: [".github/workflows/**"], languages: [], frameworks: [], task_keywords: ["ci", "build", "test", "release", "workflow"] } },
  )];
}

async function collectFiles(root: string, relative = "", depth = 0, output: string[] = []): Promise<string[]> {
  if (depth > MAX_SCAN_DEPTH || output.length >= MAX_SCAN_FILES) return output;
  const absolute = relative ? join(root, relative) : root;
  let entries: Awaited<ReturnType<typeof readDir>>;
  try { entries = await readDir(absolute); } catch { return output; }
  for (const entry of entries) {
    if (output.length >= MAX_SCAN_FILES) break;
    if ([".git", "node_modules", "target", "dist", "build", ".next", ".venv"].includes(entry.name)) continue;
    const child = relative ? `${relative}/${entry.name}` : entry.name;
    if (entry.isDirectory) await collectFiles(root, child, depth + 1, output);
    else if (entry.isFile) output.push(child);
  }
  return output;
}

async function conventionStandards(workspacePath: string): Promise<EngineeringStandard[]> {
  const files = await collectFiles(workspacePath);
  const tests = files.filter((path) => /(^|\/)(__tests__\/|[^/]+\.(test|spec)\.(ts|tsx|js|jsx|rs|py)$)/.test(path));
  if (tests.length < 3) return [];
  const styleCounts = new Map<string, string[]>();
  for (const test of tests) {
    const style = test.includes("/__tests__/") ? "__tests__ directory" : test.includes(".test.") ? ".test file suffix" : test.includes(".spec.") ? ".spec file suffix" : "language-native test naming";
    styleCounts.set(style, [...(styleCounts.get(style) ?? []), test]);
  }
  const ranked = [...styleCounts.entries()].sort((a, b) => b[1].length - a[1].length);
  const [winner, matching] = ranked[0] ?? [];
  if (!winner || !matching || matching.length < 3) return [];
  const counter = ranked.slice(1).flatMap(([, paths]) => paths).slice(0, 5);
  const evidence: StandardEvidence[] = [];
  for (const path of matching.slice(0, 5)) {
    const item = await readEvidence(workspacePath, path, "", "test", true);
    if (item) evidence.push(item);
  }
  for (const path of counter) {
    const item = await readEvidence(workspacePath, path, "", "test", false);
    if (item) evidence.push(item);
  }
  const confidence = matching.length / tests.length;
  return [await standard(
    "testing-file-layout",
    `Existing tests predominantly use ${winner}`,
    `New tests should normally follow the repository's predominant ${winner} convention unless the target module clearly uses a different local convention.`,
    evidence,
    { confidence, tags: ["testing", "layout"], applicability: { globs: ["**/*.test.*", "**/*.spec.*", "**/__tests__/**"], languages: [], frameworks: [], task_keywords: ["test", "tests", "spec"] } },
  )];
}

export async function discoverStandards(workspacePath: string): Promise<EngineeringStandard[]> {
  const groups = await Promise.all([
    packageJsonStandards(workspacePath),
    configStandards(workspacePath),
    ciStandards(workspacePath),
    conventionStandards(workspacePath),
  ]);
  const byId = new Map<string, EngineeringStandard>();
  for (const standard of groups.flat()) {
    const existing = byId.get(standard.standard_id);
    if (!existing || existing.evidence.length < standard.evidence.length) byId.set(standard.standard_id, standard);
  }
  return [...byId.values()].sort((a, b) => a.title.localeCompare(b.title));
}

export async function loadStandards(workspacePath: string): Promise<StandardsDocument> {
  const path = standardsFilePath(workspacePath);
  if (!(await exists(path))) return emptyStandardsDocument(workspacePath);
  const raw = await readTextFile(path);
  return validateStandardsDocument(JSON.parse(raw));
}

export async function saveStandards(workspacePath: string, document: StandardsDocument): Promise<void> {
  const directory = join(workspacePath, STANDARD_DIR);
  if (!(await exists(directory))) await mkdir(directory, { recursive: true });
  const next = { ...document, generated_at_ms: Date.now(), workspace_id: workspacePath } satisfies StandardsDocument;
  await writeTextFile(standardsFilePath(workspacePath), `${JSON.stringify(next, null, 2)}\n`);
}

export async function discoverAndMergeStandards(workspacePath: string): Promise<StandardsDocument> {
  const current = await loadStandards(workspacePath);
  const discovered = await discoverStandards(workspacePath);
  const next = { ...current, standards: mergeDiscoveredStandards(current.standards, discovered), generated_at_ms: Date.now() };
  await saveStandards(workspacePath, next);
  return next;
}

export async function checkStandardsDrift(workspacePath: string, document: StandardsDocument): Promise<StandardsDocument> {
  const now = Date.now();
  const standards: EngineeringStandard[] = [];
  for (const standard of document.standards) {
    if (standard.status !== "approved" || standard.evidence.length === 0) {
      standards.push(standard);
      continue;
    }
    let supporting = 0;
    let changed = 0;
    let missing = 0;
    for (const evidence of standard.evidence.filter((item) => item.supports)) {
      const path = join(workspacePath, evidence.path);
      if (!(await exists(path))) { missing += 1; continue; }
      try {
        const current = await readTextFile(path);
        if ((await sha256(current)) === evidence.sha256) supporting += 1;
        else changed += 1;
      } catch { missing += 1; }
    }
    const drift = supporting > 0 && changed === 0 && missing === 0
      ? "healthy"
      : supporting === 0 && (changed > 0 || missing > 0)
        ? "contradicted"
        : "weakened";
    standards.push({ ...standard, drift, status: drift === "contradicted" ? "stale" : standard.status, last_verified_at_ms: now });
  }
  const next = { ...document, standards, generated_at_ms: now };
  await saveStandards(workspacePath, next);
  return next;
}

export async function approveStandard(workspacePath: string, document: StandardsDocument, standardId: string): Promise<StandardsDocument> {
  const now = Date.now();
  const standards = document.standards.map((standard) => standard.standard_id === standardId
    ? { ...standard, status: "approved" as const, version: standard.status === "approved" ? standard.version : standard.version + 1, approved_at_ms: now, last_verified_at_ms: now, drift: "healthy" as const }
    : standard);
  const next = { ...document, standards, generated_at_ms: now };
  await saveStandards(workspacePath, next);
  return next;
}

export async function setStandardStatus(workspacePath: string, document: StandardsDocument, standardId: string, status: EngineeringStandard["status"]): Promise<StandardsDocument> {
  const standards = document.standards.map((standard) => standard.standard_id === standardId ? { ...standard, status } : standard);
  const next = { ...document, standards, generated_at_ms: Date.now() };
  await saveStandards(workspacePath, next);
  return next;
}

export async function importStandards(workspacePath: string, sourcePath: string): Promise<StandardsDocument> {
  const incoming = validateStandardsDocument(JSON.parse(await readTextFile(sourcePath)));
  const current = await loadStandards(workspacePath);
  const byId = new Map(current.standards.map((standard) => [standard.standard_id, standard]));
  for (const standard of incoming.standards) {
    byId.set(standard.standard_id, { ...standard, origin: "imported", workspace_id: undefined } as EngineeringStandard);
  }
  const next = { ...current, standards: [...byId.values()] };
  await saveStandards(workspacePath, next);
  return next;
}

export async function exportStandards(document: StandardsDocument, targetPath: string): Promise<void> {
  await writeTextFile(targetPath, `${JSON.stringify(document, null, 2)}\n`);
}
