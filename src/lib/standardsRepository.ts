import { invoke } from "@tauri-apps/api/core";

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
const STANDARD_FILE = ".little-monkey/standards/index.json";
const EXPORT_FILE = ".little-monkey/standards/export.json";
const SKIP_DIRS = new Set([".git", "node_modules", "target", "dist", "build", ".next", ".venv"]);

interface WorkspaceDirEntry {
  name: string;
  is_dir: boolean;
  size: number;
}

/** Human-readable absolute path only; actual IO always goes through the
 * backend's sandboxed workspace commands using STANDARD_FILE. */
export function standardsFilePath(workspacePath: string): string {
  const separator = workspacePath.includes("\\") && !workspacePath.includes("/") ? "\\" : "/";
  return `${workspacePath.replace(/[\\/]$/, "")}${separator}${STANDARD_FILE.replace(/\//g, separator)}`;
}

async function readWorkspaceText(relativePath: string): Promise<string | null> {
  try {
    return await invoke<string>("tool_read_file", {
      path: relativePath,
      workspace_root_override: null,
    });
  } catch {
    return null;
  }
}

async function listWorkspaceDir(relativePath: string): Promise<WorkspaceDirEntry[]> {
  try {
    return await invoke<WorkspaceDirEntry[]>("tool_list_dir", {
      path: relativePath || ".",
      workspace_root_override: null,
    });
  } catch {
    return [];
  }
}

/** Persist through the exact same path sandbox + permission gate agents use.
 * A Standards Studio mutation is therefore never able to widen renderer fs
 * scope or escape the attached workspace. */
async function writeWorkspaceText(relativePath: string, content: string): Promise<void> {
  await invoke<string>("tool_write_file", {
    path: relativePath,
    content,
    checkpoint_id: null,
    turn_id: null,
    tool_call_id: `standards-studio:${crypto.randomUUID()}`,
    risk_level: "low",
    risk_reason: "User-initiated Standards Studio configuration write inside the active workspace",
    agent_label: "Standards Studio",
    workspace_root_override: null,
  });
}

async function sha256(text: string): Promise<string> {
  const bytes = new TextEncoder().encode(text);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function lineOf(content: string, needle: string): number | null {
  if (!needle) return null;
  const index = content.indexOf(needle);
  return index < 0 ? null : content.slice(0, index).split("\n").length;
}

async function readEvidence(
  relativePath: string,
  needle: string,
  kind: StandardEvidence["kind"],
  supports = true,
): Promise<StandardEvidence | null> {
  const content = await readWorkspaceText(relativePath);
  if (content === null) return null;
  if (new TextEncoder().encode(content).byteLength > MAX_EVIDENCE_BYTES) return null;
  const line = lineOf(content, needle);
  if (needle && line === null) return null;
  const lines = content.split(/\r?\n/);
  const excerpt = line
    ? lines[Math.max(0, line - 1)]?.trim().slice(0, 500) ?? needle
    : content.slice(0, 500).trim();
  return {
    path: relativePath.replace(/\\/g, "/"),
    line,
    excerpt,
    sha256: await sha256(content),
    kind,
    supports,
  };
}

async function contentDigest(title: string, body: string, evidence: StandardEvidence[]): Promise<string> {
  return sha256(JSON.stringify({
    title,
    body,
    evidence: evidence.map(({ path, sha256: digest, supports }) => ({ path, sha256: digest, supports })),
  }));
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

async function packageJsonStandards(): Promise<EngineeringStandard[]> {
  const raw = await readWorkspaceText("package.json");
  if (!raw) return [];
  let pkg: Record<string, unknown>;
  try { pkg = JSON.parse(raw) as Record<string, unknown>; } catch { return []; }
  const dependencies = {
    ...(typeof pkg.dependencies === "object" && pkg.dependencies ? pkg.dependencies as Record<string, string> : {}),
    ...(typeof pkg.devDependencies === "object" && pkg.devDependencies ? pkg.devDependencies as Record<string, string> : {}),
  };
  const scripts = typeof pkg.scripts === "object" && pkg.scripts ? pkg.scripts as Record<string, string> : {};
  const result: EngineeringStandard[] = [];

  for (const [framework, label] of [["react", "React"], ["vue", "Vue"], ["svelte", "Svelte"], ["next", "Next.js"]] as const) {
    if (!dependencies[framework]) continue;
    const evidence = await readEvidence("package.json", `\"${framework}\"`, "config");
    if (!evidence) continue;
    result.push(await standard(
      `framework-${framework}`,
      `${label} is a repository framework`,
      `Changes to UI code should follow the repository's existing ${label} patterns instead of introducing a competing UI framework.`,
      [evidence],
      { confidence: 1, tags: ["framework", framework, "ui"], applicability: { globs: ["src/**"], languages: ["typescript", "javascript"], frameworks: [framework], task_keywords: ["ui", "component", framework] } },
    ));
  }

  for (const [dependency, label, keyword] of [["vitest", "Vitest", "vitest"], ["jest", "Jest", "jest"], ["@playwright/test", "Playwright", "playwright"]] as const) {
    if (!dependencies[dependency]) continue;
    const evidence = await readEvidence("package.json", `\"${dependency}\"`, "config");
    if (!evidence) continue;
    result.push(await standard(
      `testing-${keyword}`,
      `Use ${label} for matching tests`,
      `When adding or updating tests in the areas covered by the existing test setup, use ${label} and preserve nearby test conventions.`,
      [evidence],
      { confidence: 1, tags: ["testing", keyword], applicability: { globs: ["**/*.test.*", "**/*.spec.*"], languages: ["typescript", "javascript"], frameworks: [keyword], task_keywords: ["test", "tests", "spec", keyword] } },
    ));
  }

  for (const [scriptName, command] of Object.entries(scripts)) {
    if (!/^(test|lint|typecheck|check|build)$/.test(scriptName) || typeof command !== "string") continue;
    const evidence = await readEvidence("package.json", `\"${scriptName}\"`, "config");
    if (!evidence) continue;
    result.push(await standard(
      `verification-npm-${scriptName}`,
      `Repository defines ${scriptName} verification`,
      `For changes that can affect this check, run the repository's \`${scriptName}\` script (\`${command}\`) or the equivalent configured verification before claiming completion.`,
      [evidence],
      { severity: scriptName === "test" ? "required" : "recommended", confidence: 1, tags: ["verification", scriptName], applicability: { globs: [], languages: ["typescript", "javascript"], frameworks: [], task_keywords: [scriptName, "verify", "verification"] } },
    ));
  }
  return result;
}

async function configStandards(): Promise<EngineeringStandard[]> {
  const definitions = [
    ["rustfmt.toml", "", "format-rustfmt", "Rust formatting is repository-configured", "Rust changes should remain compatible with the checked-in rustfmt configuration and be formatted before completion.", ["rust", "formatting"], ["rust"]],
    [".rustfmt.toml", "", "format-rustfmt", "Rust formatting is repository-configured", "Rust changes should remain compatible with the checked-in rustfmt configuration and be formatted before completion.", ["rust", "formatting"], ["rust"]],
    ["biome.json", "", "format-biome", "Biome owns JavaScript/TypeScript formatting or linting", "JavaScript/TypeScript changes should preserve the checked-in Biome rules instead of introducing an independent formatter/linter policy.", ["biome", "formatting", "lint"], ["typescript", "javascript"]],
    [".prettierrc", "", "format-prettier", "Prettier formatting is repository-configured", "JavaScript/TypeScript changes should remain compatible with the repository's Prettier configuration.", ["prettier", "formatting"], ["typescript", "javascript"]],
    ["eslint.config.js", "", "lint-eslint", "ESLint rules are repository-configured", "JavaScript/TypeScript changes should satisfy the checked-in ESLint configuration and avoid adding local exceptions without evidence they are necessary.", ["eslint", "lint"], ["typescript", "javascript"]],
    ["eslint.config.mjs", "", "lint-eslint", "ESLint rules are repository-configured", "JavaScript/TypeScript changes should satisfy the checked-in ESLint configuration and avoid adding local exceptions without evidence they are necessary.", ["eslint", "lint"], ["typescript", "javascript"]],
    ["pyproject.toml", "[tool.ruff", "lint-ruff", "Ruff rules are repository-configured", "Python changes should satisfy the repository's Ruff configuration.", ["python", "ruff", "lint"], ["python"]],
    ["pyproject.toml", "[tool.pytest", "testing-pytest", "Pytest is repository-configured", "Python tests should use the repository's Pytest configuration and nearby fixture conventions.", ["python", "pytest", "testing"], ["python"]],
  ] as const;
  const result: EngineeringStandard[] = [];
  const seen = new Set<string>();
  for (const [file, needle, id, title, body, tags, languages] of definitions) {
    if (seen.has(id)) continue;
    const evidence = await readEvidence(file, needle, "config");
    if (!evidence) continue;
    seen.add(id);
    result.push(await standard(id, title, body, [evidence], {
      confidence: 1,
      tags: [...tags],
      applicability: { globs: [], languages: [...languages], frameworks: [], task_keywords: [...tags] },
    }));
  }
  const cargo = await readEvidence("Cargo.toml", "[package]", "config");
  if (cargo) result.push(await standard(
    "rust-cargo-workflow",
    "Rust code follows Cargo project conventions",
    "Rust changes should preserve the existing Cargo workspace/package structure and use the repository's Cargo-based build/test workflow.",
    [cargo],
    { confidence: 1, tags: ["rust", "cargo"], applicability: { globs: ["**/*.rs", "Cargo.toml"], languages: ["rust"], frameworks: ["cargo"], task_keywords: ["rust", "cargo"] } },
  ));
  return result;
}

async function ciStandards(): Promise<EngineeringStandard[]> {
  const entries = await listWorkspaceDir(".github/workflows");
  const evidence: StandardEvidence[] = [];
  for (const entry of entries.slice(0, 30)) {
    if (entry.is_dir || !/\.ya?ml$/i.test(entry.name)) continue;
    const item = await readEvidence(`.github/workflows/${entry.name}`, "", "ci");
    if (item) evidence.push(item);
  }
  if (evidence.length === 0) return [];
  return [await standard(
    "ci-checked-in-workflows",
    "Preserve checked-in CI expectations",
    "Changes should remain compatible with the repository's checked-in CI workflows; when a relevant workflow command can be run locally, use it or an equivalent configured verification before completion.",
    evidence,
    { confidence: 1, tags: ["ci", "verification"], applicability: { globs: [".github/workflows/**"], languages: [], frameworks: [], task_keywords: ["ci", "build", "test", "release", "workflow"] } },
  )];
}

async function collectFiles(relative = ".", depth = 0, output: string[] = []): Promise<string[]> {
  if (depth > MAX_SCAN_DEPTH || output.length >= MAX_SCAN_FILES) return output;
  for (const entry of await listWorkspaceDir(relative)) {
    if (output.length >= MAX_SCAN_FILES) break;
    if (SKIP_DIRS.has(entry.name)) continue;
    const child = relative === "." ? entry.name : `${relative}/${entry.name}`;
    if (entry.is_dir) await collectFiles(child, depth + 1, output);
    else output.push(child);
  }
  return output;
}

async function conventionStandards(): Promise<EngineeringStandard[]> {
  const files = await collectFiles();
  const tests = files.filter((path) => /(^|\/)(__tests__\/|[^/]+\.(test|spec)\.(ts|tsx|js|jsx|rs|py)$)/.test(path));
  if (tests.length < 3) return [];
  const styleCounts = new Map<string, string[]>();
  for (const test of tests) {
    const style = test.includes("/__tests__/") ? "__tests__ directory" : test.includes(".test.") ? ".test file suffix" : test.includes(".spec.") ? ".spec file suffix" : "language-native test naming";
    styleCounts.set(style, [...(styleCounts.get(style) ?? []), test]);
  }
  const ranked = [...styleCounts.entries()].sort((a, b) => b[1].length - a[1].length);
  const winner = ranked[0];
  if (!winner || winner[1].length < 3) return [];
  const evidence: StandardEvidence[] = [];
  for (const path of winner[1].slice(0, 5)) {
    const item = await readEvidence(path, "", "test", true);
    if (item) evidence.push(item);
  }
  for (const path of ranked.slice(1).flatMap(([, paths]) => paths).slice(0, 5)) {
    const item = await readEvidence(path, "", "test", false);
    if (item) evidence.push(item);
  }
  return [await standard(
    "testing-file-layout",
    `Existing tests predominantly use ${winner[0]}`,
    `New tests should normally follow the repository's predominant ${winner[0]} convention unless the target module clearly uses a different local convention.`,
    evidence,
    { confidence: winner[1].length / tests.length, tags: ["testing", "layout"], applicability: { globs: ["**/*.test.*", "**/*.spec.*", "**/__tests__/**"], languages: [], frameworks: [], task_keywords: ["test", "tests", "spec"] } },
  )];
}

export async function discoverStandards(_workspacePath: string): Promise<EngineeringStandard[]> {
  const groups = await Promise.all([packageJsonStandards(), configStandards(), ciStandards(), conventionStandards()]);
  const byId = new Map<string, EngineeringStandard>();
  for (const candidate of groups.flat()) {
    const existing = byId.get(candidate.standard_id);
    if (!existing || existing.evidence.length < candidate.evidence.length) byId.set(candidate.standard_id, candidate);
  }
  return [...byId.values()].sort((a, b) => a.title.localeCompare(b.title));
}

export async function loadStandards(workspacePath: string): Promise<StandardsDocument> {
  const raw = await readWorkspaceText(STANDARD_FILE);
  if (!raw) return emptyStandardsDocument(workspacePath);
  return validateStandardsDocument(JSON.parse(raw));
}

export async function saveStandards(_workspacePath: string, document: StandardsDocument): Promise<void> {
  const next = { ...document, generated_at_ms: Date.now() } satisfies StandardsDocument;
  await writeWorkspaceText(STANDARD_FILE, `${JSON.stringify(next, null, 2)}\n`);
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
    if (standard.status !== "approved" || standard.evidence.length === 0) { standards.push(standard); continue; }
    let supporting = 0;
    let changed = 0;
    let missing = 0;
    for (const evidence of standard.evidence.filter((item) => item.supports)) {
      const current = await readWorkspaceText(evidence.path);
      if (current === null) { missing += 1; continue; }
      if ((await sha256(current)) === evidence.sha256) supporting += 1;
      else changed += 1;
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
    ? { ...standard, status: "approved" as const, approved_at_ms: now, last_verified_at_ms: now, drift: "healthy" as const }
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

/** Import/export intentionally stay inside the attached workspace. This keeps
 * portability real without granting the renderer broad HOME filesystem scope.
 * Users can copy the JSON elsewhere with their normal file manager. */
export async function importStandards(workspacePath: string, sourcePath: string): Promise<StandardsDocument> {
  const raw = await readWorkspaceText(sourcePath);
  if (!raw) throw new Error(`Could not read ${sourcePath} from the active workspace.`);
  const incoming = validateStandardsDocument(JSON.parse(raw));
  const current = await loadStandards(workspacePath);
  const byId = new Map(current.standards.map((standard) => [standard.standard_id, standard]));
  for (const standard of incoming.standards) byId.set(standard.standard_id, { ...standard, origin: "imported" });
  const next = { ...current, standards: [...byId.values()], generated_at_ms: Date.now() };
  await saveStandards(workspacePath, next);
  return next;
}

export async function exportStandards(_document: StandardsDocument, _targetPath?: string): Promise<string> {
  const document = await loadStandards("workspace");
  await writeWorkspaceText(EXPORT_FILE, `${JSON.stringify(document, null, 2)}\n`);
  return EXPORT_FILE;
}
