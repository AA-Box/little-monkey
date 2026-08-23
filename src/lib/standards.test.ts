import { describe, expect, it } from "vitest";
import {
  mergeDiscoveredStandards,
  selectStandards,
  standardsPromptSection,
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
    ...overrides,
  };
}

describe("standards selection", () => {
  it("selects relevant approved standards and records why", () => {
    const selection = selectStandards(
      [fixture(), fixture({ standard_id: "rust", title: "Rust only", body: "Use Cargo.", applicability: { globs: ["src-tauri/**"], languages: ["rust"], frameworks: ["cargo"], task_keywords: ["rust"] } })],
      "Add a React component with accessibility coverage",
      ["src/components/Settings/Foo.tsx"],
    );
    expect(selection.selected[0].standard.standard_id).toBe("react-components");
    expect(selection.selected[0].reasons.join(" ")).toContain("task keyword");
    expect(selection.selected[0].reasons.join(" ")).toContain("files:");
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

  it("honors the prompt budget without silently truncating a standard", () => {
    const first = fixture({ standard_id: "first", severity: "required", body: "A".repeat(250) });
    const second = fixture({ standard_id: "second", body: "B".repeat(250) });
    const selection = selectStandards([first, second], "general", [], 400);
    expect(selection.selected).toHaveLength(1);
    expect(selection.selected[0].standard.standard_id).toBe("first");
    expect(selection.omitted).toBe(1);
  });

  it("renders authority boundaries into the injected prompt", () => {
    const section = standardsPromptSection(selectStandards([fixture()], "React component"));
    expect(section).toContain("Applicable engineering standards");
    expect(section).toContain("never grant tools, network, secrets, budget, or permission authority");
    expect(section).toContain("react-components@v2");
  });
});

describe("discovery merge", () => {
  it("preserves approved content while refreshing evidence and drift", () => {
    const approved = fixture({ content_sha256: "1".repeat(64), evidence: [] });
    const rediscovered = fixture({
      status: "candidate",
      content_sha256: "2".repeat(64),
      evidence: [{ path: "package.json", line: 5, excerpt: "react", sha256: "2".repeat(64), kind: "config", supports: true }],
    });
    const [merged] = mergeDiscoveredStandards([approved], [rediscovered]);
    expect(merged.status).toBe("approved");
    expect(merged.version).toBe(2);
    expect(merged.evidence).toEqual(rediscovered.evidence);
    expect(merged.drift).toBe("weakened");
  });
});
