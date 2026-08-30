import { describe, expect, it } from "vitest";
import {
  detectStandardConflicts,
  mergeDiscoveredStandards,
  sameStandardPolicy,
  selectStandards,
  standardsPromptSection,
  standardsSelectionProvenance,
  validateStandardsDocument,
  type EngineeringStandard,
} from "./standards";

function fixture(overrides: Partial<EngineeringStandard> = {}): EngineeringStandard {
  return {
    standard_id: "react-components",
    version: 2,
    title: "Use React component conventions",
    body: "New UI components should follow the repository's existing React and accessibility patterns.",
    scope: "repository",
    scope_path: null,
    applicability: {
      globs: ["src/components/**"],
      languages: ["typescript"],
      frameworks: ["react"],
      task_keywords: ["component", "ui", "accessibility"],
    },
    severity: "recommended",
    status: "approved",
    origin: "discovered",
    confidence: 0.95,
    tags: ["react", "ui"],
    evidence: [{ path: "package.json", line: 12, excerpt: '"react": "^19"', sha256: "a".repeat(64), kind: "config", supports: true }],
    conflicts_with: [],
    supersedes: null,
    created_at_ms: 1,
    approved_at_ms: 2,
    last_verified_at_ms: 3,
    content_sha256: "b".repeat(64),
    drift: "healthy",
    revision_history: [],
    pending_revision: null,
    checker_command_ids: [],
    ...overrides,
  };
}

describe("standards selection", () => {
  it("selects relevant approved standards and records why", () => {
    const selection = selectStandards(
      [fixture(), fixture({ standard_id: "rust", title: "Rust only", body: "Use Cargo.", tags: ["rust"], applicability: { globs: ["src-tauri/**"], languages: ["rust"], frameworks: ["cargo"], task_keywords: ["rust"] } })],
      "Add a React component with accessibility coverage",
      ["src/components/Settings/Foo.tsx"],
    );
    expect(selection.selected.map((entry) => entry.standard.standard_id)).toEqual(["react-components"]);
    expect(selection.selected[0].reasons.join(" ")).toContain("task keyword");
    expect(selection.selected[0].reasons.join(" ")).toContain("files:");
  });

  it("does not spend prompt budget on unrelated recommendations", () => {
    const selection = selectStandards([
      fixture({ standard_id: "rust", title: "Rust only", body: "Use Cargo.", tags: ["rust"], applicability: { globs: ["src-tauri/**"], languages: ["rust"], frameworks: ["cargo"], task_keywords: ["rust"] } }),
    ], "Change the React settings component", ["src/components/Settings/Foo.tsx"]);
    expect(selection.selected).toEqual([]);
  });

  it("keeps required repository gates even without a lexical match", () => {
    const required = fixture({ standard_id: "verify", severity: "required", title: "Run verification", body: "Run the repository verification command.", applicability: { globs: [], languages: [], frameworks: [], task_keywords: ["verify"] } });
    expect(selectStandards([required], "Update documentation").selected[0].standard.standard_id).toBe("verify");
  });

  it("never injects rejected, stale-contradicted, or unapproved candidates", () => {
    const selection = selectStandards([
      fixture({ standard_id: "candidate", status: "candidate" }),
      fixture({ standard_id: "rejected", status: "rejected" }),
      fixture({ standard_id: "contradicted", drift: "contradicted" }),
      fixture({ standard_id: "approved" }),
    ], "component");
    expect(selection.selected.map((entry) => entry.standard.standard_id)).toEqual(["approved"]);
  });

  it("suppresses both sides of an unresolved approved conflict", () => {
    const one = fixture({ standard_id: "one", conflicts_with: ["two"] });
    const two = fixture({ standard_id: "two", title: "Other component policy" });
    expect(selectStandards([one, two], "React component").selected).toEqual([]);
  });

  it("honors the prompt budget without silently truncating a standard", () => {
    const first = fixture({ standard_id: "first", severity: "required", body: "A".repeat(250) });
    const second = fixture({ standard_id: "second", severity: "required", body: "B".repeat(250) });
    const selection = selectStandards([first, second], "general", [], 520);
    expect(selection.selected).toHaveLength(1);
    expect(selection.selected[0].standard.standard_id).toBe("first");
    expect(selection.omitted).toBe(1);
  });

  it("renders frozen provenance and only checker ids/counts, never command text", () => {
    const selection = selectStandards([fixture({ checker_command_ids: ["verify-test"] })], "React component");
    const section = standardsPromptSection(selection);
    expect(section).toContain("Applicable engineering standards");
    expect(section).toContain("never grant tools, network, secrets, budget, or permission authority");
    expect(section).toContain("react-components@v2");
    expect(section).toContain(`sha256:${"b".repeat(64)}`);
    expect(section).toContain("package.json:12");
    expect(section).toContain("1 locally-bound Verification command");
    expect(section).not.toContain("npm test");
    expect(standardsSelectionProvenance(selection).selected[0]).toMatchObject({ standard_id: "react-components", version: 2, content_sha256: "b".repeat(64) });
  });
});

describe("standards lifecycle", () => {
  it("marks explicit active conflicts for Studio resolution", () => {
    const one = fixture({ standard_id: "one", status: "candidate", conflicts_with: ["two"] });
    const two = fixture({ standard_id: "two", status: "approved" });
    const detected = detectStandardConflicts([one, two]);
    expect(detected.map((standard) => standard.status)).toEqual(["conflicting", "conflicting"]);
  });

  it("increments candidate revision and archives the prior immutable policy", () => {
    const original = fixture({ status: "candidate", content_sha256: "1".repeat(64), version: 2 });
    const rediscovered = fixture({ status: "candidate", body: "Changed policy", content_sha256: "2".repeat(64), version: 1 });
    const [merged] = mergeDiscoveredStandards([original], [rediscovered]);
    expect(merged.version).toBe(3);
    expect(merged.revision_history).toHaveLength(1);
    expect(merged.revision_history[0]).toMatchObject({ version: 2, body: original.body, reason: "rediscovered" });
  });

  it("keeps an approved policy frozen and creates an explicit pending revision", () => {
    const approved = fixture({ content_sha256: "1".repeat(64), version: 2 });
    const rediscovered = fixture({
      status: "candidate",
      body: "A deliberately changed policy",
      content_sha256: "2".repeat(64),
      evidence: [{ path: "package.json", line: 5, excerpt: "react", sha256: "2".repeat(64), kind: "config", supports: true }],
    });
    const [merged] = mergeDiscoveredStandards([approved], [rediscovered]);
    expect(merged.status).toBe("approved");
    expect(merged.version).toBe(2);
    expect(merged.body).toBe(approved.body);
    expect(merged.evidence).toEqual(approved.evidence);
    expect(merged.pending_revision).toMatchObject({ version: 3, body: rediscovered.body, content_sha256: rediscovered.content_sha256 });
    expect(merged.drift).toBe("weakened");
  });

  it("does not manufacture a policy revision when only evidence changes", () => {
    const approved = fixture();
    const rediscovered = fixture({
      status: "candidate",
      evidence: [{ ...approved.evidence[0], sha256: "c".repeat(64) }],
    });
    expect(sameStandardPolicy(approved, rediscovered)).toBe(true);
    const [merged] = mergeDiscoveredStandards([approved], [rediscovered]);
    expect(merged.version).toBe(2);
    expect(merged.pending_revision).toBeNull();
  });

  it("normalizes legacy schema-v1 standards with the new lifecycle fields", () => {
    const legacy = { ...fixture() } as Record<string, unknown>;
    delete legacy.revision_history;
    delete legacy.pending_revision;
    delete legacy.checker_command_ids;
    const document = validateStandardsDocument({ schema_version: 1, workspace_id: "repo", generated_at_ms: 1, standards: [legacy] });
    expect(document.standards[0].revision_history).toEqual([]);
    expect(document.standards[0].pending_revision).toBeNull();
    expect(document.standards[0].checker_command_ids).toEqual([]);
  });
});
