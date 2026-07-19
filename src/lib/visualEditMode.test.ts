import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

const attemptStreamMock = vi.fn();
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

const resolveTargetMock = vi.fn();
vi.mock("./agentLoop", () => ({ resolveTarget: (...args: unknown[]) => resolveTargetMock(...args) }));

import {
  computeUnifiedDiff,
  extractSearchTerms,
  findCandidateFiles,
  parseProposalResponse,
  proposeVisualEdit,
  writeVisualEditToDisk,
  type VisualEditElement,
} from "./visualEditMode";

const localTarget = { kind: "local" as const, baseUrl: "http://localhost:8090", modelLabel: "Local model" };

function element(overrides: Partial<VisualEditElement> = {}): VisualEditElement {
  return {
    selector: "button.cta",
    tag: "button",
    role: "button",
    ariaLabel: "",
    text: "Get started",
    rect: { x: 10, y: 20, width: 120, height: 40 },
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  attemptStreamMock.mockReset();
  resolveTargetMock.mockReset();
  resolveTargetMock.mockResolvedValue(localTarget);
});

describe("extractSearchTerms", () => {
  it("includes the full visible text when short enough", () => {
    const terms = extractSearchTerms(element({ text: "Get started" }));
    expect(terms).toContain("Get started");
  });

  it("omits the full text when it's too long, but still yields word terms", () => {
    const longText = "This is a very long paragraph of body copy that would make a poor grep pattern on its own";
    const terms = extractSearchTerms(element({ text: longText }));
    expect(terms).not.toContain(longText);
    expect(terms.length).toBeGreaterThan(0);
    expect(terms.every((term) => term.length >= 3)).toBe(true);
  });

  it("includes the aria-label when present", () => {
    const terms = extractSearchTerms(element({ text: "", ariaLabel: "Close dialog" }));
    expect(terms).toContain("Close dialog");
  });

  it("returns no terms for a completely empty element", () => {
    expect(extractSearchTerms(element({ text: "", ariaLabel: "" }))).toEqual([]);
  });
});

describe("computeUnifiedDiff", () => {
  it("returns an empty string for identical content", () => {
    expect(computeUnifiedDiff("same\nfile\n", "same\nfile\n", "a.tsx")).toBe("");
  });

  it("produces a standard unified diff header and hunk for a changed line", () => {
    const oldContent = "line1\nline2\nline3\n";
    const newContent = "line1\nCHANGED\nline3\n";
    const diff = computeUnifiedDiff(oldContent, newContent, "src/Widget.tsx");
    expect(diff).toContain("--- a/src/Widget.tsx");
    expect(diff).toContain("+++ b/src/Widget.tsx");
    expect(diff).toContain("@@");
    expect(diff).toContain("-line2");
    expect(diff).toContain("+CHANGED");
  });
});

describe("parseProposalResponse", () => {
  it("parses a clean JSON object", () => {
    const raw = '{"targetFile": "src/Foo.tsx", "newContent": "export const x = 1;", "summary": "did a thing"}';
    expect(parseProposalResponse(raw)).toEqual({
      targetFile: "src/Foo.tsx",
      newContent: "export const x = 1;",
      summary: "did a thing",
    });
  });

  it("tolerates an accidental markdown code fence", () => {
    const raw = '```json\n{"targetFile": "src/Foo.tsx", "newContent": "x", "summary": "s"}\n```';
    expect(parseProposalResponse(raw)?.targetFile).toBe("src/Foo.tsx");
  });

  it("returns null for non-JSON garbage", () => {
    expect(parseProposalResponse("not json at all")).toBeNull();
  });

  it("treats an explicit null targetFile as a declined match", () => {
    const raw = '{"targetFile": null, "newContent": null, "summary": "no match"}';
    expect(parseProposalResponse(raw)).toEqual({ targetFile: null, newContent: null, summary: "no match" });
  });
});

describe("findCandidateFiles", () => {
  it("ranks files by number of matched search terms and reads only the top files", async () => {
    invokeMock.mockImplementation((cmd: string, args?: Record<string, unknown>) => {
      if (cmd === "tool_grep") {
        const pattern = args?.pattern as string;
        if (pattern.includes("Get")) {
          return Promise.resolve([
            { file: "src/components/Cta.tsx", line: 10, text: "Get started" },
            { file: "src/components/Other.tsx", line: 3, text: "Get started too" },
          ]);
        }
        if (pattern.includes("started")) {
          return Promise.resolve([{ file: "src/components/Cta.tsx", line: 10, text: "Get started" }]);
        }
        return Promise.resolve([]);
      }
      if (cmd === "tool_read_file") {
        return Promise.resolve(`content of ${args?.path}`);
      }
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    const files = await findCandidateFiles(element({ text: "Get started" }));
    expect(files[0].path).toBe("src/components/Cta.tsx");
    expect(files[0].content).toBe("content of src/components/Cta.tsx");
  });

  it("ignores grep matches outside recognized UI source extensions", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "tool_grep") {
        return Promise.resolve([
          { file: "README.md", line: 1, text: "Get started" },
          { file: "src/components/Cta.test.tsx", line: 1, text: "Get started" },
        ]);
      }
      if (cmd === "tool_read_file") return Promise.resolve("content");
      throw new Error("unexpected invoke");
    });

    const files = await findCandidateFiles(element({ text: "Get started" }));
    // Cta.test.tsx still matches the extension regex (.tsx) but README.md never does.
    expect(files.some((f) => f.path === "README.md")).toBe(false);
  });

  it("returns an empty list when nothing matches", async () => {
    invokeMock.mockResolvedValue([]);
    const files = await findCandidateFiles(element({ text: "zzz", ariaLabel: "" }));
    expect(files).toEqual([]);
  });
});

describe("proposeVisualEdit", () => {
  function mockSearchAndRead(path: string, content: string) {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "tool_grep") return Promise.resolve([{ file: path, line: 1, text: "Get started" }]);
      if (cmd === "tool_read_file") return Promise.resolve(content);
      throw new Error(`unexpected invoke: ${cmd}`);
    });
  }

  it("returns a full proposal when the model names a real candidate file", async () => {
    mockSearchAndRead("src/components/Cta.tsx", "export function Cta() {\n  return <button>Get started</button>;\n}\n");
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify({
        targetFile: "src/components/Cta.tsx",
        newContent: "export function Cta() {\n  return <button className=\"text-lg\">Get started</button>;\n}\n",
        summary: "Made the button text larger",
      }),
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    const proposal = await proposeVisualEdit({
      element: element(),
      description: "make this button larger",
      pageUrl: "http://localhost:3000/",
    });

    expect(proposal.targetFile).toBe("src/components/Cta.tsx");
    expect(proposal.summary).toBe("Made the button text larger");
    expect(proposal.unifiedDiff).toContain("+  return <button className=\"text-lg\">Get started</button>;");
    expect(resolveTargetMock).toHaveBeenCalledTimes(1);
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
    // attemptStream(target, messages, tools, signal, effort, sessionId, onDelta, recordUsage)
    expect(attemptStreamMock.mock.calls[0][7]).toBe(false);
  });

  it("throws when no candidate files were found", async () => {
    invokeMock.mockResolvedValue([]);
    await expect(
      proposeVisualEdit({ element: element({ text: "", ariaLabel: "" }), description: "x", pageUrl: "" }),
    ).rejects.toThrow(/could not find a source file/i);
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });

  it("throws when the model declines to match any candidate", async () => {
    mockSearchAndRead("src/components/Cta.tsx", "content");
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify({ targetFile: null, newContent: null, summary: "Nothing matched well enough" }),
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    await expect(
      proposeVisualEdit({ element: element(), description: "make it blue", pageUrl: "" }),
    ).rejects.toThrow(/nothing matched well enough/i);
  });

  it("throws when the model names a file outside the searched candidates", async () => {
    mockSearchAndRead("src/components/Cta.tsx", "content");
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify({ targetFile: "src/components/OtherFile.tsx", newContent: "x", summary: "s" }),
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    await expect(proposeVisualEdit({ element: element(), description: "x", pageUrl: "" })).rejects.toThrow(
      /wasn't among the searched candidate files/i,
    );
  });

  it("throws when the model's stream itself fails", async () => {
    mockSearchAndRead("src/components/Cta.tsx", "content");
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [], streamError: "network broke", contentStarted: false });

    await expect(proposeVisualEdit({ element: element(), description: "x", pageUrl: "" })).rejects.toThrow(
      "network broke",
    );
  });

  it("throws when the proposed content is identical to what's on disk", async () => {
    const content = "export function Cta() {\n  return <button>Get started</button>;\n}\n";
    mockSearchAndRead("src/components/Cta.tsx", content);
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify({ targetFile: "src/components/Cta.tsx", newContent: content, summary: "no-op" }),
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });

    await expect(proposeVisualEdit({ element: element(), description: "x", pageUrl: "" })).rejects.toThrow(
      /identical to what is already on disk/i,
    );
  });
});

describe("writeVisualEditToDisk", () => {
  it("invokes tool_write_file with the target path and content", async () => {
    invokeMock.mockResolvedValue(undefined);
    await writeVisualEditToDisk("src/components/Cta.tsx", "new content");
    expect(invokeMock).toHaveBeenCalledWith("tool_write_file", { path: "src/components/Cta.tsx", content: "new content" });
  });
});
