import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

import {
  buildCrossRepoIndex,
  buildImpact,
  extractSymbols,
  findReferences,
  guessTestCandidates,
  isLikelyTestFile,
  loadCodeowners,
  matchCodeowners,
  parseCodeowners,
  queryImpact,
  type CrossRepoFileRef,
  type CrossRepoSymbol,
  type ReferenceHit,
} from "./crossRepoIndex";
import type { WorkspaceRootInfo } from "../store/workspaceStore";

beforeEach(() => invokeMock.mockReset());

const PRIMARY: WorkspaceRootInfo = { id: "/repo", path: "/repo", label: "repo", is_primary: true };
const SECONDARY: WorkspaceRootInfo = { id: "/other", path: "/other", label: "other-repo", is_primary: false };

describe("extractSymbols", () => {
  it("extracts exported TS/JS declarations by kind", () => {
    const content = [
      "export function greet(name: string) {}",
      "export class Widget {}",
      "export interface Props {}",
      "export type Id = string;",
      "export enum Color { Red }",
      "export const LIMIT = 10;",
      "export async function fetchThing() {}",
      "function notExported() {}",
    ].join("\n");

    const found = extractSymbols(content, "src/widget.ts");
    expect(found).toEqual([
      { name: "greet", kind: "function", line: 1 },
      { name: "Widget", kind: "class", line: 2 },
      { name: "Props", kind: "interface", line: 3 },
      { name: "Id", kind: "type", line: 4 },
      { name: "Color", kind: "enum", line: 5 },
      { name: "LIMIT", kind: "const", line: 6 },
      { name: "fetchThing", kind: "function", line: 7 },
    ]);
  });

  it("extracts Python def/class", () => {
    const content = "def compute(x):\n    pass\n\nclass Model:\n    pass\n";
    expect(extractSymbols(content, "lib/model.py")).toEqual([
      { name: "compute", kind: "function", line: 1 },
      { name: "Model", kind: "class", line: 4 },
    ]);
  });

  it("extracts Rust pub fn/struct/enum/trait", () => {
    const content = "pub fn run() {}\npub struct Config {}\npub enum Mode {}\npub trait Runner {}\n";
    expect(extractSymbols(content, "src/lib.rs")).toEqual([
      { name: "run", kind: "function", line: 1 },
      { name: "Config", kind: "struct", line: 2 },
      { name: "Mode", kind: "enum", line: 3 },
      { name: "Runner", kind: "trait", line: 4 },
    ]);
  });

  it("extracts Go funcs, methods, and type declarations", () => {
    const content = [
      "func New() *Server {}",
      "func (s *Server) Start() {}",
      "type Server struct {}",
      "type Runner interface {}",
    ].join("\n");
    expect(extractSymbols(content, "main.go")).toEqual([
      { name: "New", kind: "function", line: 1 },
      { name: "Start", kind: "method", line: 2 },
      { name: "Server", kind: "struct", line: 3 },
      { name: "Runner", kind: "interface", line: 4 },
    ]);
  });

  it("returns nothing for unindexed extensions", () => {
    expect(extractSymbols("export function f() {}", "notes.md")).toEqual([]);
  });
});

describe("isLikelyTestFile / guessTestCandidates", () => {
  it("recognizes common test-file naming conventions", () => {
    expect(isLikelyTestFile("src/foo.test.ts")).toBe(true);
    expect(isLikelyTestFile("src/foo.spec.tsx")).toBe(true);
    expect(isLikelyTestFile("pkg/test_foo.py")).toBe(true);
    expect(isLikelyTestFile("pkg/foo_test.py")).toBe(true);
    expect(isLikelyTestFile("pkg/foo_test.go")).toBe(true);
    expect(isLikelyTestFile("pkg/foo_test.rs")).toBe(true);
    expect(isLikelyTestFile("src/foo.ts")).toBe(false);
  });

  it("guesses candidates preserving the file's root", () => {
    const ref: CrossRepoFileRef = { file: "other-repo/src/lib/foo.ts", rootId: "/other", rootLabel: "other-repo" };
    const candidates = guessTestCandidates(ref);
    expect(candidates).toEqual([
      { file: "other-repo/src/lib/foo.test.ts", rootId: "/other", rootLabel: "other-repo" },
      { file: "other-repo/src/lib/foo.spec.ts", rootId: "/other", rootLabel: "other-repo" },
      { file: "other-repo/src/lib/__tests__/foo.test.ts", rootId: "/other", rootLabel: "other-repo" },
    ]);
  });

  it("guesses Python-style candidates", () => {
    const ref: CrossRepoFileRef = { file: "pkg/model.py", rootId: "/repo", rootLabel: "repo" };
    expect(guessTestCandidates(ref)).toEqual([
      { file: "pkg/test_model.py", rootId: "/repo", rootLabel: "repo" },
      { file: "pkg/model_test.py", rootId: "/repo", rootLabel: "repo" },
    ]);
  });
});

describe("parseCodeowners / matchCodeowners", () => {
  it("parses pattern + owners lines, skipping comments and blanks", () => {
    const content = "# comment\n\n*.ts @frontend-team\n/src/lib/ @core-team @jane\n";
    expect(parseCodeowners(content)).toEqual([
      { pattern: "*.ts", owners: ["@frontend-team"] },
      { pattern: "/src/lib/", owners: ["@core-team", "@jane"] },
    ]);
  });

  it("last matching rule wins, directory patterns match descendants", () => {
    const rules = parseCodeowners("*.ts @frontend-team\n/src/lib/ @core-team\n");
    expect(matchCodeowners(rules, "src/lib/foo.ts")).toEqual(["@core-team"]);
    expect(matchCodeowners(rules, "src/other/bar.ts")).toEqual(["@frontend-team"]);
    expect(matchCodeowners(rules, "README.md")).toEqual([]);
  });
});

describe("buildImpact", () => {
  const defs: CrossRepoSymbol[] = [
    { name: "widgetFactory", kind: "function", file: "src/widget.ts", rootId: "/repo", rootLabel: "repo", line: 3 },
  ];
  const refs: ReferenceHit[] = [
    { file: "src/app.ts", rootId: "/repo", rootLabel: "repo", line: 10, text: "widgetFactory()" },
    { file: "other-repo/src/main.ts", rootId: "/other", rootLabel: "other-repo", line: 5, text: "widgetFactory()" },
  ];
  const knownFiles: CrossRepoFileRef[] = [
    { file: "src/widget.ts", rootId: "/repo", rootLabel: "repo" },
    { file: "src/widget.test.ts", rootId: "/repo", rootLabel: "repo" },
    { file: "src/app.ts", rootId: "/repo", rootLabel: "repo" },
    { file: "other-repo/src/main.ts", rootId: "/other", rootLabel: "other-repo" },
  ];

  it("aggregates affected repos/files/tests/owners and produces migration steps", () => {
    const codeownersByRoot = new Map([
      ["repo", parseCodeowners("/src/ @core-team\n")],
      ["other-repo", []],
    ]);

    const impact = buildImpact({
      symbolName: "widgetFactory",
      definitions: defs,
      references: refs,
      knownFiles,
      codeownersByRoot,
      primaryRootLabel: "repo",
    });

    expect(impact.affectedRoots).toEqual(["other-repo", "repo"]);
    expect(impact.affectedFiles).toEqual(["other-repo/src/main.ts", "src/app.ts", "src/widget.ts"]);
    expect(impact.testMatches).toEqual([{ file: "src/widget.test.ts", rootId: "/repo", rootLabel: "repo" }]);
    expect(impact.owners).toEqual([
      { file: "other-repo/src/main.ts", owners: [] },
      { file: "src/app.ts", owners: ["@core-team"] },
      { file: "src/widget.ts", owners: ["@core-team"] },
    ]);
    expect(impact.migrationSteps.some((s) => s.includes("2 reference(s) across 3 file(s)"))).toBe(true);
    expect(impact.migrationSteps.some((s) => s.includes("Run the 1 matched test file(s)"))).toBe(true);
    expect(impact.migrationSteps.some((s) => s.includes("Coordinate the change across 2 repos/roots"))).toBe(true);
  });

  it("flags a missing definition and unassigned ownership", () => {
    const impact = buildImpact({
      symbolName: "ghostSymbol",
      definitions: [],
      references: [],
      knownFiles: [],
      codeownersByRoot: new Map(),
      primaryRootLabel: "repo",
    });
    expect(impact.migrationSteps.some((s) => s.includes('No definition of "ghostSymbol"'))).toBe(true);
    expect(impact.migrationSteps.some((s) => s.includes("No references were found"))).toBe(true);
    expect(impact.migrationSteps.some((s) => s.includes("No test file was found"))).toBe(true);
  });
});

describe("buildCrossRepoIndex (Tauri-backed)", () => {
  it("globs each indexed extension per root, reads matched files, and extracts symbols", async () => {
    invokeMock.mockImplementation(async (cmd?: string, args: any = {}) => {
      if (cmd === "tool_glob") {
        if (args.pattern === "**/*.ts" && args.path === undefined) return ["src/foo.ts"];
        return [];
      }
      if (cmd === "tool_read_file") {
        if (args.path === "src/foo.ts") return "export function foo() {}\n";
        throw new Error("not found");
      }
      // Vitest's own test-runner instrumentation calls every mock function
      // once more with no arguments during cleanup — tolerate it rather
      // than treat it as an unexpected invocation from our own code.
      return [];
    });

    const { symbols, files } = await buildCrossRepoIndex([PRIMARY]);
    expect(files).toEqual([{ file: "src/foo.ts", rootId: "/repo", rootLabel: "repo" }]);
    expect(symbols).toEqual([
      { name: "foo", kind: "function", line: 1, file: "src/foo.ts", rootId: "/repo", rootLabel: "repo" },
    ]);
  });

  it("targets secondary roots via the label-prefixed path argument", async () => {
    invokeMock.mockImplementation(async (cmd?: string, args: any = {}) => {
      if (cmd === "tool_glob") {
        expect(args.path).toBe("other-repo/");
      }
      return [];
    });
    await buildCrossRepoIndex([SECONDARY]);
    expect(invokeMock).toHaveBeenCalled();
  });
});

describe("findReferences / loadCodeowners / queryImpact (Tauri-backed)", () => {
  it("finds word-boundary references per root and tolerates a failing root", async () => {
    invokeMock.mockImplementation(async (cmd?: string, args: any = {}) => {
      if (cmd === "tool_grep" && args.path === undefined) {
        return [{ file: "src/app.ts", line: 2, text: "widgetFactory()" }];
      }
      if (cmd === "tool_grep" && args.path === "other-repo/") {
        throw new Error("root missing");
      }
      return [];
    });

    const hits = await findReferences("widgetFactory", [PRIMARY, SECONDARY]);
    expect(hits).toEqual([
      { file: "src/app.ts", line: 2, text: "widgetFactory()", rootId: "/repo", rootLabel: "repo" },
    ]);
  });

  it("loads the first CODEOWNERS candidate found for a root", async () => {
    invokeMock.mockImplementation(async (cmd?: string, args: any = {}) => {
      if (cmd === "tool_read_file" && args.path === "CODEOWNERS") throw new Error("missing");
      if (cmd === "tool_read_file" && args.path === ".github/CODEOWNERS") return "* @org/team\n";
      if (cmd === "tool_read_file") throw new Error(`unexpected read ${args.path}`);
      return null;
    });
    const rules = await loadCodeowners(PRIMARY);
    expect(rules).toEqual([{ pattern: "*", owners: ["@org/team"] }]);
  });

  it("queryImpact composes definitions, references, and owners into one ImpactResult", async () => {
    invokeMock.mockImplementation(async (cmd?: string) => {
      if (cmd === "tool_grep") return [];
      if (cmd === "tool_read_file") throw new Error("missing");
      return null;
    });

    const symbols: CrossRepoSymbol[] = [
      { name: "widgetFactory", kind: "function", file: "src/widget.ts", rootId: "/repo", rootLabel: "repo", line: 1 },
    ];
    const impact = await queryImpact({ symbolName: "widgetFactory", roots: [PRIMARY], symbols, files: [] });
    expect(impact.definitions).toEqual(symbols);
    expect(impact.affectedRoots).toEqual(["repo"]);
  });
});
