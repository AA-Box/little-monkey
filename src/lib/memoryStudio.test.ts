import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

const writeTextFileMock = vi.fn();
vi.mock("@tauri-apps/plugin-fs", () => ({ writeTextFile: (...args: unknown[]) => writeTextFileMock(...args) }));

import {
  buildMemoryExport,
  deleteMemory,
  exportMemories,
  importMemories,
  listAllMemories,
  setMemoryEnabled,
  updateMemory,
  type MemoryEntry,
} from "./memoryStudio";

function entry(overrides: Partial<MemoryEntry> = {}): MemoryEntry {
  return {
    id: "fact-1",
    text: "Uses pnpm, not npm.",
    source: "agent",
    created_at: "2026-01-01T00:00:00.000Z",
    enabled: true,
    source_turn_id: null,
    scope: "project",
    project_root: "/ws/project",
    ...overrides,
  };
}

describe("memoryStudio client", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    writeTextFileMock.mockReset();
  });

  it("listAllMemories calls memory_list_all with no arguments", async () => {
    invokeMock.mockResolvedValue([entry()]);
    const result = await listAllMemories();
    expect(invokeMock).toHaveBeenCalledWith("memory_list_all");
    expect(result).toEqual([entry()]);
  });

  it("updateMemory forwards id, projectRoot, and text to memory_studio_update", async () => {
    invokeMock.mockResolvedValue(entry({ text: "edited" }));
    await updateMemory("fact-1", "/ws/project", "edited");
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_update", {
      id: "fact-1",
      projectRoot: "/ws/project",
      text: "edited",
    });
  });

  it("updateMemory forwards a null projectRoot for a global memory", async () => {
    invokeMock.mockResolvedValue(entry({ scope: "global", project_root: null }));
    await updateMemory("fact-1", null, "edited");
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_update", {
      id: "fact-1",
      projectRoot: null,
      text: "edited",
    });
  });

  it("setMemoryEnabled forwards id, projectRoot, and enabled to memory_studio_set_enabled", async () => {
    invokeMock.mockResolvedValue(entry({ enabled: false }));
    await setMemoryEnabled("fact-1", "/ws/project", false);
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_set_enabled", {
      id: "fact-1",
      projectRoot: "/ws/project",
      enabled: false,
    });
  });

  it("deleteMemory forwards id and projectRoot to memory_studio_delete", async () => {
    invokeMock.mockResolvedValue(undefined);
    await deleteMemory("fact-1", "/ws/project");
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_delete", {
      id: "fact-1",
      projectRoot: "/ws/project",
    });
  });

  it("importMemories forwards path to memory_import", async () => {
    invokeMock.mockResolvedValue({ added: 1, skipped_duplicate: 0, errors: [] });
    await importMemories("/tmp/in.json");
    expect(invokeMock).toHaveBeenCalledWith("memory_import", { path: "/tmp/in.json" });
  });

  describe("buildMemoryExport", () => {
    // Split so secret scanners don't flag the fixture as a real key.
    const FAKE_KEY = ["sk-live-", "abcdef0123456789ABCDEF"].join("");

    it("redacts secret-shaped text by default and flags which entries changed", () => {
      const secret = entry({ id: "a", text: `api_key: ${FAKE_KEY}` });
      const plain = entry({ id: "b", text: "Uses pnpm, not npm." });

      const file = buildMemoryExport([secret, plain], true);

      expect(file.redacted).toBe(true);
      expect(file.entries).toHaveLength(2);

      const redactedSecret = file.entries.find((e) => e.id === "a")!;
      expect(redactedSecret.redacted).toBe(true);
      expect(redactedSecret.text).not.toContain(FAKE_KEY);

      const untouchedPlain = file.entries.find((e) => e.id === "b")!;
      expect(untouchedPlain.redacted).toBe(false);
      expect(untouchedPlain.text).toBe("Uses pnpm, not npm.");
    });

    it("keeps original text verbatim when redact is false", () => {
      const secret = entry({ text: `api_key: ${FAKE_KEY}` });
      const file = buildMemoryExport([secret], false);

      expect(file.redacted).toBe(false);
      expect(file.entries[0].redacted).toBe(false);
      expect(file.entries[0].text).toBe(`api_key: ${FAKE_KEY}`);
    });

    it("preserves every other MemoryEntry field on each exported entry", () => {
      const source = entry({ id: "z", enabled: false, source_turn_id: "turn-9", scope: "global", project_root: null });
      const file = buildMemoryExport([source], true);
      const [exported] = file.entries;
      expect(exported.id).toBe("z");
      expect(exported.enabled).toBe(false);
      expect(exported.source_turn_id).toBe("turn-9");
      expect(exported.scope).toBe("global");
      expect(exported.project_root).toBeNull();
    });
  });

  describe("exportMemories", () => {
    it("lists every memory, redacts by default, and writes the file via writeTextFile", async () => {
      invokeMock.mockImplementation((cmd: string) => {
        if (cmd === "memory_list_all") return Promise.resolve([entry()]);
        return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
      });
      writeTextFileMock.mockResolvedValue(undefined);

      const summary = await exportMemories("/tmp/out.json");

      expect(invokeMock).toHaveBeenCalledWith("memory_list_all");
      expect(writeTextFileMock).toHaveBeenCalledTimes(1);
      const [path, contents] = writeTextFileMock.mock.calls[0];
      expect(path).toBe("/tmp/out.json");
      const parsed = JSON.parse(contents as string);
      expect(parsed.redacted).toBe(true);
      expect(parsed.entries).toHaveLength(1);
      expect(summary).toEqual({ path: "/tmp/out.json", count: 1, redacted_count: 0 });
    });

    it("does not call the removed memory_export Tauri command", async () => {
      invokeMock.mockImplementation((cmd: string) => {
        if (cmd === "memory_list_all") return Promise.resolve([]);
        return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
      });
      writeTextFileMock.mockResolvedValue(undefined);

      await exportMemories("/tmp/out.json");

      expect(invokeMock).not.toHaveBeenCalledWith("memory_export", expect.anything());
    });

    it("reports a redacted count when secret-shaped text is masked", async () => {
      invokeMock.mockImplementation((cmd: string) => {
        if (cmd === "memory_list_all") {
          return Promise.resolve([entry({ text: "password: hunter2hunter2hunter2" })]);
        }
        return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
      });
      writeTextFileMock.mockResolvedValue(undefined);

      const summary = await exportMemories("/tmp/out.json", true);
      expect(summary.redacted_count).toBe(1);
    });
  });
});
