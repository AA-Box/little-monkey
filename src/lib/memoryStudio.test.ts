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
  markMemoriesUsed,
  mergeMemories,
  purgeExpiredMemories,
  setMemoryEnabled,
  setMemoryExpiry,
  setMemoryPinned,
  unmergeMemories,
  updateMemory,
  wouldReachPrompt,
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
    pinned: false,
    expires_at: null,
    last_used_at: null,
    merged_from: [],
    merged_into: null,
    retired_at: null,
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

describe("memoryStudio lifecycle commands", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("setMemoryPinned forwards id, projectRoot, and pinned", async () => {
    invokeMock.mockResolvedValue(entry({ pinned: true }));
    await setMemoryPinned("fact-1", "/ws/project", true);
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_set_pinned", {
      id: "fact-1",
      projectRoot: "/ws/project",
      pinned: true,
    });
  });

  it("setMemoryExpiry forwards a bare date and, separately, a null that clears it", async () => {
    invokeMock.mockResolvedValue(entry({ expires_at: "2026-12-31T23:59:59.999Z" }));
    await setMemoryExpiry("fact-1", "/ws/project", "2026-12-31");
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_set_expiry", {
      id: "fact-1",
      projectRoot: "/ws/project",
      expiresAt: "2026-12-31",
    });

    invokeMock.mockResolvedValue(entry());
    await setMemoryExpiry("fact-1", null, null);
    expect(invokeMock).toHaveBeenLastCalledWith("memory_studio_set_expiry", {
      id: "fact-1",
      projectRoot: null,
      expiresAt: null,
    });
  });

  it("mergeMemories forwards the id list, projectRoot, and text (null to join the originals)", async () => {
    invokeMock.mockResolvedValue(entry({ merged_from: ["a", "b"] }));
    await mergeMemories(["a", "b"], "/ws/project", "combined");
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_merge", {
      ids: ["a", "b"],
      projectRoot: "/ws/project",
      text: "combined",
    });

    await mergeMemories(["a", "b"], null, null);
    expect(invokeMock).toHaveBeenLastCalledWith("memory_studio_merge", {
      ids: ["a", "b"],
      projectRoot: null,
      text: null,
    });
  });

  it("unmergeMemories forwards id and projectRoot and resolves to the restored count", async () => {
    invokeMock.mockResolvedValue(2);
    await expect(unmergeMemories("merged-1", "/ws/project")).resolves.toBe(2);
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_unmerge", {
      id: "merged-1",
      projectRoot: "/ws/project",
    });
  });

  it("purgeExpiredMemories takes no arguments and resolves to the purged count", async () => {
    invokeMock.mockResolvedValue(3);
    await expect(purgeExpiredMemories()).resolves.toBe(3);
    expect(invokeMock).toHaveBeenCalledWith("memory_studio_purge_expired");
  });
});

describe("markMemoriesUsed", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("sends the ids to memory_mark_used", () => {
    invokeMock.mockResolvedValue(2);
    markMemoriesUsed(["a", "b"]);
    expect(invokeMock).toHaveBeenCalledWith("memory_mark_used", { ids: ["a", "b"] });
  });

  it("does nothing at all for an empty id list", () => {
    markMemoriesUsed([]);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("swallows a rejecting backend rather than failing the prompt build", async () => {
    invokeMock.mockRejectedValue(new Error("unknown command"));
    expect(() => markMemoriesUsed(["a"])).not.toThrow();
    // Flush the microtask queue: an unhandled rejection here would fail the
    // suites that stub `invoke` to reject unknown commands.
    await Promise.resolve();
    await Promise.resolve();
  });
});

describe("wouldReachPrompt (display only — mirrors list_impl's filter)", () => {
  // This asserts a TS predicate used for Memory Studio's badges, NOT that
  // anything was excluded from a real prompt. The prompt guarantee is proved
  // where the filter actually lives: memory.rs's
  // `expired_and_merge_retired_facts_are_excluded_from_list_impl` and
  // monkey-cli's
  // `an_expired_or_merged_away_fact_is_excluded_from_the_cli_system_prompt`.
  const NOW = "2026-06-01T00:00:00.000Z";

  it("keeps a plain enabled memory", () => {
    expect(wouldReachPrompt(entry(), NOW)).toBe(true);
  });

  it("excludes a disabled memory", () => {
    expect(wouldReachPrompt(entry({ enabled: false }), NOW)).toBe(false);
  });

  it("excludes an expired memory but not one whose expiry is still ahead", () => {
    expect(wouldReachPrompt(entry({ expires_at: "2026-01-01T00:00:00.000Z" }), NOW)).toBe(false);
    expect(wouldReachPrompt(entry({ expires_at: "2027-01-01T00:00:00.000Z" }), NOW)).toBe(true);
  });

  it("excludes a memory retired by a merge", () => {
    expect(wouldReachPrompt(entry({ retired_at: NOW, merged_into: "merged-1" }), NOW)).toBe(false);
  });

  it("keeps a pinned memory whose expiry has passed", () => {
    expect(wouldReachPrompt(entry({ pinned: true, expires_at: "2026-01-01T00:00:00.000Z" }), NOW)).toBe(true);
  });

  it("still excludes a pinned memory that is disabled or retired", () => {
    expect(wouldReachPrompt(entry({ pinned: true, enabled: false }), NOW)).toBe(false);
    expect(wouldReachPrompt(entry({ pinned: true, retired_at: NOW }), NOW)).toBe(false);
  });
});
