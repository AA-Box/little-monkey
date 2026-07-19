import { invoke } from "@tauri-apps/api/core";

import type { WorkspaceRootInfo } from "../store/workspaceStore";

/**
 * Cross-Repo Code Intelligence (ROADMAP.md Phase 7): index symbols across the
 * app's own multi-root workspace (primary root + any attached secondary
 * roots, exactly how this app already represents "multiple repos on disk" —
 * see `workspaceStore.ts`), then answer "impact" queries — affected
 * repos/files/owners/tests/likely migration steps for a given symbol name.
 *
 * MVP scope, explicitly narrowed (see ROADMAP.md's own note on this item):
 * - A lightweight regex/text index, NOT a full multi-language AST or call
 *   graph. `extractSymbols` below matches common top-level export patterns
 *   per language (function/class/interface/type/const/enum for
 *   TS/JS, def/class for Python, pub fn/struct/enum/trait for Rust,
 *   func/type for Go) — it does not resolve imports, generics, overloads, or
 *   re-exports, and it only looks at the extensions in `INDEXED_EXTENSIONS`.
 * - "Who references this symbol" is a plain word-boundary text search
 *   (`tool_grep`), not semantic/type-aware reference resolution — it will
 *   include comments, strings, and unrelated identically-named symbols.
 * - "Owners" comes from a real (but simple) CODEOWNERS parse: last matching
 *   pattern wins, à la GitHub's own semantics, but pattern matching here only
 *   supports `*`, `**`, and directory-prefix patterns — not the full
 *   gitignore-style grammar. Repos with no CODEOWNERS file simply report no
 *   owner ("unassigned"), which is expected for this app's own repo today.
 * - "Likely tests" is a naming-convention guess (`foo.ts` -> `foo.test.ts` /
 *   `foo.spec.ts` / `__tests__/foo.test.ts`, `foo.py` -> `test_foo.py` /
 *   `foo_test.py`, etc.), cross-checked against the file list this same index
 *   already gathered — not a build-system/test-runner integration.
 * Follow-ups (not in this MVP): full AST/call-graph indexing, cross-repo
 * import resolution, incremental/background re-indexing, and richer
 * CODEOWNERS glob support.
 */

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type SymbolKind =
  | "function"
  | "method"
  | "class"
  | "interface"
  | "type"
  | "const"
  | "enum"
  | "struct"
  | "trait";

export interface CrossRepoFileRef {
  /** Display path exactly as returned by `tool_glob`/`tool_grep` — prefixed
   * with `"<label>/"` for a secondary root, plain-relative for the primary
   * root (mirrors `workspace::resolve_path_and_root`'s own convention). */
  file: string;
  rootId: string;
  rootLabel: string;
}

export interface CrossRepoSymbol extends CrossRepoFileRef {
  name: string;
  kind: SymbolKind;
  line: number;
}

export interface ReferenceHit extends CrossRepoFileRef {
  line: number;
  text: string;
}

export interface CodeownersRule {
  pattern: string;
  owners: string[];
}

export interface FileOwners {
  file: string;
  owners: string[];
}

export interface ImpactResult {
  symbolName: string;
  definitions: CrossRepoSymbol[];
  references: ReferenceHit[];
  /** Unique root labels among every definition/reference — the "affected
   * repos" acceptance criterion. */
  affectedRoots: string[];
  /** Unique display paths among every definition/reference. */
  affectedFiles: string[];
  testMatches: CrossRepoFileRef[];
  owners: FileOwners[];
  migrationSteps: string[];
}

// ---------------------------------------------------------------------------
// Path helpers (no Node `path` module — these run in the renderer too)
// ---------------------------------------------------------------------------

function splitPath(file: string): { dir: string; base: string; ext: string; stem: string } {
  const slash = file.lastIndexOf("/");
  const dir = slash >= 0 ? file.slice(0, slash) : "";
  const base = slash >= 0 ? file.slice(slash + 1) : file;
  const dot = base.lastIndexOf(".");
  const ext = dot > 0 ? base.slice(dot) : "";
  const stem = dot > 0 ? base.slice(0, dot) : base;
  return { dir, base, ext, stem };
}

/** Strips a `"<label>/"` secondary-root prefix off a display path, leaving a
 * path relative to that root's own top level — the shape CODEOWNERS patterns
 * are written against. Primary-root paths have no prefix to strip. */
function relativeToRoot(file: string, rootLabel: string, isPrimary: boolean): string {
  if (isPrimary) return file;
  const prefix = `${rootLabel}/`;
  return file.startsWith(prefix) ? file.slice(prefix.length) : file;
}

// ---------------------------------------------------------------------------
// Symbol extraction (pure, regex-based — see doc comment above for scope)
// ---------------------------------------------------------------------------

interface ExtractedSymbol {
  name: string;
  kind: SymbolKind;
  line: number;
}

const TS_JS_EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx"]);

const TS_JS_PATTERNS: Array<{ regex: RegExp; kind: SymbolKind }> = [
  { regex: /^export\s+(?:default\s+)?(?:async\s+)?function\s*\*?\s+([A-Za-z_$][A-Za-z0-9_$]*)/, kind: "function" },
  { regex: /^export\s+(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)/, kind: "class" },
  { regex: /^export\s+interface\s+([A-Za-z_$][A-Za-z0-9_$]*)/, kind: "interface" },
  { regex: /^export\s+type\s+([A-Za-z_$][A-Za-z0-9_$]*)/, kind: "type" },
  { regex: /^export\s+enum\s+([A-Za-z_$][A-Za-z0-9_$]*)/, kind: "enum" },
  { regex: /^export\s+const\s+([A-Za-z_$][A-Za-z0-9_$]*)/, kind: "const" },
  { regex: /^export\s+(?:async\s+)?function\s*\*?\s+([A-Za-z_$][A-Za-z0-9_$]*)/, kind: "function" },
];

const PY_PATTERNS: Array<{ regex: RegExp; kind: SymbolKind }> = [
  { regex: /^def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(/, kind: "function" },
  { regex: /^class\s+([A-Za-z_][A-Za-z0-9_]*)/, kind: "class" },
];

const RUST_PATTERNS: Array<{ regex: RegExp; kind: SymbolKind }> = [
  { regex: /^pub(?:\([^)]*\))?\s+(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)/, kind: "function" },
  { regex: /^pub(?:\([^)]*\))?\s+struct\s+([A-Za-z_][A-Za-z0-9_]*)/, kind: "struct" },
  { regex: /^pub(?:\([^)]*\))?\s+enum\s+([A-Za-z_][A-Za-z0-9_]*)/, kind: "enum" },
  { regex: /^pub(?:\([^)]*\))?\s+trait\s+([A-Za-z_][A-Za-z0-9_]*)/, kind: "trait" },
  { regex: /^pub(?:\([^)]*\))?\s+type\s+([A-Za-z_][A-Za-z0-9_]*)/, kind: "type" },
];

const GO_PATTERNS: Array<{ regex: RegExp; kind: SymbolKind }> = [
  { regex: /^func\s+\([^)]*\)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(/, kind: "method" },
  { regex: /^func\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(/, kind: "function" },
  { regex: /^type\s+([A-Za-z_][A-Za-z0-9_]*)\s+struct\b/, kind: "struct" },
  { regex: /^type\s+([A-Za-z_][A-Za-z0-9_]*)\s+interface\b/, kind: "interface" },
];

function patternsForExt(ext: string): Array<{ regex: RegExp; kind: SymbolKind }> | null {
  if (TS_JS_EXTENSIONS.has(ext)) return TS_JS_PATTERNS;
  if (ext === ".py") return PY_PATTERNS;
  if (ext === ".rs") return RUST_PATTERNS;
  if (ext === ".go") return GO_PATTERNS;
  return null;
}

/** Extracts top-level exported symbol declarations from one file's source
 * text. Only looks at each line's leading (unindented, in most languages)
 * declaration keyword — nested/local declarations are intentionally skipped,
 * matching the "index exported surface" scope of this MVP. */
export function extractSymbols(content: string, filePath: string): ExtractedSymbol[] {
  const { ext } = splitPath(filePath);
  const patterns = patternsForExt(ext);
  if (!patterns) return [];

  const found: ExtractedSymbol[] = [];
  const lines = content.split("\n");
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    for (const { regex, kind } of patterns) {
      const match = regex.exec(line);
      if (match) {
        found.push({ name: match[1], kind, line: i + 1 });
        break;
      }
    }
  }
  return found;
}

// ---------------------------------------------------------------------------
// Test-file naming heuristic
// ---------------------------------------------------------------------------

const TEST_FILE_BASENAME = /\.(test|spec)\.[jt]sx?$/;
const PY_TEST_BASENAME = /^test_.*\.py$|^.*_test\.py$/;
const GO_RS_TEST_BASENAME = /_test\.(go|rs)$/;

export function isLikelyTestFile(file: string): boolean {
  const { base } = splitPath(file);
  return TEST_FILE_BASENAME.test(base) || PY_TEST_BASENAME.test(base) || GO_RS_TEST_BASENAME.test(base);
}

/** Candidate test-file display paths for a given source file, by naming
 * convention only (existence is checked by the caller against the already-
 * gathered file list — see `buildImpact`). */
export function guessTestCandidates(ref: CrossRepoFileRef): CrossRepoFileRef[] {
  const { dir, ext, stem } = splitPath(ref.file);
  const join = (name: string) => (dir ? `${dir}/${name}` : name);
  const candidates: string[] = [];

  if (TS_JS_EXTENSIONS.has(ext)) {
    candidates.push(join(`${stem}.test${ext}`), join(`${stem}.spec${ext}`), join(`__tests__/${stem}.test${ext}`));
  } else if (ext === ".py") {
    candidates.push(join(`test_${stem}.py`), join(`${stem}_test.py`));
  } else if (ext === ".go") {
    candidates.push(join(`${stem}_test.go`));
  } else if (ext === ".rs") {
    candidates.push(join(`${stem}_test.rs`), join(`tests/${stem}.rs`));
  }

  return candidates
    .filter((file) => file !== ref.file)
    .map((file) => ({ file, rootId: ref.rootId, rootLabel: ref.rootLabel }));
}

// ---------------------------------------------------------------------------
// CODEOWNERS (real but simple parse/match — see scope note above)
// ---------------------------------------------------------------------------

export function parseCodeowners(content: string): CodeownersRule[] {
  const rules: CodeownersRule[] = [];
  for (const rawLine of content.split("\n")) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const parts = line.split(/\s+/).filter(Boolean);
    if (parts.length === 0) continue;
    const [pattern, ...owners] = parts;
    rules.push({ pattern, owners });
  }
  return rules;
}

/** Gitignore/CODEOWNERS-style pattern match: a pattern containing no `/`
 * (other than a trailing one) matches at any depth, not just at the root -
 * e.g. `*.ts` matches both `foo.ts` and `src/lib/foo.ts`. A pattern that
 * starts with `/` or contains an inner `/` is anchored to the root instead. */
function patternMatches(pattern: string, relPath: string): boolean {
  if (pattern === "*") return true;
  let p = pattern;
  const isDir = p.endsWith("/");
  if (isDir) p = p.slice(0, -1);
  const rooted = p.startsWith("/");
  if (rooted) p = p.slice(1);
  const matchesAnyDepth = !rooted && !p.includes("/");

  let escaped = "";
  for (let i = 0; i < p.length; i += 1) {
    const ch = p[i];
    if (ch === "*") {
      if (p[i + 1] === "*") {
        escaped += ".*";
        i += 1;
      } else {
        escaped += "[^/]*";
      }
    } else if (".+^${}()|[]\\".includes(ch)) {
      escaped += `\\${ch}`;
    } else {
      escaped += ch;
    }
  }

  const prefix = matchesAnyDepth ? "(^|.*/)" : "^";
  const suffix = isDir ? "(/.*)?$" : "$";
  return new RegExp(`${prefix}${escaped}${suffix}`).test(relPath);
}

/** Last matching rule wins — mirrors GitHub's own CODEOWNERS precedence. */
export function matchCodeowners(rules: readonly CodeownersRule[], relPath: string): string[] {
  let matched: string[] = [];
  for (const rule of rules) {
    if (patternMatches(rule.pattern, relPath)) matched = rule.owners;
  }
  return matched;
}

// ---------------------------------------------------------------------------
// Pure impact aggregation
// ---------------------------------------------------------------------------

export function buildImpact(params: {
  symbolName: string;
  definitions: readonly CrossRepoSymbol[];
  references: readonly ReferenceHit[];
  knownFiles: readonly CrossRepoFileRef[];
  codeownersByRoot: ReadonlyMap<string, CodeownersRule[]>;
  primaryRootLabel: string | null;
}): ImpactResult {
  const { symbolName, definitions, references, knownFiles, codeownersByRoot, primaryRootLabel } = params;

  const fileRefMap = new Map<string, CrossRepoFileRef>();
  for (const d of definitions) fileRefMap.set(d.file, { file: d.file, rootId: d.rootId, rootLabel: d.rootLabel });
  for (const r of references) fileRefMap.set(r.file, { file: r.file, rootId: r.rootId, rootLabel: r.rootLabel });

  const affectedFiles = [...fileRefMap.keys()].sort();
  const affectedRoots = [...new Set([...fileRefMap.values()].map((f) => f.rootLabel))].sort();

  const knownFileSet = new Set(knownFiles.map((f) => f.file));
  const testMatches = new Map<string, CrossRepoFileRef>();
  for (const ref of fileRefMap.values()) {
    if (isLikelyTestFile(ref.file)) testMatches.set(ref.file, ref);
    for (const candidate of guessTestCandidates(ref)) {
      if (knownFileSet.has(candidate.file)) testMatches.set(candidate.file, candidate);
    }
  }

  const owners: FileOwners[] = affectedFiles.map((file) => {
    const ref = fileRefMap.get(file)!;
    const isPrimary = ref.rootLabel === primaryRootLabel;
    const rules = codeownersByRoot.get(ref.rootLabel) ?? [];
    const relPath = relativeToRoot(file, ref.rootLabel, isPrimary);
    return { file, owners: matchCodeowners(rules, relPath) };
  });

  const migrationSteps: string[] = [];
  if (definitions.length === 0) {
    migrationSteps.push(
      `No definition of "${symbolName}" was found in the indexed roots — confirm the spelling, or rebuild the index if files changed since it was built.`,
    );
  }
  if (references.length > 0) {
    migrationSteps.push(
      `Review and update ${references.length} reference(s) across ${affectedFiles.length} file(s) for any signature or behavior change.`,
    );
  } else {
    migrationSteps.push(
      "No references were found by text search — this symbol may be unused, or only reached indirectly (e.g. dynamic dispatch, reflection); double-check before removing it.",
    );
  }
  if (testMatches.size > 0) {
    migrationSteps.push(`Run the ${testMatches.size} matched test file(s) before merging: ${[...testMatches.keys()].join(", ")}.`);
  } else {
    migrationSteps.push("No test file was found by naming convention — consider adding coverage before changing this symbol.");
  }
  if (affectedRoots.length > 1) {
    migrationSteps.push(`Coordinate the change across ${affectedRoots.length} repos/roots: ${affectedRoots.join(", ")}.`);
  }
  if (owners.length > 0 && owners.every((o) => o.owners.length === 0)) {
    migrationSteps.push("No CODEOWNERS entry matched any affected file — ownership is unassigned in this workspace.");
  }

  return {
    symbolName,
    definitions: [...definitions],
    references: [...references],
    affectedRoots,
    affectedFiles,
    testMatches: [...testMatches.values()],
    owners,
    migrationSteps,
  };
}

// ---------------------------------------------------------------------------
// Tauri-backed fetchers — reuse the existing `tool_glob`/`tool_grep`/
// `tool_read_file` frontend<->Rust bridge (src-tauri/src/tools.rs) already
// exposed to the agent's own file tools, rather than adding a new backend.
// ---------------------------------------------------------------------------

const INDEXED_EXTENSIONS = ["ts", "tsx", "js", "jsx", "py", "rs", "go"] as const;
/** Safety cap per root across every indexed extension combined, so a huge
 * attached folder can't turn "rebuild on demand" into a multi-minute hang. */
const MAX_FILES_PER_ROOT = 1500;
const CODEOWNERS_CANDIDATES = ["CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"];

function rootPathArg(root: WorkspaceRootInfo): string | undefined {
  return root.is_primary ? undefined : `${root.label}/`;
}

function escapeRegex(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

async function globExtension(root: WorkspaceRootInfo, ext: string): Promise<string[]> {
  try {
    return await invoke<string[]>("tool_glob", { pattern: `**/*.${ext}`, path: rootPathArg(root) });
  } catch {
    return [];
  }
}

async function readFileSafe(path: string): Promise<string | null> {
  try {
    return await invoke<string>("tool_read_file", { path });
  } catch {
    return null;
  }
}

export async function listIndexableFiles(root: WorkspaceRootInfo): Promise<CrossRepoFileRef[]> {
  const lists = await Promise.all(INDEXED_EXTENSIONS.map((ext) => globExtension(root, ext)));
  const files = [...new Set(lists.flat())].slice(0, MAX_FILES_PER_ROOT);
  return files.map((file) => ({ file, rootId: root.id, rootLabel: root.label }));
}

export async function buildCrossRepoIndex(
  roots: readonly WorkspaceRootInfo[],
): Promise<{ symbols: CrossRepoSymbol[]; files: CrossRepoFileRef[] }> {
  const symbols: CrossRepoSymbol[] = [];
  const files: CrossRepoFileRef[] = [];

  for (const root of roots) {
    const rootFiles = await listIndexableFiles(root);
    files.push(...rootFiles);
    for (const ref of rootFiles) {
      const content = await readFileSafe(ref.file);
      if (content == null) continue;
      for (const found of extractSymbols(content, ref.file)) {
        symbols.push({ ...found, file: ref.file, rootId: ref.rootId, rootLabel: ref.rootLabel });
      }
    }
  }

  return { symbols, files };
}

export async function findReferences(
  symbolName: string,
  roots: readonly WorkspaceRootInfo[],
): Promise<ReferenceHit[]> {
  const pattern = `\\b${escapeRegex(symbolName)}\\b`;
  const hits: ReferenceHit[] = [];

  for (const root of roots) {
    try {
      const matches = await invoke<Array<{ file: string; line: number; text: string }>>("tool_grep", {
        pattern,
        path: rootPathArg(root),
      });
      for (const m of matches) {
        hits.push({ file: m.file, line: m.line, text: m.text, rootId: root.id, rootLabel: root.label });
      }
    } catch {
      // Root unreadable (e.g. detached mid-query) — the other roots still
      // contribute; a partial impact view beats failing the whole query.
    }
  }

  return hits;
}

export async function loadCodeowners(root: WorkspaceRootInfo): Promise<CodeownersRule[]> {
  const prefix = rootPathArg(root) ?? "";
  for (const candidate of CODEOWNERS_CANDIDATES) {
    const content = await readFileSafe(`${prefix}${candidate}`);
    if (content != null) return parseCodeowners(content);
  }
  return [];
}

export async function queryImpact(params: {
  symbolName: string;
  roots: readonly WorkspaceRootInfo[];
  symbols: readonly CrossRepoSymbol[];
  files: readonly CrossRepoFileRef[];
}): Promise<ImpactResult> {
  const { symbolName, roots, symbols, files } = params;
  const definitions = symbols.filter((s) => s.name === symbolName);
  const primaryRootLabel = roots.find((r) => r.is_primary)?.label ?? null;

  const [references, codeownersEntries] = await Promise.all([
    findReferences(symbolName, roots),
    Promise.all(roots.map(async (root) => [root.label, await loadCodeowners(root)] as const)),
  ]);

  return buildImpact({
    symbolName,
    definitions,
    references,
    knownFiles: files,
    codeownersByRoot: new Map(codeownersEntries),
    primaryRootLabel,
  });
}
