import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async () => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

import { findByCommand, hydratePrompts, selectPersonas, selectSnippets, usePromptStore, type PromptEntry } from "./promptStore";

/** Resets the singleton store to empty, as if freshly hydrated with no
 * saved prompts. */
function seed(entries: PromptEntry[] = []): void {
  usePromptStore.setState({ entries, defaultPersonaId: null, persistError: null });
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  seed();
});

describe("addEntry", () => {
  it("generates an id and timestamps, appends the entry, and persists", () => {
    const entry = usePromptStore
      .getState()
      .addEntry({ kind: "snippet", name: "Standup Update", command: "standup", content: "Wrote the daily update." });

    expect(entry.id).toBeTruthy();
    expect(entry.createdAt).toBe(entry.updatedAt);
    expect(usePromptStore.getState().entries).toEqual([entry]);
  });

  it("keeps an optional description undefined when not supplied", () => {
    const entry = usePromptStore.getState().addEntry({ kind: "snippet", name: "n", command: "n", content: "c" });
    expect(entry.description).toBeUndefined();
  });
});

describe("updateEntry", () => {
  it("patches fields and bumps updatedAt", () => {
    const entry = usePromptStore.getState().addEntry({ kind: "snippet", name: "Old", command: "old", content: "c" });
    const before = entry.updatedAt;

    usePromptStore.getState().updateEntry(entry.id, { name: "New" });

    const updated = usePromptStore.getState().entries.find((e) => e.id === entry.id);
    expect(updated?.name).toBe("New");
    expect(updated?.updatedAt).toBeGreaterThanOrEqual(before);
    expect(updated?.createdAt).toBe(entry.createdAt);
  });

  it("no-ops for an unknown id", () => {
    seed([{ id: "a", kind: "snippet", name: "A", command: "a", content: "c", createdAt: 1, updatedAt: 1 }]);
    usePromptStore.getState().updateEntry("ghost", { name: "New" });
    expect(usePromptStore.getState().entries[0].name).toBe("A");
  });
});

describe("removeEntry", () => {
  it("removes the entry", () => {
    const entry = usePromptStore.getState().addEntry({ kind: "snippet", name: "n", command: "n", content: "c" });
    usePromptStore.getState().removeEntry(entry.id);
    expect(usePromptStore.getState().entries).toEqual([]);
  });

  it("clears defaultPersonaId when it pointed at the removed entry", () => {
    const persona = usePromptStore.getState().addEntry({ kind: "persona", name: "P", command: "p", content: "c" });
    usePromptStore.setState({ defaultPersonaId: persona.id });

    usePromptStore.getState().removeEntry(persona.id);

    expect(usePromptStore.getState().defaultPersonaId).toBeNull();
  });

  it("no-ops for an unknown id", () => {
    seed([{ id: "a", kind: "snippet", name: "A", command: "a", content: "c", createdAt: 1, updatedAt: 1 }]);
    usePromptStore.getState().removeEntry("ghost");
    expect(usePromptStore.getState().entries).toHaveLength(1);
  });
});

describe("selectPersonas / selectSnippets", () => {
  it("partition entries by kind", () => {
    seed([
      { id: "1", kind: "persona", name: "Persona", command: "p", content: "c", createdAt: 1, updatedAt: 1 },
      { id: "2", kind: "snippet", name: "Snippet", command: "s", content: "c", createdAt: 1, updatedAt: 1 },
    ]);
    expect(selectPersonas(usePromptStore.getState()).map((e) => e.id)).toEqual(["1"]);
    expect(selectSnippets(usePromptStore.getState()).map((e) => e.id)).toEqual(["2"]);
  });
});

describe("findByCommand", () => {
  it("finds the entry with a matching command", () => {
    const entries: PromptEntry[] = [{ id: "1", kind: "snippet", name: "N", command: "greet", content: "c", createdAt: 1, updatedAt: 1 }];
    expect(findByCommand(entries, "greet")?.id).toBe("1");
    expect(findByCommand(entries, "missing")).toBeUndefined();
  });
});

describe("hydratePrompts", () => {
  it("loads and normalizes persisted entries from prompts_load", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        version: 1,
        entries: [{ id: "x", kind: "snippet", name: "X", command: "x", content: "hello" }],
        defaultPersonaId: null,
      })
    );

    await hydratePrompts();

    expect(invokeMock).toHaveBeenCalledWith("prompts_load");
    expect(usePromptStore.getState().entries).toHaveLength(1);
    expect(usePromptStore.getState().entries[0].name).toBe("X");
  });

  it("keeps an empty library when nothing has been persisted yet", async () => {
    invokeMock.mockImplementationOnce(async () => null);
    await hydratePrompts();
    expect(usePromptStore.getState().entries).toEqual([]);
  });

  it("surfaces a read failure via persistError instead of throwing", async () => {
    invokeMock.mockImplementationOnce(async () => {
      throw new Error("disk on fire");
    });
    await hydratePrompts();
    expect(usePromptStore.getState().persistError).toBe("disk on fire");
  });

  /** Canonical fixture also parsed by `prompts.rs`'s
   * `prompt_entry_deserializes_canonical_fixture` Rust test — pins the
   * TS<->Rust schema against drift, since `lm-cli` reads `PromptEntry`
   * directly without going through this store at all. */
  it("normalizes the same canonical entry the Rust unit test pins", async () => {
    const canonicalEntry = {
      id: "11111111-1111-4111-8111-111111111111",
      kind: "persona",
      name: "Code Reviewer",
      command: "code-reviewer",
      content: "You are a meticulous code reviewer.",
      description: "Reviews diffs for bugs",
      createdAt: 1700000000000,
      updatedAt: 1700000000000,
    };
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({ version: 1, entries: [canonicalEntry], defaultPersonaId: null })
    );

    await hydratePrompts();

    expect(usePromptStore.getState().entries).toEqual([canonicalEntry]);
  });

  it("fills in defaults for a malformed entry instead of dropping it", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({ version: 1, entries: [{ kind: "not-a-real-kind" }], defaultPersonaId: null })
    );

    await hydratePrompts();

    const entries = usePromptStore.getState().entries;
    expect(entries).toHaveLength(1);
    expect(entries[0].id).toBeTruthy();
    expect(entries[0].kind).toBe("snippet"); // unrecognized kind falls back to "snippet"
    expect(entries[0].name).toBe("Untitled");
    expect(entries[0].command).toBe("");
    expect(entries[0].content).toBe("");
  });
});
