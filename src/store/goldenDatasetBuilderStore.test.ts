import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

// `goldenDatasetBuilderStore.ts` drives its one-shot generation call through
// `agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s `attemptStream` —
// exactly the same pair `sopCompilerStore.ts`'s `compile()` uses for its own
// one-shot call — mocked here so these tests pin the STORE's own behavior
// (persistence, dedupe/privacy folding, versioning, eval) without needing a
// real streaming provider.
const resolveTargetMock = vi.fn();
vi.mock("../lib/agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => resolveTargetMock(...args),
}));

const attemptStreamMock = vi.fn();
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

import { useGoldenDatasetBuilderStore } from "./goldenDatasetBuilderStore";

function mockGenerationReply(examples: Array<Record<string, string>>): void {
  attemptStreamMock.mockImplementation(async () => ({
    content: JSON.stringify({ examples }),
    toolCalls: [],
    streamError: null,
    contentStarted: true,
  }));
}

describe("goldenDatasetBuilderStore", () => {
  beforeAll(() => {
    const values = new Map<string, string>();
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        clear: () => values.clear(),
        getItem: (key: string) => values.get(key) ?? null,
        setItem: (key: string, value: string) => values.set(key, value),
        removeItem: (key: string) => values.delete(key),
      },
    });
  });

  beforeEach(() => {
    localStorage.clear();
    resolveTargetMock.mockReset();
    attemptStreamMock.mockReset();
    resolveTargetMock.mockResolvedValue({ kind: "local", baseUrl: "http://localhost:8090" });
    useGoldenDatasetBuilderStore.setState({ datasets: [], activeDatasetId: null, generating: false });
  });

  it("creates a dataset with a parsed schema and an initial empty version entry", () => {
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "20 example support tickets", "text, category");
    const dataset = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    expect(dataset.fields).toEqual(["text", "category"]);
    expect(dataset.currentVersion).toBe(1);
    expect(dataset.versions).toHaveLength(1);
    expect(dataset.examples).toHaveLength(0);
    expect(useGoldenDatasetBuilderStore.getState().activeDatasetId).toBe(id);
  });

  it("generates synthetic examples, recording provenance with the exact generation prompt and bumping the version", async () => {
    mockGenerationReply([
      { text: "My order arrived late", category: "shipping" },
      { text: "I was charged twice", category: "billing" },
    ]);
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "support tickets", "text, category");
    await useGoldenDatasetBuilderStore.getState().generateExamples(id, 2);

    const dataset = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    expect(dataset.examples).toHaveLength(2);
    expect(dataset.currentVersion).toBe(2);
    expect(dataset.versions).toHaveLength(2);
    expect(dataset.versions[1].note).toContain("Generated 2 synthetic example(s)");
    for (const example of dataset.examples) {
      expect(example.provenance.kind).toBe("synthetic");
      if (example.provenance.kind === "synthetic") {
        expect(example.provenance.generationPrompt).toContain("support tickets");
      }
      expect(example.version).toBe(2);
      expect(example.included).toBe(true);
    }
  });

  it("throws instead of running a second concurrent generation", async () => {
    attemptStreamMock.mockImplementation(() => new Promise(() => {}));
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "support tickets", "text");
    const first = useGoldenDatasetBuilderStore.getState().generateExamples(id, 3);
    await vi.waitFor(() => expect(useGoldenDatasetBuilderStore.getState().generating).toBe(true));
    await expect(useGoldenDatasetBuilderStore.getState().generateExamples(id, 3)).rejects.toThrow("already in progress");
    void first;
  });

  it("records a generation failure on the dataset without throwing it away silently", async () => {
    attemptStreamMock.mockImplementation(async () => ({ content: "not json", toolCalls: [], streamError: null, contentStarted: true }));
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "support tickets", "text");
    await expect(useGoldenDatasetBuilderStore.getState().generateExamples(id, 3)).rejects.toThrow();
    const dataset = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    expect(dataset.lastError).toMatch(/did not return any usable examples/);
    expect(useGoldenDatasetBuilderStore.getState().generating).toBe(false);
  });

  it("imports real examples, excluding any that fail the privacy filter — never silently included", () => {
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "support tickets", "text, category");
    const raw = JSON.stringify([
      { text: "Order never arrived", category: "shipping" },
      { text: "Contact me at jane@example.com about my refund", category: "billing" },
    ]);
    const result = useGoldenDatasetBuilderStore.getState().importExamples(id, raw, "support-export.csv");
    expect(result).toEqual({ imported: 2, skippedLines: 0 });

    const dataset = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    expect(dataset.examples).toHaveLength(2);
    const clean = dataset.examples.find((e) => e.fields.text === "Order never arrived")!;
    expect(clean.included).toBe(true);
    expect(clean.provenance).toEqual({ kind: "imported", source: "support-export.csv" });

    const flagged = dataset.examples.find((e) => e.fields.text.includes("jane@example.com"))!;
    expect(flagged.included).toBe(false);
    expect(flagged.exclusionReason).toBe("privacy");
    expect(flagged.privacy.passed).toBe(false);

    expect(dataset.currentVersion).toBe(2);
    expect(dataset.versions[1].note).toContain("excluded by the privacy filter");
  });

  it("marks an exact duplicate between a synthetic batch and an import as excluded, keeping the earlier one canonical", async () => {
    mockGenerationReply([{ text: "My order never arrived", category: "shipping" }]);
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "support tickets", "text, category");
    await useGoldenDatasetBuilderStore.getState().generateExamples(id, 1);

    const raw = JSON.stringify([{ text: "my order never arrived!!", category: "shipping" }]);
    useGoldenDatasetBuilderStore.getState().importExamples(id, raw, "csv");

    const dataset = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    const synthetic = dataset.examples.find((e) => e.provenance.kind === "synthetic")!;
    const imported = dataset.examples.find((e) => e.provenance.kind === "imported")!;
    expect(synthetic.included).toBe(true);
    expect(imported.duplicateKind).toBe("exact");
    expect(imported.included).toBe(false);
    expect(imported.exclusionReason).toBe("duplicate");
  });

  it("deletes an example, bumps the version, and recomputes duplicates over what remains", async () => {
    mockGenerationReply([
      { text: "My order never arrived", category: "shipping" },
      { text: "Totally different complaint about billing errors", category: "billing" },
    ]);
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "support tickets", "text, category");
    await useGoldenDatasetBuilderStore.getState().generateExamples(id, 2);
    const dataset = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    const toDelete = dataset.examples[0].id;

    useGoldenDatasetBuilderStore.getState().deleteExample(id, toDelete);
    const updated = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    expect(updated.examples).toHaveLength(1);
    expect(updated.currentVersion).toBe(3);
    expect(updated.versions[2].note).toBe("Removed an example");
  });

  it("runs a schema-conformance eval reflecting only currently-included examples", () => {
    const id = useGoldenDatasetBuilderStore.getState().createDataset("Support tickets", "support tickets", "text, category");
    const raw = JSON.stringify([
      { text: "Order never arrived", category: "shipping" },
      { text: "Contact me at jane@example.com", category: "billing" },
    ]);
    useGoldenDatasetBuilderStore.getState().importExamples(id, raw, "csv");
    useGoldenDatasetBuilderStore.getState().runEval(id);

    const dataset = useGoldenDatasetBuilderStore.getState().datasets.find((d) => d.id === id)!;
    expect(dataset.evalRuns).toHaveLength(1);
    expect(dataset.evalRuns[0].total).toBe(1);
    expect(dataset.evalRuns[0].passed).toBe(1);
    expect(dataset.evalRuns[0].version).toBe(dataset.currentVersion);
  });

  it("persists datasets to localStorage and hydrates them back", () => {
    useGoldenDatasetBuilderStore.getState().createDataset("Persisted dataset", "seed", "text");
    const raw = localStorage.getItem("little-monkey-golden-datasets-v1");
    expect(raw).toBeTruthy();
    const parsed = JSON.parse(raw!);
    expect(parsed.datasets).toHaveLength(1);
    expect(parsed.datasets[0].name).toBe("Persisted dataset");
  });

  it("deletes a dataset and clears activeDatasetId if it was active", () => {
    const id = useGoldenDatasetBuilderStore.getState().createDataset("To delete", "seed", "text");
    useGoldenDatasetBuilderStore.getState().deleteDataset(id);
    expect(useGoldenDatasetBuilderStore.getState().datasets).toHaveLength(0);
    expect(useGoldenDatasetBuilderStore.getState().activeDatasetId).toBeNull();
  });
});
