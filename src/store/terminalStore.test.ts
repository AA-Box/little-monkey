import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  outputHandler: null as ((event: { payload: unknown }) => void) | null,
  statusHandler: null as ((event: { payload: unknown }) => void) | null,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (name: string, handler: (event: { payload: unknown }) => void) => {
    if (name === "terminal://output") mocks.outputHandler = handler;
    if (name === "terminal://status") mocks.statusHandler = handler;
    return Promise.resolve(() => {});
  },
}));

import {
  MAX_TERMINAL_EVIDENCE_CHARS,
  MAX_TERMINAL_OUTPUT_CHARS,
  appendBoundedTerminalOutput,
  bracketedPasteInput,
  buildTerminalEvidence,
  disposeTerminalListenersForTests,
  readableTerminalOutput,
  terminalCommandLines,
  useTerminalStore,
  type TerminalSession,
} from "./terminalStore";

function session(overrides: Partial<TerminalSession> = {}): TerminalSession {
  return {
    id: "term-1",
    workspace_id: "/workspace",
    workspace_path: "/workspace",
    shell: "/bin/zsh",
    status: "running",
    exit_code: null,
    output: "",
    output_truncated: false,
    started_at_ms: 1,
    ...overrides,
  };
}

beforeEach(() => {
  disposeTerminalListenersForTests();
  mocks.invoke.mockReset();
  mocks.outputHandler = null;
  mocks.statusHandler = null;
  useTerminalStore.setState({
    sessions: [],
    activeSessionId: null,
    historyByWorkspace: {},
    pendingEvidenceByChat: {},
    initialized: false,
    busy: false,
    error: null,
  });
});

describe("terminal output bounds", () => {
  it("keeps only the newest bounded tail", () => {
    const output = appendBoundedTerminalOutput("a".repeat(MAX_TERMINAL_OUTPUT_CHARS), "tail");
    expect(output).toHaveLength(MAX_TERMINAL_OUTPUT_CHARS);
    expect(output.endsWith("tail")).toBe(true);
  });

  it("normalizes ANSI and carriage returns for display", () => {
    expect(readableTerminalOutput("\u001b[31mred\u001b[0m\r\nnext\rline")).toBe("red\nnext\nline");
  });

  it("drops the stray C0 controls a shell prompt leaves behind but keeps newline and tab", () => {
    expect(readableTerminalOutput("\u0001ahmad@Mac\u0002 \u001b]0;title\u0007» ls\u007f")).toBe("ahmad@Mac » ls");
    // Keypad-mode and reset escapes a zsh prompt emits around every redraw:
    // the payload characters must go with the ESC, not survive as text.
    expect(readableTerminalOutput("\u001b=\u001bccat log\u001b>")).toBe("cat log");
    expect(readableTerminalOutput("\u001b]133;C\u001b\\done")).toBe("done");
    expect(readableTerminalOutput("\u001b(Bplain")).toBe("plain");
    expect(readableTerminalOutput("a\tb\nc")).toBe("a\tb\nc");
  });

  it("caps evidence independently and labels truncation", () => {
    const evidence = buildTerminalEvidence(session({ output: "x".repeat(MAX_TERMINAL_EVIDENCE_CHARS + 50) }), undefined, 5);
    expect(evidence.content).toContain("Earlier terminal output omitted");
    expect(evidence.content.endsWith("x".repeat(MAX_TERMINAL_EVIDENCE_CHARS))).toBe(true);
    expect(evidence.truncated).toBe(true);
  });
});

describe("terminal store lifecycle", () => {
  it("hydrates sessions and applies output/status events", async () => {
    mocks.invoke.mockResolvedValueOnce([session()]);
    await useTerminalStore.getState().initialize();

    mocks.outputHandler?.({ payload: { session_id: "term-1", chunk: "hello", output_truncated: false } });
    expect(useTerminalStore.getState().sessions[0].output).toBe("hello");

    mocks.statusHandler?.({ payload: { session: session({ status: "exited", exit_code: 0, output: "hello" }) } });
    expect(useTerminalStore.getState().sessions[0].status).toBe("exited");
  });

  it("replaces a restarted tab with the returned PTY session", async () => {
    useTerminalStore.setState({ sessions: [session()], activeSessionId: "term-1" });
    const restarted = session({ id: "term-2", started_at_ms: 2 });
    mocks.invoke.mockResolvedValueOnce(restarted);

    await useTerminalStore.getState().restart("term-1", 30, 100);

    expect(mocks.invoke).toHaveBeenCalledWith("terminal_restart", { sessionId: "term-1", rows: 30, cols: 100 });
    expect(useTerminalStore.getState().sessions.map((entry) => entry.id)).toEqual(["term-2"]);
    expect(useTerminalStore.getState().activeSessionId).toBe("term-2");
  });

  it("refreshes backend-owned history after raw Enter without parsing keystrokes", async () => {
    useTerminalStore.setState({ sessions: [session()], activeSessionId: "term-1" });
    mocks.invoke.mockImplementation(async (command: string) => {
      if (command === "terminal_history") return ["echo hello"];
      return undefined;
    });

    await useTerminalStore.getState().write("term-1", "echo hello");
    expect(mocks.invoke).toHaveBeenCalledTimes(1);

    await useTerminalStore.getState().write("term-1", "\r");
    expect(mocks.invoke).toHaveBeenNthCalledWith(2, "terminal_write", {
      sessionId: "term-1",
      data: "\r",
    });
    expect(mocks.invoke).toHaveBeenNthCalledWith(3, "terminal_history", {
      workspaceId: "/workspace",
    });
    expect(useTerminalStore.getState().historyByWorkspace["/workspace"]).toEqual(["echo hello"]);
  });

  it("serializes raw writes so shell editing bytes keep their xterm order", async () => {
    let releaseFirst!: () => void;
    mocks.invoke
      .mockImplementationOnce(() => new Promise<void>((resolve) => {
        releaseFirst = resolve;
      }))
      .mockResolvedValueOnce(undefined);

    const first = useTerminalStore.getState().write("term-1", "a");
    const second = useTerminalStore.getState().write("term-1", "\u007f");
    await Promise.resolve();
    await Promise.resolve();
    expect(mocks.invoke).toHaveBeenCalledTimes(1);

    releaseFirst();
    await Promise.all([first, second]);
    expect(mocks.invoke).toHaveBeenNthCalledWith(1, "terminal_write", {
      sessionId: "term-1",
      data: "a",
    });
    expect(mocks.invoke).toHaveBeenNthCalledWith(2, "terminal_write", {
      sessionId: "term-1",
      data: "\u007f",
    });
  });

  it("keeps interrupt and kill as distinct PTY controls", async () => {
    useTerminalStore.setState({ sessions: [session()], activeSessionId: "term-1" });
    mocks.invoke
      .mockResolvedValueOnce(undefined)
      .mockResolvedValueOnce(session({ status: "killed" }));

    await useTerminalStore.getState().interrupt("term-1");
    await useTerminalStore.getState().kill("term-1");

    expect(mocks.invoke).toHaveBeenNthCalledWith(1, "terminal_interrupt", { sessionId: "term-1" });
    expect(mocks.invoke).toHaveBeenNthCalledWith(2, "terminal_kill", { sessionId: "term-1" });
    expect(useTerminalStore.getState().sessions[0].status).toBe("killed");
  });

  it("submits a multi-line snippet one line at a time, in order", async () => {
    // `terminal_execute` rejects an embedded newline ("Submit one terminal
    // command line at a time"), so a multi-line fence has to arrive as
    // separate submissions or nothing runs at all.
    expect(terminalCommandLines("  rm ./a  \n\n codesign ./B.app \n")).toEqual([
      "rm ./a",
      "codesign ./B.app",
    ]);

    mocks.invoke.mockResolvedValue(undefined);
    useTerminalStore.setState({ sessions: [session()], activeSessionId: "term-1" });
    await useTerminalStore.getState().executeScript("term-1", "rm ./a\ncodesign ./B.app");

    const executed = mocks.invoke.mock.calls
      .filter(([name]) => name === "terminal_execute")
      .map(([, args]) => (args as { command: string }).command);
    expect(executed).toEqual(["rm ./a", "codesign ./B.app"]);
  });

  it("wraps only multi-line input in bracketed-paste markers", () => {
    expect(bracketedPasteInput("ls -l")).toBe("ls -l");
    expect(bracketedPasteInput("a\nb")).toBe("\u001B[200~a\nb\u001B[201~");
  });

  it("queues and atomically consumes evidence for one chat", () => {
    const evidence = buildTerminalEvidence(session({ output: "tests passed" }), undefined, 7);
    useTerminalStore.getState().queueEvidence("chat-a", evidence);

    expect(useTerminalStore.getState().consumeEvidence("chat-a")).toEqual([evidence]);
    expect(useTerminalStore.getState().consumeEvidence("chat-a")).toEqual([]);
  });
});
