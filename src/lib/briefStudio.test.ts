import { beforeEach, describe, expect, it, vi } from "vitest";

// `generateBriefAsset` drives its one-shot completion via `turnEngine.ts`'s
// `attemptStream` against whatever `agentLoop.ts`'s `resolveTarget` resolves
// to — mocked here exactly like `sideTaskRunner.test.ts` mocks the same two
// functions, so these tests pin the MODULE's own behavior (policy gate,
// citation verification, unsupported-type short circuit) without a real
// streaming provider.
const attemptStreamMock = vi.fn();
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

const resolveTargetMock = vi.fn();
vi.mock("./agentLoop", () => ({ resolveTarget: (...args: unknown[]) => resolveTargetMock(...args) }));

import {
  BRIEF_STUDIO_USAGE_KEY,
  BriefStudioPolicyError,
  MAX_BLOCKS,
  MAX_BLOCK_CHARS,
  MAX_TOTAL_SOURCE_CHARS,
  buildKnowledgeStackSource,
  buildPastedSource,
  buildSessionSource,
  generateBriefAsset,
  normalizeSourceBlocks,
  verifyCitations,
  type SourceBlock,
} from "./briefStudio";
import type { ChatMessage } from "./llamaClient";

const localTarget = { kind: "local" as const, baseUrl: "http://localhost:8090", modelLabel: "Local model" };
const providerTarget = { kind: "provider" as const, providerId: "openai", model: "gpt-5" };

beforeEach(() => {
  attemptStreamMock.mockReset();
  resolveTargetMock.mockReset();
  resolveTargetMock.mockResolvedValue(localTarget);
});

describe("normalizeSourceBlocks", () => {
  it("numbers blocks sequentially and drops blank ones", () => {
    const blocks = normalizeSourceBlocks([
      { label: "A", text: "first" },
      { label: "B", text: "   " },
      { label: "C", text: "third" },
    ]);
    expect(blocks.map((b) => b.refId)).toEqual(["S1", "S2"]);
    expect(blocks.map((b) => b.label)).toEqual(["A", "C"]);
  });

  it("truncates a single oversized block rather than erroring", () => {
    const huge = "x".repeat(MAX_BLOCK_CHARS + 500);
    const [block] = normalizeSourceBlocks([{ label: "Huge", text: huge }]);
    expect(block.text.length).toBeLessThan(huge.length);
    expect(block.text.endsWith("[truncated]")).toBe(true);
  });

  it("caps total characters across many blocks", () => {
    const raw = Array.from({ length: 50 }, (_, i) => ({ label: `B${i}`, text: "y".repeat(1000) }));
    const blocks = normalizeSourceBlocks(raw);
    const total = blocks.reduce((sum, b) => sum + b.text.length, 0);
    expect(total).toBeLessThanOrEqual(MAX_TOTAL_SOURCE_CHARS);
  });

  it("caps the number of blocks", () => {
    const raw = Array.from({ length: MAX_BLOCKS + 10 }, (_, i) => ({ label: `B${i}`, text: `text ${i}` }));
    const blocks = normalizeSourceBlocks(raw);
    expect(blocks.length).toBeLessThanOrEqual(MAX_BLOCKS);
  });
});

describe("buildPastedSource / buildSessionSource / buildKnowledgeStackSource", () => {
  it("builds a single-block pasted source with a default label", () => {
    const source = buildPastedSource("  ", "Some pasted content about widgets.");
    expect(source.kind).toBe("pasted");
    expect(source.label).toBe("Pasted document");
    expect(source.blocks).toHaveLength(1);
    expect(source.blocks[0].text).toContain("widgets");
  });

  it("builds one block per user/assistant turn and skips system/tool messages", () => {
    const messages: ChatMessage[] = [
      { role: "system", content: "system prompt" },
      { role: "user", content: "What is the capital of France?" },
      { role: "assistant", content: "Paris is the capital of France." },
      { role: "tool", content: "tool output", tool_call_id: "1" } as ChatMessage,
    ];
    const source = buildSessionSource("Geography chat", messages);
    expect(source.blocks).toHaveLength(2);
    expect(source.blocks[0].label).toBe("Turn 1 (user)");
    expect(source.blocks[1].label).toBe("Turn 2 (assistant)");
  });

  it("builds a knowledge-stack source from raw hits", () => {
    const source = buildKnowledgeStackSource("My Stack", [
      { label: "doc.md", text: "The revenue grew 12% year over year." },
    ]);
    expect(source.kind).toBe("knowledge_stack");
    expect(source.blocks[0].refId).toBe("S1");
  });
});

describe("verifyCitations", () => {
  const blocks: SourceBlock[] = [
    { refId: "S1", label: "Report", text: "Revenue grew 12% year over year, driven by new products." },
  ];

  it("marks a verbatim quote as verified", () => {
    const content = 'Revenue increased significantly [S1: "Revenue grew 12% year over year"].';
    const citations = verifyCitations(content, blocks);
    expect(citations).toHaveLength(1);
    expect(citations[0].verified).toBe(true);
    expect(citations[0].refId).toBe("S1");
  });

  it("is whitespace/case-insensitive but still requires the exact word sequence", () => {
    const content = 'Growth was strong [S1: "REVENUE   grew 12% year over year"].';
    const citations = verifyCitations(content, blocks);
    expect(citations[0].verified).toBe(true);
  });

  it("marks a fabricated quote as unverified", () => {
    const content = 'Profits tripled [S1: "profits tripled overnight"].';
    const citations = verifyCitations(content, blocks);
    expect(citations[0].verified).toBe(false);
  });

  it("marks a citation to a nonexistent block as unverified", () => {
    const content = 'Something happened [S9: "made up quote"].';
    const citations = verifyCitations(content, blocks);
    expect(citations[0].verified).toBe(false);
    expect(citations[0].sourceLabel).toBeNull();
  });

  it("returns an empty list when the model cited nothing", () => {
    expect(verifyCitations("Plain text with no citations at all.", blocks)).toEqual([]);
  });
});

describe("generateBriefAsset", () => {
  const source = buildPastedSource("Doc", "Revenue grew 12% year over year, driven by new products.");

  it("throws without ever calling attemptStream when there are no source blocks", async () => {
    const empty = buildPastedSource("Empty", "   ");
    await expect(
      generateBriefAsset(empty, "brief", { requireLocalOnly: false }),
    ).rejects.toThrow(/no source material/i);
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });

  it("short-circuits unsupported asset types without calling resolveTarget or attemptStream", async () => {
    const result = await generateBriefAsset(source, "audio_overview", { requireLocalOnly: false });
    expect(result.supported).toBe(false);
    expect(result.unsupportedReason).toMatch(/isn't available yet/i);
    expect(result.content).toBe("");
    expect(resolveTargetMock).not.toHaveBeenCalled();
    expect(attemptStreamMock).not.toHaveBeenCalled();

    const videoResult = await generateBriefAsset(source, "video_outline", { requireLocalOnly: false });
    expect(videoResult.supported).toBe(false);
  });

  it("generates a supported asset, verifies citations, and reports ranLocally for a local target", async () => {
    attemptStreamMock.mockResolvedValue({
      content: 'Revenue is growing [S1: "Revenue grew 12% year over year"].',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    const result = await generateBriefAsset(source, "brief", { requireLocalOnly: false });

    expect(result.supported).toBe(true);
    expect(result.targetKind).toBe("local");
    expect(result.ranLocally).toBe(true);
    expect(result.citations).toHaveLength(1);
    expect(result.citations[0].verified).toBe(true);
    expect(result.unverifiedCitationCount).toBe(0);

    // never records into a real chat session's usage ledger
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
    expect(attemptStreamMock.mock.calls[0][5]).toBe(BRIEF_STUDIO_USAGE_KEY);
    expect(attemptStreamMock.mock.calls[0][7]).toBe(false);
  });

  it("flags an unverified citation without dropping it from the output", async () => {
    attemptStreamMock.mockResolvedValue({
      content: 'Profits tripled [S1: "profits tripled last quarter"].',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    const result = await generateBriefAsset(source, "study_guide", { requireLocalOnly: false });
    expect(result.content).toContain("profits tripled");
    expect(result.unverifiedCitationCount).toBe(1);
  });

  it("throws the model's stream error rather than returning a partial asset", async () => {
    attemptStreamMock.mockResolvedValue({
      content: "",
      toolCalls: [],
      streamError: "network broke",
      contentStarted: false,
    });
    await expect(generateBriefAsset(source, "quiz", { requireLocalOnly: false })).rejects.toThrow("network broke");
  });

  it("refuses to generate against a cloud provider when requireLocalOnly is set, before calling attemptStream", async () => {
    resolveTargetMock.mockResolvedValue(providerTarget);
    await expect(
      generateBriefAsset(source, "brief", { requireLocalOnly: true }),
    ).rejects.toThrow(BriefStudioPolicyError);
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });

  it("allows a cloud provider target when requireLocalOnly is false", async () => {
    resolveTargetMock.mockResolvedValue(providerTarget);
    attemptStreamMock.mockResolvedValue({
      content: 'Fine [S1: "Revenue grew 12% year over year"].',
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    const result = await generateBriefAsset(source, "flashcards", { requireLocalOnly: false });
    expect(result.targetKind).toBe("provider");
    expect(result.ranLocally).toBe(false);
  });
});
