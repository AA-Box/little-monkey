import { describe, expect, it } from "vitest";

import {
  buildLabReport,
  createLabPrompt,
  emptyResult,
  renderLabReportMarkdown,
  type LabRun,
} from "./compareLab";
import type { ModelTargetSnapshot } from "./modelTargets";

function target(displayName: string): ModelTargetSnapshot {
  return {
    kind: "provider",
    key: "provider:test:model",
    label: "Test Provider",
    displayName,
    providerId: "test",
    endpoint: "https://provider.test/v1",
    model: "model",
    credentialRefId: "keychain:com.littlemonkey.app:test",
    capabilities: {
      toolCalling: { state: "unknown", evidence: "test" },
      vision: { state: "unknown", evidence: "test" },
    },
    availability: { status: "available", evidence: "test" },
  };
}

/** One-prompt, one-model run whose free-text fields carry `text`, so the
 * report renderer's escaping is what decides how they land in Markdown. */
function runWithUntrustedText(text: string): LabRun {
  const prompt = createLabPrompt(text);
  const snapshot = target(text);
  return {
    id: "run",
    suiteId: "suite",
    suiteName: "Suite",
    suiteCategory: "custom",
    modelSetId: "set",
    modelSetName: "Set",
    prompts: [prompt],
    targets: [snapshot],
    createdAt: 0,
    completedAt: null,
    status: "completed",
    results: [
      {
        ...emptyResult(prompt.id, snapshot.key, false),
        status: "completed",
        content: "ok",
        error: text,
      },
    ],
  };
}

describe("renderLabReportMarkdown escaping", () => {
  it("escapes a backslash that would otherwise release the pipe after it", () => {
    // Regression: escaping `|` alone turned `a\|b` into `a\\|b`, which Markdown
    // reads as a literal backslash followed by an *unescaped* pipe — the model
    // label broke out of its summary-table cell.
    const markdown = renderLabReportMarkdown(buildLabReport(runWithUntrustedText("a\\|b")));
    expect(markdown).toContain("a\\\\\\|b");
    expect(markdown).not.toContain("a\\\\|b");
  });

  it("still escapes a bare pipe and collapses newlines", () => {
    const markdown = renderLabReportMarkdown(buildLabReport(runWithUntrustedText("a|b\nc")));
    expect(markdown).toContain("a\\|b c");
  });

  it("leaves text with neither a pipe nor a backslash untouched", () => {
    const markdown = renderLabReportMarkdown(buildLabReport(runWithUntrustedText("plain label")));
    expect(markdown).toContain("plain label");
    expect(markdown).not.toContain("\\plain");
  });
});
