import { describe, expect, it, vi } from "vitest";

import {
  MAX_CLAIMS,
  MAX_EVIDENCE_SPANS,
  assistantTextFromMessages,
  buildExtractionMessages,
  extractClaims,
  groundedSpans,
  materializeClaim,
  parseExtractionResponse,
  type ModelCallResult,
} from "./evidenceBoard";
import type { ChatMessage } from "./llamaClient";

describe("assistantTextFromMessages", () => {
  it("keeps only assistant text, joined and separated", () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "What is the revenue figure?" },
      { role: "assistant", content: "Revenue grew 40% year over year." },
      { role: "system", content: "private control message" },
      { role: "assistant", content: "The team shipped the feature on time." },
    ];
    const text = assistantTextFromMessages(messages);
    expect(text).toContain("Revenue grew 40% year over year.");
    expect(text).toContain("The team shipped the feature on time.");
    expect(text).not.toContain("private control message");
    expect(text).not.toContain("revenue figure");
  });

  it("returns an empty string when there is no assistant text", () => {
    expect(assistantTextFromMessages([{ role: "user", content: "hi" }])).toBe("");
  });
});

describe("groundedSpans", () => {
  const source = "Revenue grew 40% year over year. The team shipped on time. Costs were flat.";

  it("keeps verbatim substrings and drops fabricated ones", () => {
    const kept = groundedSpans(["Revenue grew 40% year over year.", "Profit tripled overnight."], source);
    expect(kept).toEqual(["Revenue grew 40% year over year."]);
  });

  it("tolerates whitespace re-wrapping in an otherwise verbatim quote", () => {
    const kept = groundedSpans(["Revenue grew 40%\nyear over year."], source);
    expect(kept).toEqual(["Revenue grew 40%\nyear over year."]);
  });

  it("de-duplicates and caps at MAX_EVIDENCE_SPANS", () => {
    const repeated = Array(MAX_EVIDENCE_SPANS + 5).fill("Costs were flat.");
    const kept = groundedSpans(repeated, source);
    expect(kept).toEqual(["Costs were flat."]);
  });

  it("drops empty/whitespace-only spans", () => {
    expect(groundedSpans(["   ", ""], source)).toEqual([]);
  });
});

describe("parseExtractionResponse", () => {
  const source = "Revenue grew 40% year over year. Analysts had projected only 10% growth. The launch date slipped twice.";

  it("parses a well-formed response and derives unresolved from evidence, not the model's confidence label", () => {
    const content = JSON.stringify({
      claims: [
        {
          claim: "Revenue grew 40% year over year",
          confidence: "high",
          supporting: ["Revenue grew 40% year over year."],
          conflicting: [],
          unresolvedQuestion: null,
        },
        {
          claim: "The launch was on schedule",
          confidence: "high",
          supporting: [],
          conflicting: ["The launch date slipped twice."],
          unresolvedQuestion: null,
        },
      ],
    });
    const claims = parseExtractionResponse(content, source);
    expect(claims).toHaveLength(2);
    expect(claims[0]).toMatchObject({ confidence: "high", unresolved: false });
    expect(claims[0].supportingEvidence).toEqual(["Revenue grew 40% year over year."]);
    // No supporting evidence at all -> unresolved, regardless of the
    // model's own (wrong) "high" confidence label.
    expect(claims[1]).toMatchObject({ confidence: "high", unresolved: true });
    expect(claims[1].conflictingEvidence).toEqual(["The launch date slipped twice."]);
  });

  it("flags unresolved when conflicting evidence outweighs supporting evidence", () => {
    const content = JSON.stringify({
      claims: [
        {
          claim: "Growth was solid",
          confidence: "medium",
          supporting: ["Revenue grew 40% year over year."],
          conflicting: ["Analysts had projected only 10% growth.", "The launch date slipped twice."],
        },
      ],
    });
    expect(parseExtractionResponse(content, source)[0].unresolved).toBe(true);
  });

  it("flags unresolved when an unresolvedQuestion is present even with strong support", () => {
    const content = JSON.stringify({
      claims: [
        {
          claim: "Revenue grew 40%",
          confidence: "high",
          supporting: ["Revenue grew 40% year over year."],
          conflicting: [],
          unresolvedQuestion: "Was this figure audited?",
        },
      ],
    });
    const claims = parseExtractionResponse(content, source);
    expect(claims[0].unresolved).toBe(true);
    expect(claims[0].unresolvedQuestion).toBe("Was this figure audited?");
  });

  it("drops fabricated quotes that are not verbatim substrings of the source", () => {
    const content = JSON.stringify({
      claims: [
        {
          claim: "Revenue tripled",
          confidence: "high",
          supporting: ["Revenue tripled overnight, stunning everyone."],
        },
      ],
    });
    const claims = parseExtractionResponse(content, source);
    expect(claims[0].supportingEvidence).toEqual([]);
    expect(claims[0].unresolved).toBe(true);
  });

  it("falls back to an embedded JSON object inside surrounding prose", () => {
    const content = `Sure, here you go:\n${JSON.stringify({
      claims: [{ claim: "Revenue grew 40%", confidence: "low", supporting: [], conflicting: [] }],
    })}\nHope that helps!`;
    const claims = parseExtractionResponse(content, source);
    expect(claims).toHaveLength(1);
    expect(claims[0].text).toBe("Revenue grew 40%");
  });

  it("defaults an invalid confidence value to low", () => {
    const content = JSON.stringify({ claims: [{ claim: "X happened", confidence: "definitely", supporting: [] }] });
    expect(parseExtractionResponse(content, source)[0].confidence).toBe("low");
  });

  it("returns an empty array for unparseable content", () => {
    expect(parseExtractionResponse("not json at all", source)).toEqual([]);
  });

  it("returns an empty array when claims is missing or not an array", () => {
    expect(parseExtractionResponse(JSON.stringify({ claims: "nope" }), source)).toEqual([]);
    expect(parseExtractionResponse(JSON.stringify({}), source)).toEqual([]);
  });

  it("caps the number of claims at MAX_CLAIMS", () => {
    const claims = Array.from({ length: MAX_CLAIMS + 10 }, (_, i) => ({
      claim: `Claim number ${i}`,
      confidence: "low",
      supporting: [],
    }));
    const result = parseExtractionResponse(JSON.stringify({ claims }), source);
    expect(result).toHaveLength(MAX_CLAIMS);
  });
});

describe("buildExtractionMessages", () => {
  it("wraps the source in an untrusted-content tag and reports no truncation for short input", () => {
    const { messages, truncated, groundingSource } = buildExtractionMessages("Short report text.");
    expect(truncated).toBe(false);
    expect(groundingSource).toBe("Short report text.");
    expect(messages[0].role).toBe("system");
    expect(messages[1].content).toContain("<untrusted_source_text>");
    expect(messages[1].content).toContain("Short report text.");
  });

  it("truncates very long source text and reports it", () => {
    const long = "x".repeat(30_000);
    const { truncated, groundingSource } = buildExtractionMessages(long);
    expect(truncated).toBe(true);
    expect(groundingSource.length).toBeLessThan(long.length);
  });
});

describe("extractClaims", () => {
  it("returns grounded claims on a successful call", async () => {
    const source = "The API now supports pagination.";
    const callModel = vi.fn(
      async (): Promise<ModelCallResult> => ({
        content: JSON.stringify({
          claims: [{ claim: "The API supports pagination", confidence: "high", supporting: [source] }],
        }),
        streamError: null,
      })
    );
    const result = await extractClaims(source, callModel);
    expect(result.claims).toHaveLength(1);
    expect(result.truncated).toBe(false);
    expect(callModel).toHaveBeenCalledTimes(1);
  });

  it("throws on empty source text without calling the model", async () => {
    const callModel = vi.fn();
    await expect(extractClaims("   ", callModel)).rejects.toThrow("no text to extract");
    expect(callModel).not.toHaveBeenCalled();
  });

  it("throws when the model call reports a stream error", async () => {
    const callModel = vi.fn(async (): Promise<ModelCallResult> => ({ content: "", streamError: "connection lost" }));
    await expect(extractClaims("Some report text.", callModel)).rejects.toThrow("connection lost");
  });

  it("throws when the model returns no extractable claims", async () => {
    const callModel = vi.fn(async (): Promise<ModelCallResult> => ({ content: "not json", streamError: null }));
    await expect(extractClaims("Some report text.", callModel)).rejects.toThrow("did not return any extractable claims");
  });

  it("aborts the callModel signal when the parent signal is aborted", async () => {
    const controller = new AbortController();
    const callModel = vi.fn(
      (_messages, signal: AbortSignal) =>
        new Promise<ModelCallResult>((resolve) => {
          signal.addEventListener("abort", () => resolve({ content: "", streamError: "aborted" }), { once: true });
        })
    );
    const promise = extractClaims("Some report text.", callModel, controller.signal);
    controller.abort();
    await expect(promise).rejects.toThrow("aborted");
  });
});

describe("materializeClaim", () => {
  it("fills in id/owner/status/createdAt around the extracted fields", () => {
    const claim = materializeClaim(
      { text: "X happened", confidence: "medium", supportingEvidence: [], conflictingEvidence: [], unresolvedQuestion: null, unresolved: true },
      () => "fixed-id"
    );
    expect(claim).toMatchObject({ id: "fixed-id", owner: "", status: "open", text: "X happened" });
    expect(typeof claim.createdAt).toBe("number");
  });
});
