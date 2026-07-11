import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

/** Emitted by the backend after every successful `prompts_save`, with the
 * saving window's label as payload (see src-tauri/src/prompts.rs). Other
 * windows rehydrate from the file on it so two open windows stop clobbering
 * each other's prompt library — same mechanism as `sessionStore.ts`. */
const PROMPTS_CHANGED_EVENT = "prompts://changed";

/** How long after the last mutation the debounced file write fires — same
 * value/rationale as `sessionStore.ts`'s `PERSIST_DEBOUNCE_MS`. */
const PERSIST_DEBOUNCE_MS = 400;

export type PromptKind = "persona" | "snippet";

/**
 * A saved prompt-library entry: a persona (a system-prompt extension that
 * shapes the whole session — wired into the agent loop in slice 2) or a
 * snippet (reusable text inserted into the chat composer). Mirrors the Rust
 * `PromptEntry` struct (src-tauri/src/prompts.rs) field-for-field —
 * `camelCase` on the wire, matching that struct's `#[serde(rename_all =
 * "camelCase")]`.
 */
export interface PromptEntry {
  id: string;
  kind: PromptKind;
  /** Display name, e.g. "Code Reviewer". */
  name: string;
  /** Slash-trigger slug, `/^[a-z0-9-]{1,32}$/`, unique across the library. */
  command: string;
  /** The prompt/snippet text. */
  content: string;
  /** One-liner shown in the autocomplete row. */
  description?: string;
  createdAt: number;
  updatedAt: number;
}

/** Fields a caller supplies to `addEntry` — the store fills in `id` and the
 * timestamps. */
export interface NewPromptInput {
  kind: PromptKind;
  name: string;
  command: string;
  content: string;
  description?: string;
}

/** Fields `updateEntry` may patch — `id`/`createdAt` never change after
 * creation. */
export type PromptEntryPatch = Partial<Omit<PromptEntry, "id" | "createdAt" | "updatedAt">>;

interface PersistedShape {
  version: 1;
  entries: PromptEntry[];
  defaultPersonaId: string | null;
  /** Whether the built-in starter personas (see `STARTER_PERSONAS`) have
   * already been seeded once. Tracked in the persisted blob itself — NOT
   * derived from `entries.length === 0` — so a user who deletes every
   * starter persona (or every entry) never has them silently reappear on
   * the next launch. Absent/`false` on any blob written before this field
   * existed, which is exactly the "never seeded yet" state those blobs
   * should get. */
  hasSeededDefaults: boolean;
}

/**
 * The prompt/persona library: personas and snippets share one table since
 * they share every behavior except what "invoke" means (see
 * `src/components/Chat/SlashCommandAutocomplete.tsx`). Persistence lives in
 * a file in the app data directory (see src-tauri/src/prompts.rs), written
 * debounced after every mutation — the same file-based pattern
 * `sessionStore.ts` uses, just without that store's split-pane/legacy-
 * localStorage complexity (this feature has no earlier persisted form to
 * migrate from). Call `hydratePrompts()` once at startup (main.tsx does,
 * alongside `hydrateSessions()`) before the first render.
 */
export interface PromptStore {
  /** All saved prompt-library entries, in no particular order (sort/filter
   * at render time via `selectPersonas`/`selectSnippets`). */
  entries: PromptEntry[];
  /** The persona applied to new sessions by default (see `sessionStore.ts`'s
   * `createSession`), or `null` for none. Set via the "Set as default" action
   * on a persona row in `PromptLibraryPanel.tsx`, or `setDefaultPersona`. */
  defaultPersonaId: string | null;
  /** Whether the built-in starter personas have already been seeded once —
   * see `STARTER_PERSONAS`. Internal bookkeeping, not surfaced in the UI. */
  hasSeededDefaults: boolean;
  /** Last file-persistence failure, surfaced in the UI instead of silently
   * dropping a save; cleared by the next successful save. */
  persistError: string | null;
  /** Create a new entry and persist it. Returns the created entry (its
   * generated `id` is otherwise unobservable until the next render). */
  addEntry: (input: NewPromptInput) => PromptEntry;
  /** Patch an existing entry by id; no-ops if it doesn't exist. Bumps
   * `updatedAt`. */
  updateEntry: (id: string, patch: PromptEntryPatch) => void;
  /** Remove an entry by id; no-ops if it doesn't exist. Clears
   * `defaultPersonaId` if it pointed at the removed entry. */
  removeEntry: (id: string) => void;
  /** Merge externally-sourced entries (parsed by `parseImportPayload`) into
   * the library. Matches by `command`: a colliding command is renamed
   * (`"review"` -> `"review-2"`) rather than overwriting the existing entry,
   * so Import only ever adds rows — it can never silently clobber a saved
   * prompt. Every imported entry gets a fresh `id`/timestamps, ignoring
   * whatever the source file had. Returns the number of entries added. */
  importEntries: (incoming: PromptEntry[]) => number;
  /** Serializes the current library to the portable JSON shape written by
   * `prompts_write_external` — the Settings "Prompts" tab's Export button.
   * Deliberately omits `defaultPersonaId`: it's a local preference, not
   * something that should travel into a teammate's imported copy. */
  exportPayload: () => string;
  /** Sets (or, passed the id already set, clears) the persona new sessions
   * start on — see `sessionStore.ts`'s `createSession`. No-ops if `id`
   * doesn't name a persona entry. Passing `null` always clears it. */
  setDefaultPersona: (id: string | null) => void;
}

/** 2-3 small built-in personas seeded once, on the very first hydration ever
 * (see `hasSeededDefaults` on `PersistedShape`) — enough to make the "/"
 * popup and the Prompts tab feel populated on a fresh install, without
 * pretending to be a curated prompt marketplace. English-only: there is no
 * ergonomic way to call `useT()` from `hydratePrompts()` (it runs before
 * React mounts, in `main.tsx`), and these are ordinary editable/deletable
 * library entries afterward, not chrome — a deliberately small deviation
 * from full i18n coverage for this one piece of seed *data*. */
const STARTER_PERSONAS: readonly Omit<NewPromptInput, "kind">[] = [
  {
    name: "Code Reviewer",
    command: "code-reviewer",
    description: "Reviews code for correctness, clarity, and maintainability.",
    content:
      "You are a meticulous code reviewer. Point out correctness bugs, unclear naming, missed edge cases, and simplification opportunities. Be direct and specific — cite the exact line or snippet. Prefer small, targeted suggestions over rewrites.",
  },
  {
    name: "Concise Explainer",
    command: "concise-explainer",
    description: "Explains things plainly, in as few words as needed.",
    content:
      "Explain things as plainly and concisely as possible. Prefer short sentences and concrete examples over abstract prose. Skip preamble and caveats unless they change the answer. If something is genuinely uncertain, say so in one sentence rather than hedging throughout.",
  },
  {
    name: "Brainstorm Partner",
    command: "brainstorm-partner",
    description: "Generates a wide range of options before narrowing down.",
    content:
      "Act as a brainstorming partner. When given a problem, first generate a diverse range of options or angles before evaluating any of them. Favor breadth over premature convergence, and clearly separate 'ideas' from 'recommendation' when you do narrow down.",
  },
];

/** Derives a candidate command slug from a display name — lowercased,
 * non-alphanumerics collapsed to single hyphens, trimmed of leading/
 * trailing hyphens, capped at the `command` field's 32-character limit.
 * Shared by `PromptLibraryPanel.tsx`'s auto-slug-as-you-type behavior and
 * the Cherry Studio import adapter below, so both derive commands the same
 * way. */
export function slugify(name: string): string {
  return name
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 32);
}

/** Appends `-2`, `-3`, ... to `command` until it no longer collides with
 * anything in `taken`, so a batch import never silently overwrites an
 * existing entry's command. Truncates back to 32 characters (the
 * `command` field's limit) after appending the suffix. Returns `command`
 * unchanged if it doesn't collide. */
function uniqueCommand(command: string, taken: Set<string>): string {
  const base = command.length > 0 ? command : "prompt";
  if (!taken.has(base)) return base;
  let n = 2;
  let candidate = `${base}-${n}`.slice(0, 32);
  while (taken.has(candidate)) {
    n += 1;
    candidate = `${base}-${n}`.slice(0, 32);
  }
  return candidate;
}

/** Raised by `parseImportPayload` for a file that is either invalid JSON or
 * doesn't match any recognized shape (this app's own export, or Cherry
 * Studio's agents export) — a distinct type so the Settings UI can show a
 * clear "couldn't read that file" message instead of an unhandled crash. */
export class ImportParseError extends Error {}

/** Cherry Studio's exported "agents" JSON is a bare array of
 * `{ name, prompt, description }` objects — no `version`/`entries`
 * wrapper. Recognized structurally (every element has string `name` and
 * `prompt` fields) rather than by a format flag, since the file carries no
 * such flag itself. Every agent becomes a persona: Cherry Studio's
 * `prompt` is exactly a system-prompt extension, which is what
 * `kind: "persona"` means here. Returns `null` (not an error) when `raw`
 * doesn't match, so the caller can fall through to "unrecognized format".
 */
function adaptCherryStudioAgents(raw: unknown[]): PromptEntry[] | null {
  if (raw.length === 0) return null;
  const isCherryShape = raw.every(
    (item): item is { name: string; prompt: string; description?: unknown } =>
      !!item &&
      typeof item === "object" &&
      typeof (item as Record<string, unknown>).name === "string" &&
      typeof (item as Record<string, unknown>).prompt === "string",
  );
  if (!isCherryShape) return null;

  const now = Date.now();
  return raw.map((item) => {
    const agent = item as { name: string; prompt: string; description?: unknown };
    return {
      id: crypto.randomUUID(),
      kind: "persona" as const,
      name: agent.name,
      command: slugify(agent.name),
      content: agent.prompt,
      description: typeof agent.description === "string" && agent.description.length > 0 ? agent.description : undefined,
      createdAt: now,
      updatedAt: now,
    };
  });
}

/** Parses a file picked via the Import button into normalized entries,
 * without touching the store — the Settings UI shows a count preview
 * (`entries.length`) and asks for confirmation before calling
 * `importEntries` with the result. Recognizes two shapes: this app's own
 * export (`{ version, entries }`, reusing `normalizeEntry`'s leniency) and
 * Cherry Studio's exported agents array (see `adaptCherryStudioAgents`).
 * Throws `ImportParseError` with a user-facing message for invalid JSON or
 * an unrecognized shape — callers should catch this specifically rather
 * than let a malformed file crash the import flow. */
export function parseImportPayload(raw: string): PromptEntry[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new ImportParseError("That file isn't valid JSON.");
  }

  if (parsed && typeof parsed === "object" && Array.isArray((parsed as { entries?: unknown }).entries)) {
    return ((parsed as { entries: unknown[] }).entries)
      .filter((e): e is Partial<PromptEntry> => !!e && typeof e === "object")
      .map(normalizeEntry);
  }

  if (Array.isArray(parsed)) {
    const adapted = adaptCherryStudioAgents(parsed);
    if (adapted) return adapted;
  }

  throw new ImportParseError("Unrecognized prompt library file format.");
}

/** Fills in defaults for a possibly hand-edited or partially malformed
 * persisted entry, so it never corrupts the rest of the library — mirrors
 * `sessionStore.ts`'s `normalizeSession`. Unlike `normalizeMessage` (which
 * drops unrecognizable messages), a raw object here always yields a usable
 * entry: an id is generated if absent, and every other field falls back to
 * an empty/derived default rather than causing the whole entry to be
 * dropped. */
function normalizeEntry(raw: Partial<PromptEntry>): PromptEntry {
  const now = Date.now();
  const createdAt = typeof raw.createdAt === "number" ? raw.createdAt : now;
  return {
    id: typeof raw.id === "string" && raw.id.length > 0 ? raw.id : crypto.randomUUID(),
    kind: raw.kind === "persona" ? "persona" : "snippet",
    name: typeof raw.name === "string" && raw.name.trim().length > 0 ? raw.name : "Untitled",
    command: typeof raw.command === "string" ? raw.command : "",
    content: typeof raw.content === "string" ? raw.content : "",
    description: typeof raw.description === "string" && raw.description.length > 0 ? raw.description : undefined,
    createdAt,
    updatedAt: typeof raw.updatedAt === "number" ? raw.updatedAt : createdAt,
  };
}

/** Parses and validates a persisted `{ version, entries, defaultPersonaId }`
 * JSON blob. Returns `null` for anything absent, corrupt, or missing an
 * `entries` array. */
function parsePersisted(raw: string | null): PersistedShape | null {
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as
      | { version?: unknown; entries?: unknown; defaultPersonaId?: unknown; hasSeededDefaults?: unknown }
      | null;
    if (!parsed || !Array.isArray(parsed.entries)) return null;
    return {
      version: 1,
      entries: (parsed.entries as unknown[])
        .filter((e): e is Partial<PromptEntry> => !!e && typeof e === "object")
        .map(normalizeEntry),
      defaultPersonaId: typeof parsed.defaultPersonaId === "string" ? parsed.defaultPersonaId : null,
      // Absent on any blob written before this field existed — exactly the
      // "never seeded yet" state those blobs should get.
      hasSeededDefaults: parsed.hasSeededDefaults === true,
    };
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Debounced file persistence — identical shape to `sessionStore.ts`'s.
// ---------------------------------------------------------------------------

let persistTimer: ReturnType<typeof setTimeout> | null = null;
let pendingPayload: string | null = null;

function flushPersist(): void {
  if (persistTimer !== null) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  const payload = pendingPayload;
  pendingPayload = null;
  if (payload === null) return;

  invoke("prompts_save", { payload })
    .then(() => {
      if (usePromptStore.getState().persistError !== null) {
        usePromptStore.setState({ persistError: null });
      }
    })
    .catch((err: unknown) => {
      usePromptStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    });
}

function persist(entries: PromptEntry[], defaultPersonaId: string | null, hasSeededDefaults: boolean): void {
  try {
    pendingPayload = JSON.stringify({ version: 1, entries, defaultPersonaId, hasSeededDefaults });
  } catch (err) {
    usePromptStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    return;
  }
  if (persistTimer === null) {
    persistTimer = setTimeout(flushPersist, PERSIST_DEBOUNCE_MS);
  }
}

// Best-effort flush of a pending (debounced) write when the window goes away
// mid-debounce — mirrors `sessionStore.ts`.
if (typeof window !== "undefined") {
  window.addEventListener("beforeunload", flushPersist);
}

/** Re-reads the saved blob after ANOTHER window persisted it, replacing this
 * window's entries wholesale. Read errors are ignored: the current
 * in-memory state stays, and this window's own next save will surface any
 * real persistence problem — same stance as `sessionStore.ts`'s
 * `rehydrateFromFile`. */
async function rehydrateFromFile(): Promise<void> {
  let fromFile: PersistedShape | null = null;
  try {
    const raw = await invoke<string | null>("prompts_load");
    fromFile = parsePersisted(raw);
  } catch {
    return;
  }
  if (!fromFile) return;
  usePromptStore.setState({
    entries: fromFile.entries,
    defaultPersonaId: fromFile.defaultPersonaId,
    hasSeededDefaults: fromFile.hasSeededDefaults,
  });
}

/** Starts listening for other windows' saves. Called once per window from
 * `hydratePrompts`. */
async function listenForOtherWindowSaves(): Promise<void> {
  const ownLabel = getCurrentWindow().label;
  await listen<string>(PROMPTS_CHANGED_EVENT, (event) => {
    // Our own save — the store already reflects it.
    if (event.payload === ownLabel) return;
    // A local mutation is still waiting in the debounce window: rehydrating
    // now would visibly discard it. Skip — our imminent flush notifies the
    // other window instead, and subsequent events converge us onto whoever
    // saved last.
    if (pendingPayload !== null) return;
    void rehydrateFromFile();
  });
}

/**
 * Loads the persisted prompt library from the app-data file (see
 * src-tauri/src/prompts.rs) into the store. Must be awaited before the
 * first render (main.tsx does, alongside `hydrateSessions()`) so a user
 * action can never race the hydrate and get overwritten by it. Also
 * subscribes this window to other windows' saves so multi-window use stays
 * in sync.
 */
export async function hydratePrompts(): Promise<void> {
  // Subscribe before the initial load so a save landing in another window
  // during hydration isn't missed.
  void listenForOtherWindowSaves().catch((err: unknown) => {
    console.error("Failed to subscribe to cross-window prompt-library sync", err);
  });

  let fromFile: PersistedShape | null = null;
  try {
    const raw = await invoke<string | null>("prompts_load");
    fromFile = parsePersisted(raw);
  } catch (err) {
    // Read failure (not "file missing" — that returns null). Keep the empty
    // in-memory library and surface the error; the file on disk is left
    // untouched until the user actually does something worth saving.
    usePromptStore.setState({ persistError: err instanceof Error ? err.message : String(err) });
    return;
  }

  if (fromFile) {
    usePromptStore.setState({
      entries: fromFile.entries,
      defaultPersonaId: fromFile.defaultPersonaId,
      hasSeededDefaults: fromFile.hasSeededDefaults,
    });
  }
  // No file yet (first run) — keep the empty initial state. Unlike
  // `sessionStore.ts` there's no legacy localStorage blob to migrate from:
  // this feature never persisted anywhere before this file existed.

  // First-ever hydration (this device has never seeded the starter personas,
  // whether because the file is brand new or predates this field): add them
  // once and persist immediately so a fast quit-after-launch doesn't lose the
  // seed. Never re-runs afterward, even if the user deletes every entry —
  // `hasSeededDefaults` is what's checked, not `entries.length === 0`.
  const stateAfterLoad = usePromptStore.getState();
  if (!stateAfterLoad.hasSeededDefaults) {
    const now = Date.now();
    const taken = new Set(stateAfterLoad.entries.map((e) => e.command));
    const seeded: PromptEntry[] = STARTER_PERSONAS.map((starter) => {
      const command = uniqueCommand(starter.command, taken);
      taken.add(command);
      return {
        id: crypto.randomUUID(),
        kind: "persona",
        name: starter.name,
        command,
        content: starter.content,
        description: starter.description,
        createdAt: now,
        updatedAt: now,
      };
    });
    const entries = [...stateAfterLoad.entries, ...seeded];
    usePromptStore.setState({ entries, hasSeededDefaults: true });
    persist(entries, stateAfterLoad.defaultPersonaId, true);
  }
}

export const usePromptStore = create<PromptStore>((set, get) => ({
  entries: [],
  defaultPersonaId: null,
  hasSeededDefaults: false,
  persistError: null,

  addEntry: (input) => {
    const now = Date.now();
    const entry: PromptEntry = {
      id: crypto.randomUUID(),
      kind: input.kind,
      name: input.name,
      command: input.command,
      content: input.content,
      description: input.description,
      createdAt: now,
      updatedAt: now,
    };
    set((state) => {
      const entries = [...state.entries, entry];
      persist(entries, state.defaultPersonaId, state.hasSeededDefaults);
      return { entries };
    });
    return entry;
  },

  updateEntry: (id, patch) => {
    set((state) => {
      const target = state.entries.find((e) => e.id === id);
      if (!target) return state;
      const entries = state.entries.map((e) => (e.id === id ? { ...e, ...patch, updatedAt: Date.now() } : e));
      persist(entries, state.defaultPersonaId, state.hasSeededDefaults);
      return { entries };
    });
  },

  removeEntry: (id) => {
    set((state) => {
      if (!state.entries.some((e) => e.id === id)) return state;
      const entries = state.entries.filter((e) => e.id !== id);
      const defaultPersonaId = state.defaultPersonaId === id ? null : state.defaultPersonaId;
      persist(entries, defaultPersonaId, state.hasSeededDefaults);
      return { entries, defaultPersonaId };
    });
  },

  importEntries: (incoming) => {
    let added = 0;
    set((state) => {
      const taken = new Set(state.entries.map((e) => e.command));
      const now = Date.now();
      const imported = incoming.map((item) => {
        const command = uniqueCommand(item.command, taken);
        taken.add(command);
        return { ...item, id: crypto.randomUUID(), command, createdAt: now, updatedAt: now };
      });
      added = imported.length;
      const entries = [...state.entries, ...imported];
      persist(entries, state.defaultPersonaId, state.hasSeededDefaults);
      return { entries };
    });
    return added;
  },

  exportPayload: () => JSON.stringify({ version: 1, entries: get().entries }, null, 2),

  setDefaultPersona: (id) => {
    set((state) => {
      if (id !== null && !state.entries.some((e) => e.id === id && e.kind === "persona")) return state;
      if (state.defaultPersonaId === id) return state;
      persist(state.entries, id, state.hasSeededDefaults);
      return { defaultPersonaId: id };
    });
  },
}));

/** Zustand selector: every saved persona, in library order. */
export function selectPersonas(state: PromptStore): PromptEntry[] {
  return state.entries.filter((e) => e.kind === "persona");
}

/** Zustand selector: every saved snippet, in library order. */
export function selectSnippets(state: PromptStore): PromptEntry[] {
  return state.entries.filter((e) => e.kind === "snippet");
}

/** Finds the entry (if any) whose `command` matches exactly — used for
 * slash-command exact lookup and for the create/edit form's uniqueness
 * validation (exclude the entry being edited by checking `.id` on the
 * result). */
export function findByCommand(entries: PromptEntry[], command: string): PromptEntry | undefined {
  return entries.find((e) => e.command === command);
}
