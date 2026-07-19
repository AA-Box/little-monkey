import { describe, expect, it } from "vitest";

import {
  detectDuplicates,
  jaccardSimilarity,
  materializeExample,
  parseFieldsInput,
  parseGenerationResponse,
  parseImportedExamples,
  recomputeDuplicates,
  runPrivacyFilter,
  runSchemaConformanceEval,
  generateSyntheticExamples,
  buildGenerationMessages,
  type DatasetExample,
  type ModelCallResult,
} from "./goldenDatasetBuilder";
import type { ChatMessage } from "./llamaClient";

describe("parseFieldsInput", () => {
  it("trims, dedupes case-insensitively, and drops empties", () => {
    expect(parseFieldsInput("text, Category ,  , text ,category")).toEqual(["text", "Category"]);
  });

  it("caps at MAX_FIELDS", () => {
    const raw = Array.from({ length: 20 }, (_, i) => `field${i}`).join(",");
    const fields = parseFieldsInput(raw);
    expect(fields.length).toBeLessThanOrEqual(8);
  });
});

describe("runPrivacyFilter", () => {
  it("passes clean text", () => {
    expect(runPrivacyFilter("The customer asked about their order status.")).toEqual({ passed: true, findings: [] });
  });

  it("flags an email address", () => {
    const result = runPrivacyFilter("Contact me at jane.doe@example.com about this.");
    expect(result.passed).toBe(false);
    expect(result.findings).toEqual([{ type: "email", count: 1 }]);
  });

  it("flags a phone number", () => {
    const result = runPrivacyFilter("Call me at 555-123-4567 tomorrow.");
    expect(result.passed).toBe(false);
    expect(result.findings.some((f) => f.type === "phone")).toBe(true);
  });

  it("flags an SSN-like pattern", () => {
    const result = runPrivacyFilter("SSN on file: 123-45-6789");
    expect(result.passed).toBe(false);
    expect(result.findings).toEqual([{ type: "ssn", count: 1 }]);
  });

  it("flags a credit-card-like pattern", () => {
    const result = runPrivacyFilter("Card number 4111 1111 1111 1111 was declined.");
    expect(result.passed).toBe(false);
    expect(result.findings.some((f) => f.type === "creditCard")).toBe(true);
  });

  it("can flag multiple finding types in one text", () => {
    const result = runPrivacyFilter("Email jane@example.com or call 555-123-4567.");
    expect(result.passed).toBe(false);
    expect(result.findings.map((f) => f.type).sort()).toEqual(["email", "phone"]);
  });
});

describe("jaccardSimilarity", () => {
  it("is 1 for identical text", () => {
    expect(jaccardSimilarity("the quick brown fox", "the quick brown fox")).toBe(1);
  });

  it("is 0 for completely disjoint text", () => {
    expect(jaccardSimilarity("apples oranges bananas", "cars trucks planes")).toBe(0);
  });

  it("is high for near-identical text with minor rewording", () => {
    const similarity = jaccardSimilarity(
      "My package never arrived and tracking shows no updates.",
      "My package never arrived, and tracking shows no updates!",
    );
    expect(similarity).toBeGreaterThan(0.8);
  });
});

describe("detectDuplicates / recomputeDuplicates", () => {
  it("marks an exact normalized-text repeat as exact, keeping the first as canonical", () => {
    const results = detectDuplicates([
      { id: "a", fields: { text: "My order never arrived." } },
      { id: "b", fields: { text: "my order never arrived!!" } },
      { id: "c", fields: { text: "Totally unrelated content about billing." } },
    ]);
    expect(results.find((r) => r.id === "a")).toMatchObject({ duplicateKind: "none", duplicateOfId: null });
    expect(results.find((r) => r.id === "b")).toMatchObject({ duplicateKind: "exact", duplicateOfId: "a" });
    expect(results.find((r) => r.id === "c")).toMatchObject({ duplicateKind: "none", duplicateOfId: null });
  });

  it("marks a near (but not exact) match as near", () => {
    const results = detectDuplicates([
      { id: "a", fields: { text: "customer reports package never arrived and tracking shows no recent update at all" } },
      { id: "b", fields: { text: "customer reports package never arrived and tracking shows no new update at all" } },
    ]);
    expect(results.find((r) => r.id === "b")?.duplicateKind).toBe("near");
  });

  it("recomputeDuplicates folds duplicate status into inclusion without ever overriding a privacy exclusion", () => {
    const base: DatasetExample[] = [
      materializeExample({ text: "First support ticket about shipping." }, { kind: "synthetic", generationPrompt: "p" }, 1, () => "a"),
      materializeExample({ text: "first support ticket about shipping!" }, { kind: "synthetic", generationPrompt: "p" }, 1, () => "b"),
      materializeExample({ text: "Contact jane@example.com for a refund." }, { kind: "imported", source: "csv" }, 1, () => "c"),
    ];
    const recomputed = recomputeDuplicates(base);
    const dup = recomputed.find((e) => e.id === "b")!;
    expect(dup.duplicateKind).toBe("exact");
    expect(dup.included).toBe(false);
    expect(dup.exclusionReason).toBe("duplicate");

    const privacyFail = recomputed.find((e) => e.id === "c")!;
    expect(privacyFail.included).toBe(false);
    expect(privacyFail.exclusionReason).toBe("privacy");
  });
});

describe("materializeExample", () => {
  it("excludes and flags an example that fails the privacy filter", () => {
    const example = materializeExample(
      { text: "Reach me at 555-123-4567", category: "billing" },
      { kind: "imported", source: "support-export.csv" },
      2,
      () => "id-1",
    );
    expect(example.privacy.passed).toBe(false);
    expect(example.included).toBe(false);
    expect(example.exclusionReason).toBe("privacy");
    expect(example.provenance).toEqual({ kind: "imported", source: "support-export.csv" });
    expect(example.version).toBe(2);
  });

  it("includes a clean example and records synthetic provenance with its generation prompt", () => {
    const example = materializeExample(
      { text: "My package arrived damaged.", category: "shipping" },
      { kind: "synthetic", generationPrompt: 'Generate 5 example(s) for: "support tickets" with fields [text, category]' },
      1,
      () => "id-2",
    );
    expect(example.included).toBe(true);
    expect(example.exclusionReason).toBeNull();
    expect(example.provenance.kind).toBe("synthetic");
    if (example.provenance.kind === "synthetic") {
      expect(example.provenance.generationPrompt).toContain("support tickets");
    }
  });
});

describe("parseGenerationResponse", () => {
  it("parses a well-formed reply and drops incomplete items", () => {
    const content = JSON.stringify({
      examples: [
        { text: "Order is late", category: "shipping" },
        { text: "Missing a field" },
        { text: "Refund request", category: "billing" },
      ],
    });
    const parsed = parseGenerationResponse(content, ["text", "category"], 10);
    expect(parsed).toEqual([
      { text: "Order is late", category: "shipping" },
      { text: "Refund request", category: "billing" },
    ]);
  });

  it("recovers JSON embedded in surrounding prose", () => {
    const content = `Sure, here you go:\n${JSON.stringify({ examples: [{ text: "a", category: "b" }] })}\nHope that helps!`;
    expect(parseGenerationResponse(content, ["text", "category"], 5)).toEqual([{ text: "a", category: "b" }]);
  });

  it("returns an empty array for unparsable content", () => {
    expect(parseGenerationResponse("not json at all", ["text"], 5)).toEqual([]);
  });

  it("caps results at the requested count", () => {
    const content = JSON.stringify({ examples: Array.from({ length: 10 }, (_, i) => ({ text: `item ${i}` })) });
    expect(parseGenerationResponse(content, ["text"], 3)).toHaveLength(3);
  });
});

describe("generateSyntheticExamples", () => {
  const fakeCallModel =
    (content: string) =>
    async (_messages: ChatMessage[], _signal: AbortSignal): Promise<ModelCallResult> => ({ content, streamError: null });

  it("throws when the seed description is empty", async () => {
    await expect(generateSyntheticExamples("   ", ["text"], 5, fakeCallModel("{}"))).rejects.toThrow("Describe what");
  });

  it("throws when there are no schema fields", async () => {
    await expect(generateSyntheticExamples("support tickets", [], 5, fakeCallModel("{}"))).rejects.toThrow("schema field");
  });

  it("throws when the model returns no usable examples", async () => {
    await expect(generateSyntheticExamples("support tickets", ["text"], 5, fakeCallModel("garbage"))).rejects.toThrow(
      "did not return any usable examples",
    );
  });

  it("returns parsed examples plus the exact generation prompt used", async () => {
    const content = JSON.stringify({ examples: [{ text: "a", category: "b" }] });
    const { examples, prompt } = await generateSyntheticExamples("support tickets", ["text", "category"], 5, fakeCallModel(content));
    expect(examples).toEqual([{ text: "a", category: "b" }]);
    expect(prompt).toContain("support tickets");
    expect(prompt).toContain("text, category");
  });

  it("propagates a stream error instead of swallowing it", async () => {
    const callModel = async (): Promise<ModelCallResult> => ({ content: "", streamError: "connection lost" });
    await expect(generateSyntheticExamples("support tickets", ["text"], 5, callModel)).rejects.toThrow("connection lost");
  });
});

describe("buildGenerationMessages", () => {
  it("includes the seed, field list, and count in the stored prompt", () => {
    const { prompt, messages } = buildGenerationMessages("20 example support tickets", ["text", "category"], 20);
    expect(prompt).toBe('Generate 20 example(s) for: "20 example support tickets" with fields [text, category]');
    expect(messages[0].role).toBe("system");
    expect(messages[1].role).toBe("user");
  });
});

describe("parseImportedExamples", () => {
  it("parses a JSON array of matching objects", () => {
    const raw = JSON.stringify([
      { text: "Order never shipped", category: "shipping" },
      { text: "Wrong item received", category: "shipping" },
    ]);
    const result = parseImportedExamples(raw, ["text", "category"]);
    expect(result.examples).toHaveLength(2);
    expect(result.skippedLines).toBe(0);
  });

  it("skips JSON entries missing a required field", () => {
    const raw = JSON.stringify([{ text: "Order never shipped" }, { text: "ok", category: "billing" }]);
    const result = parseImportedExamples(raw, ["text", "category"]);
    expect(result.examples).toHaveLength(1);
    expect(result.skippedLines).toBe(1);
  });

  it("falls back to pipe-delimited lines when the input isn't JSON", () => {
    const raw = "Order never shipped|shipping\nWrong item received|shipping\nbad line with no delimiter";
    const result = parseImportedExamples(raw, ["text", "category"]);
    expect(result.examples).toEqual([
      { text: "Order never shipped", category: "shipping" },
      { text: "Wrong item received", category: "shipping" },
    ]);
    expect(result.skippedLines).toBe(1);
  });

  it("returns empty for blank input", () => {
    expect(parseImportedExamples("   ", ["text"])).toEqual({ examples: [], skippedLines: 0 });
  });
});

describe("runSchemaConformanceEval", () => {
  it("only counts included examples and reports how many are complete", () => {
    const examples: DatasetExample[] = [
      materializeExample({ text: "a", category: "b" }, { kind: "synthetic", generationPrompt: "p" }, 1, () => "1"),
      materializeExample({ text: "", category: "b" }, { kind: "synthetic", generationPrompt: "p" }, 1, () => "2"),
      materializeExample({ text: "reach me at jane@example.com", category: "b" }, { kind: "imported", source: "s" }, 1, () => "3"),
    ];
    const recomputed = recomputeDuplicates(examples);
    const evalResult = runSchemaConformanceEval({ examples: recomputed, fields: ["text", "category"], currentVersion: 1 });
    // Example "2" has an empty text field, so it's parsed as failing
    // materializeExample's own validity — but since materializeExample never
    // enforces non-empty fields itself (only generation/import parsing do),
    // it's included but incomplete, while "3" is excluded outright by privacy.
    expect(evalResult.total).toBe(2);
    expect(evalResult.passed).toBe(1);
    expect(evalResult.summary).toContain("1/2");
    expect(evalResult.version).toBe(1);
  });
});
