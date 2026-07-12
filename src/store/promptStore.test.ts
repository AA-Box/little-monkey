import { beforeEach, describe, expect, it, vi } from "vitest";
// Shared with `prompts.rs`'s `prompt_entry_deserializes_canonical_fixture`
// Rust test (which reads the same file via `include_str!`) — see the note
// on the `it(...)` below that uses this.
import canonicalEntryFixture from "../../src-tauri/fixtures/prompt-entry.canonical.json";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

import {
  findByCommand,
  hydratePrompts,
  ImportParseError,
  parseImportPayload,
  selectPersonas,
  selectSnippets,
  usePromptStore,
  type PromptEntry,
} from "./promptStore";

/** Resets the singleton store to empty, as if freshly hydrated with no
 * saved prompts. */
function seed(entries: PromptEntry[] = []): void {
  // hasSeededDefaults: true keeps these direct-state tests from ever
  // exercising the starter-persona seed path — that's covered separately in
  // the "hydratePrompts / starter personas" tests below, which manipulate it
  // explicitly.
  usePromptStore.setState({ entries, defaultPersonaId: null, hasSeededDefaults: true, persistError: null });
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

  it("clears defaultPersonaId when the default persona's kind is edited away from persona", () => {
    const persona = usePromptStore.getState().addEntry({ kind: "persona", name: "P", command: "p", content: "c" });
    usePromptStore.getState().setDefaultPersona(persona.id);

    usePromptStore.getState().updateEntry(persona.id, { kind: "snippet" });

    expect(usePromptStore.getState().defaultPersonaId).toBeNull();
    expect(usePromptStore.getState().entries.find((e) => e.id === persona.id)?.kind).toBe("snippet");
  });

  it("leaves defaultPersonaId alone when patching an entry that isn't the default", () => {
    const persona = usePromptStore.getState().addEntry({ kind: "persona", name: "P", command: "p", content: "c" });
    const other = usePromptStore.getState().addEntry({ kind: "persona", name: "Q", command: "q", content: "c" });
    usePromptStore.getState().setDefaultPersona(persona.id);

    usePromptStore.getState().updateEntry(other.id, { kind: "snippet" });

    expect(usePromptStore.getState().defaultPersonaId).toBe(persona.id);
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
        hasSeededDefaults: true,
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
    // beforeEach's seed() already marked hasSeededDefaults true, so this
    // no-file case stays empty rather than picking up the starter personas
    // (that seeding path is covered on its own below).
    expect(usePromptStore.getState().entries).toEqual([]);
  });

  it("surfaces a read failure via persistError instead of throwing", async () => {
    invokeMock.mockImplementationOnce(async () => {
      throw new Error("disk on fire");
    });
    await hydratePrompts();
    expect(usePromptStore.getState().persistError).toBe("disk on fire");
  });

  /** Reads the exact same file `prompts.rs`'s
   * `prompt_entry_deserializes_canonical_fixture` Rust test reads via
   * `include_str!` — a single shared fixture, not two independently
   * hand-typed literals, is what actually pins the TS<->Rust schema against
   * drift, since `lm-cli` reads `PromptEntry` directly without going
   * through this store at all. */
  it("normalizes the same canonical entry the Rust unit test pins", async () => {
    const canonicalEntry = canonicalEntryFixture;
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({ version: 1, entries: [canonicalEntry], defaultPersonaId: null, hasSeededDefaults: true })
    );

    await hydratePrompts();

    expect(usePromptStore.getState().entries).toEqual([canonicalEntry]);
  });

  it("fills in defaults for a malformed entry instead of dropping it", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({ version: 1, entries: [{ kind: "not-a-real-kind" }], defaultPersonaId: null, hasSeededDefaults: true })
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

describe("hydratePrompts / starter personas", () => {
  it("seeds the starter personas on the very first hydration (no file yet)", async () => {
    usePromptStore.setState({ entries: [], defaultPersonaId: null, hasSeededDefaults: false, persistError: null });
    invokeMock.mockImplementationOnce(async () => null); // prompts_load: no file

    await hydratePrompts();

    const state = usePromptStore.getState();
    expect(state.hasSeededDefaults).toBe(true);
    expect(state.entries.length).toBeGreaterThanOrEqual(2);
    expect(state.entries.every((e) => e.kind === "persona")).toBe(true);
  });

  it("never re-seeds once hasSeededDefaults is true, even if the library is empty", async () => {
    usePromptStore.setState({ entries: [], defaultPersonaId: null, hasSeededDefaults: true, persistError: null });
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({ version: 1, entries: [], defaultPersonaId: null, hasSeededDefaults: true })
    );

    await hydratePrompts();

    expect(usePromptStore.getState().entries).toEqual([]);
  });

  it("avoids command collisions between seeded personas and existing entries", async () => {
    usePromptStore.setState({ entries: [], defaultPersonaId: null, hasSeededDefaults: false, persistError: null });
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        version: 1,
        entries: [{ id: "x", kind: "snippet", name: "X", command: "code-reviewer", content: "c" }],
        defaultPersonaId: null,
        hasSeededDefaults: false,
      })
    );
    invokeMock.mockImplementationOnce(async () => undefined);

    await hydratePrompts();

    const commands = usePromptStore.getState().entries.map((e) => e.command);
    expect(new Set(commands).size).toBe(commands.length);
  });
});

describe("setDefaultPersona", () => {
  it("sets the default when the id names a persona entry", () => {
    const persona = usePromptStore.getState().addEntry({ kind: "persona", name: "P", command: "p", content: "c" });
    usePromptStore.getState().setDefaultPersona(persona.id);
    expect(usePromptStore.getState().defaultPersonaId).toBe(persona.id);
  });

  it("clears the default when passed null", () => {
    const persona = usePromptStore.getState().addEntry({ kind: "persona", name: "P", command: "p", content: "c" });
    usePromptStore.getState().setDefaultPersona(persona.id);
    usePromptStore.getState().setDefaultPersona(null);
    expect(usePromptStore.getState().defaultPersonaId).toBeNull();
  });

  it("no-ops for an id that isn't a persona entry", () => {
    const snippet = usePromptStore.getState().addEntry({ kind: "snippet", name: "S", command: "s", content: "c" });
    usePromptStore.getState().setDefaultPersona(snippet.id);
    expect(usePromptStore.getState().defaultPersonaId).toBeNull();
  });

  it("no-ops for an unknown id", () => {
    usePromptStore.getState().setDefaultPersona("ghost");
    expect(usePromptStore.getState().defaultPersonaId).toBeNull();
  });
});

describe("importEntries", () => {
  it("adds every incoming entry with a fresh id and timestamps", () => {
    const incoming: PromptEntry[] = [
      { id: "external-1", kind: "snippet", name: "Standup", command: "standup", content: "c", createdAt: 1, updatedAt: 1 },
      { id: "external-2", kind: "persona", name: "Reviewer", command: "reviewer", content: "c", createdAt: 1, updatedAt: 1 },
    ];

    const added = usePromptStore.getState().importEntries(incoming);

    expect(added).toBe(2);
    const entries = usePromptStore.getState().entries;
    expect(entries).toHaveLength(2);
    expect(entries.map((e) => e.command).sort()).toEqual(["reviewer", "standup"]);
    // Ids are regenerated, never carried over from the source file.
    expect(entries.every((e) => e.id !== "external-1" && e.id !== "external-2")).toBe(true);
  });

  it("renames the imported entry's command on collision instead of overwriting the existing one", () => {
    seed([{ id: "1", kind: "snippet", name: "Existing", command: "review", content: "old", createdAt: 1, updatedAt: 1 }]);

    const added = usePromptStore
      .getState()
      .importEntries([{ id: "x", kind: "snippet", name: "Incoming", command: "review", content: "new", createdAt: 1, updatedAt: 1 }]);

    expect(added).toBe(1);
    const entries = usePromptStore.getState().entries;
    expect(entries).toHaveLength(2);
    const existing = entries.find((e) => e.name === "Existing");
    const imported = entries.find((e) => e.name === "Incoming");
    expect(existing?.command).toBe("review");
    expect(existing?.content).toBe("old"); // never overwritten
    expect(imported?.command).toBe("review-2");
  });

  it("uniquifies commands within the same import batch, not just against the existing library", () => {
    const added = usePromptStore.getState().importEntries([
      { id: "a", kind: "snippet", name: "A", command: "dup", content: "1", createdAt: 1, updatedAt: 1 },
      { id: "b", kind: "snippet", name: "B", command: "dup", content: "2", createdAt: 1, updatedAt: 1 },
    ]);

    expect(added).toBe(2);
    const commands = usePromptStore.getState().entries.map((e) => e.command).sort();
    expect(commands).toEqual(["dup", "dup-2"]);
  });

  it("terminates (rather than hanging) when a colliding command is already exactly 32 characters", () => {
    // Regression test: appending "-2", "-3", ... to a 32-char base and then
    // truncating the *whole* string back to 32 chars used to always cut off
    // the entire suffix, reproducing the same 32-char string forever and
    // hanging `uniqueCommand`'s retry loop. `withSuffix` now shortens the
    // base instead, so this must return promptly with a genuinely different
    // command.
    const base = "a".repeat(32);
    seed([{ id: "1", kind: "snippet", name: "Existing", command: base, content: "old", createdAt: 1, updatedAt: 1 }]);

    const added = usePromptStore
      .getState()
      .importEntries([{ id: "x", kind: "snippet", name: "Incoming", command: base, content: "new", createdAt: 1, updatedAt: 1 }]);

    expect(added).toBe(1);
    const entries = usePromptStore.getState().entries;
    const imported = entries.find((e) => e.name === "Incoming");
    expect(imported?.command).not.toBe(base);
    expect(imported?.command.length).toBeLessThanOrEqual(32);
  });

  it("caps an incoming command at 32 characters even when it doesn't collide with anything", () => {
    const longCommand = "a".repeat(50);

    const added = usePromptStore
      .getState()
      .importEntries([{ id: "x", kind: "snippet", name: "Incoming", command: longCommand, content: "c", createdAt: 1, updatedAt: 1 }]);

    expect(added).toBe(1);
    expect(usePromptStore.getState().entries[0].command).toBe("a".repeat(32));
  });
});

describe("parseImportPayload", () => {
  it("throws ImportParseError for invalid JSON", () => {
    expect(() => parseImportPayload("not json{")).toThrow(ImportParseError);
  });

  it("throws ImportParseError for valid JSON in an unrecognized shape", () => {
    expect(() => parseImportPayload(JSON.stringify({ foo: "bar" }))).toThrow(ImportParseError);
  });

  it("parses this app's own export shape", () => {
    const payload = JSON.stringify({
      version: 1,
      entries: [{ id: "1", kind: "snippet", name: "N", command: "n", content: "c", createdAt: 1, updatedAt: 1 }],
    });
    const entries = parseImportPayload(payload);
    expect(entries).toHaveLength(1);
    expect(entries[0].name).toBe("N");
  });

  it("adapts Cherry Studio's exported agents JSON shape into personas", () => {
    const payload = JSON.stringify([
      { name: "Code Reviewer", prompt: "You are a meticulous code reviewer.", description: "Reviews diffs" },
      { name: "Rust Mentor", prompt: "You teach idiomatic Rust." },
    ]);

    const entries = parseImportPayload(payload);

    expect(entries).toHaveLength(2);
    expect(entries.every((e) => e.kind === "persona")).toBe(true);
    expect(entries[0].name).toBe("Code Reviewer");
    expect(entries[0].content).toBe("You are a meticulous code reviewer.");
    expect(entries[0].command).toBe("code-reviewer");
    expect(entries[0].description).toBe("Reviews diffs");
    expect(entries[1].description).toBeUndefined();
  });

  it("does not misidentify an arbitrary array as Cherry Studio's shape", () => {
    expect(() => parseImportPayload(JSON.stringify([{ foo: "bar" }, 1, "two"]))).toThrow(ImportParseError);
  });
});
