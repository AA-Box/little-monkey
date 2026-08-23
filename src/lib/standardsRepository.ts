import { invoke } from "@tauri-apps/api/core";

import {
  emptyStandardsDocument,
  mergeDiscoveredStandards,
  snapshotStandardRevision,
  validateStandardsDocument,
  type EngineeringStandard,
  type PendingStandardRevision,
  type StandardEvidence,
  type StandardsDocument,
} from "./standards";

const MAX_SCAN_FILES = 300;
const MAX_SCAN_DEPTH = 4;
const MAX_EVIDENCE_BYTES = 256 * 1024;
const MAX_AGENT_OS_FILES = 100;
const STANDARD_FILE = ".little-monkey/standards/index.json";
const EXPORT_FILE = ".little-monkey/standards/export.json";
const AGENT_OS_INDEX_CANDIDATES = ["agent-os/standards/index.yml", ".agent-os/standards/index.yml"] as const;
const AGENT_OS_EXPORT_DIR = ".little-monkey/standards/agent-os-export";
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

async function contentDigest(title: string, body: string, applicability: EngineeringStandard["applicability"], severity: EngineeringStandard["severity"], tags: string[]): Promise<string> {
  return sha256(JSON.stringify({ title, body, applicability, severity, tags }));
}

async function standard(
  id: string,
  title: string,
  body: string,
  evidence: StandardEvidence[],
  options: Partial<Pick<EngineeringStandard, "severity" | "confidence" | "tags" | "applicability" | "origin">> = {},
): Promise<EngineeringStandard> {
  const now = Date.now();
  const applicability = options.applicability ?? { globs: [], languages: [], frameworks: [], task_keywords: [] };
  const severity = options.severity ?? "recommended";
  const tags = options.tags ?? [];
  return {
    standard_id: id,
    version: 1,
    title,
    body,
    scope: "repository",
    scope_path: null,
    applicability,
    severity,
    status: "candidate",
    origin: options.origin ?? "discovered",
    confidence: options.confidence ?? 0.9,
    tags,
    evidence,
    conflicts_with: [],
    supersedes: null,
    created_at_ms: now,
    approved_at_ms: null,
    last_verified_at_ms: now,
    content_sha256: await contentDigest(title, body, applicability, severity, tags),
    drift: "healthy",
    revision_history: [],
    pending_revision: null,
    checker_command_ids: [],
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

/** Extract Markdown paths from Agent OS's standards index without interpreting
 * YAML as code or requiring a YAML runtime. The adapter intentionally accepts
 * only repository-relative `.md` references and then reads each through the
 * normal workspace sandbox. */
function agentOsIndexMarkdownPaths(index: string): string[] {
  const paths = new Set<string>();
  const matcher = /(?:^|[\s\[,{:-])['\"]?([A-Za-z0-9_./-]+\.md)['\"]?(?=$|[\s\]},#])/gm;
  for (const match of index.matchAll(matcher)) {
    const raw = match[1].replace(/\\/g, "/");
    if (raw.startsWith("/") || raw.includes("..") || /^[A-Za-z]:/.test(raw)) continue;
    paths.add(raw);
    if (paths.size >= MAX_AGENT_OS_FILES) break;
  }
  return [...paths];
}

function markdownTitle(markdown: string, fallback: string): string {
  const heading = markdown.match(/^#\s+(.+)$/m)?.[1]?.trim();
  return heading || fallback.replace(/\.md$/i, "").split("/").at(-1)?.replace(/[-_]+/g, " ") || "Imported standard";
}

function normalizeAgentOsPath(indexPath: string, referencedPath: string): string {
  if (referencedPath.startsWith("agent-os/") || referencedPath.startsWith(".agent-os/")) return referencedPath;
  const root = indexPath.slice(0, indexPath.lastIndexOf("/") + 1);
  return `${root}${referencedPath}`.replace(/\/\.\//g, "/");
}

export async function importAgentOsStandards(workspacePath: string): Promise<StandardsDocument> {
  let indexPath: string | null = null;
  let rawIndex: string | null = null;
  for (const candidate of AGENT_OS_INDEX_CANDIDATES) {
    const raw = await readWorkspaceText(candidate);
    if (raw) { indexPath = candidate; rawIndex = raw; break; }
  }
  if (!indexPath || !rawIndex) {
    throw new Error(`No Agent OS standards index found (${AGENT_OS_INDEX_CANDIDATES.join(" or ")}).`);
  }

  const indexEvidence = await readEvidence(indexPath, "", "documentation");
  const imported: EngineeringStandard[] = [];
  for (const referenced of agentOsIndexMarkdownPaths(rawIndex)) {
    const path = normalizeAgentOsPath(indexPath, referenced);
    const markdown = await readWorkspaceText(path);
    if (!markdown || new TextEncoder().encode(markdown).byteLength > MAX_EVIDENCE_BYTES) continue;
    const evidence = await readEvidence(path, "", "documentation");
    if (!evidence) continue;
    const normalizedId = `agent-os-${path.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "")}`;
    imported.push(await standard(
      normalizedId,
      markdownTitle(markdown, path),
      markdown.trim(),
      indexEvidence ? [indexEvidence, evidence] : [evidence],
      { origin: "imported", confidence: 1, tags: ["agent-os", "imported"], applicability: { globs: [], languages: [], frameworks: [], task_keywords: [] } },
    ));
  }
  if (imported.length === 0) throw new Error(`Agent OS index ${indexPath} did not reference readable Markdown standards.`);

  const current = await loadStandards(workspacePath);
  const next = { ...current, standards: mergeDiscoveredStandards(current.standards, imported), generated_at_ms: Date.now() };
  await saveStandards(workspacePath, next);
  return next;
}

function safeExportName(standard: EngineeringStandard): string {
  return `${standard.standard_id.toLowerCase().replace(/[^a-z0-9._-]+/g, "-")}.md`;
}

export async function exportAgentOsStandards(document: StandardsDocument): Promise<string> {
  const approved = document.standards.filter((standard) => standard.status === "approved");
  if (approved.length === 0) throw new Error("Approve at least one standard before exporting to Agent OS.");
  const files = approved.map((standard) => safeExportName(standard));
  const index = ["# Generated by Little Monkey Standards Studio", "standards:", ...files.map((file) => `  - ${file}`), ""].join("\n");
  await writeWorkspaceText(`${AGENT_OS_EXPORT_DIR}/index.yml`, index);
  for (let i = 0; i < approved.length; i += 1) {
    const standard = approved[i];
    const markdown = [
      `# ${standard.title}`,
      "",
      standard.body,
      "",
      `<!-- little-monkey-standard: ${standard.standard_id}@v${standard.version} sha256:${standard.content_sha256} -->`,
      "",
    ].join("\n");
    await writeWorkspaceText(`${AGENT_OS_EXPORT_DIR}/${files[i]}`, markdown);
  }
  return `${AGENT_OS_EXPORT_DIR}/index.yml`;
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

function activatePendingRevision(standard: EngineeringStandard, pending: PendingStandardRevision, now: number): EngineeringStandard {
  return {
    ...standard,
    version: pending.version,
    title: pending.title,
    body: pending.body,
    applicability: structuredClone(pending.applicability),
    severity: pending.severity,
    tags: [...pending.tags],
    evidence: pending.evidence.map((entry) => ({ ...entry })),
    content_sha256: pending.content_sha256,
    origin: pending.source,
    status: "approved",
    approved_at_ms: now,
    last_verified_at_ms: now,
    drift: "healthy",
    revision_history: [...standard.revision_history, snapshotStandardRevision(standard, "approved_revision", now)],
    pending_revision: null,
  };
}

export async function approveStandard(workspacePath: string, document: StandardsDocument, standardId: string): Promise<StandardsDocument> {
  const now = Date.now();
  const standards = document.standards.map((standard) => {
    if (standard.standard_id !== standardId) return standard;
    if (standard.pending_revision) return activatePendingRevision(standard, standard.pending_revision, now);
    return { ...standard, status: "approved" as const, approved_at_ms: now, last_verified_at_ms: now, drift: "healthy" as const };
  });
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

export async function setStandardCheckers(workspacePath: string, document: StandardsDocument, standardId: string, commandIds: string[]): Promise<StandardsDocument> {
  const normalized = [...new Set(commandIds.map((id) => id.trim()).filter(Boolean))];
  const standards = document.standards.map((standard) => standard.standard_id === standardId
    ? { ...standard, checker_command_ids: normalized }
    : standard);
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
  for (const imported of incoming.standards) {
    const existing = byId.get(imported.standard_id);
    if (!existing) {
      byId.set(imported.standard_id, { ...imported, origin: "imported" });
      continue;
    }
    if (existing.content_sha256 === imported.content_sha256) {
      byId.set(imported.standard_id, { ...imported, origin: "imported", checker_command_ids: existing.checker_command_ids });
      continue;
    }
    if (existing.status === "approved") {
      const now = Date.now();
      byId.set(imported.standard_id, {
        ...existing,
        drift: "weakened",
        pending_revision: {
          version: existing.version + 1,
          title: imported.title,
          body: imported.body,
          applicability: structuredClone(imported.applicability),
          severity: imported.severity,
          tags: [...imported.tags],
          evidence: imported.evidence.map((entry) => ({ ...entry })),
          content_sha256: imported.content_sha256,
          recorded_at_ms: now,
          proposed_at_ms: now,
          source: "imported",
        },
      });
    } else {
      byId.set(imported.standard_id, {
        ...imported,
        origin: "imported",
        version: existing.version + 1,
        revision_history: [...existing.revision_history, snapshotStandardRevision(existing, "imported_revision")],
        checker_command_ids: existing.checker_command_ids,
      });
    }
  }
  const next = { ...current, standards: [...byId.values()], generated_at_ms: Date.now() };
  await saveStandards(workspacePath, next);
  return next;
}

export async function exportStandards(document: StandardsDocument, targetPath = EXPORT_FILE): Promise<string> {
  await writeWorkspaceText(targetPath, `${JSON.stringify(document, null, 2)}\n`);
  return targetPath;
}
