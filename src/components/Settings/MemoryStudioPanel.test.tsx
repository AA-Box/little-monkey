// @vitest-environment jsdom
/**
 * Memory Studio's panel-side branching, driven the way a user drives it.
 *
 * Three of these are honesty tests, not cosmetics. A pinned memory that
 * still holds an expiry date must say that the date is retained and applies
 * again on unpin — and must keep Clear expiry reachable — because
 * `set_pinned_impl` never clears `expires_at`, so "pinned memories never
 * expire" beside a stored date would be a promise the store breaks the
 * moment the memory is unpinned. A merge selection the backend would refuse
 * must say why here rather than as an error after the click. And a row that
 * cannot be a merge parent must not offer the checkbox at all.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(), save: vi.fn() }));
vi.mock("@tauri-apps/plugin-fs", () => ({ writeTextFile: vi.fn() }));

import { MemoryStudioPanel } from "./MemoryStudioPanel";
import type { MemoryEntry } from "../../lib/memoryStudio";

function entry(over: Partial<MemoryEntry>): MemoryEntry {
  return {
    id: "m-1",
    text: "a remembered thing",
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
    scope: "global",
    project_root: null,
    ...over,
  };
}

const PINNED_WITH_EXPIRY = entry({
  id: "pinned-1",
  text: "pinned but still carrying a date",
  pinned: true,
  expires_at: "2026-12-31T23:59:59.999Z",
});
const PLAIN_GLOBAL = entry({ id: "global-1", text: "a plain global memory" });
const PLAIN_PROJECT = entry({
  id: "project-1",
  text: "a plain project memory",
  scope: "project",
  project_root: "/ws/project",
});
const RETIRED = entry({
  id: "retired-1",
  text: "an original retired by a merge",
  retired_at: "2026-02-01T00:00:00.000Z",
  merged_into: "merged-1",
});
/** A merge that was itself merged into a newer one: `unmerge_impl` refuses
 * its undo until the newer merge is undone. */
const RETIRED_MERGE = entry({
  id: "retired-merge-1",
  text: "a merge that was merged again",
  merged_from: ["a-1", "b-1"],
  retired_at: "2026-02-01T00:00:00.000Z",
  merged_into: "merged-2",
});
const LIVE_MERGE = entry({
  id: "merged-2",
  text: "the outer merge",
  merged_from: ["retired-merge-1", "c-1"],
});

function listing(entries: MemoryEntry[]) {
  invoke.mockImplementation((cmd: string) => {
    if (cmd === "memory_list_all") return Promise.resolve(entries);
    return Promise.reject(new Error(`unexpected command ${cmd}`));
  });
}

beforeEach(() => {
  // Braces matter: `mockReset()` returns the mock, and a `beforeEach` that
  // returns a function has just registered it as the teardown hook.
  invoke.mockReset();
});
afterEach(cleanup);

describe("Memory Studio panel", () => {
  it("tells the truth about a pinned memory that still holds an expiry, and lets it be cleared", async () => {
    listing([PINNED_WITH_EXPIRY]);
    render(<MemoryStudioPanel />);

    await screen.findByText("pinned but still carrying a date");
    // Not "Pinned memories never expire" — the date is stored, and comes back.
    expect(screen.getByText(/exempt from its .* expiry/)).toBeTruthy();
    expect(screen.getByText(/unpinning it applies that date again/)).toBeTruthy();
    expect(screen.queryByText(/unpin this one to give it an expiry/)).toBeNull();
    // The date can be removed for good without unpinning first.
    expect(screen.getByRole("button", { name: "Clear expiry" })).toBeTruthy();
  });

  it("keeps the plain 'a date expires at the end of that day' hint for a pinned memory with no date", async () => {
    listing([entry({ pinned: true })]);
    render(<MemoryStudioPanel />);

    await screen.findByText("a remembered thing");
    expect(screen.getByText("Pinned memories never expire — unpin this one to give it an expiry.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Clear expiry" })).toBeNull();
  });

  it("hides merge-retired memories until the Retired filter asks for them, and offers them no merge checkbox", async () => {
    listing([PLAIN_GLOBAL, RETIRED]);
    render(<MemoryStudioPanel />);

    await screen.findByText("a plain global memory");
    expect(screen.queryByText("an original retired by a merge")).toBeNull();
    // One selectable row: the live one.
    expect(screen.getAllByLabelText("Select for merge")).toHaveLength(1);

    fireEvent.click(screen.getByRole("button", { name: "Retired by merge" }));
    await screen.findByText("an original retired by a merge");
    expect(screen.queryByText("a plain global memory")).toBeNull();
    // `merge_impl` refuses a retired parent, so no checkbox is offered.
    expect(screen.queryAllByLabelText("Select for merge")).toHaveLength(0);
  });

  it("says why a one-row and a cross-scope selection cannot be merged instead of failing on the click", async () => {
    listing([PLAIN_GLOBAL, PLAIN_PROJECT]);
    render(<MemoryStudioPanel />);

    await screen.findByText("a plain global memory");
    const boxes = screen.getAllByLabelText("Select for merge");
    fireEvent.click(boxes[0]);
    await screen.findByText("Merging needs at least two memories — select another one.");

    fireEvent.click(boxes[1]);
    await waitFor(() =>
      expect(screen.getByText(/Memories can only be merged within one scope/)).toBeTruthy(),
    );
    expect(screen.queryByText(/needs at least two memories/)).toBeNull();
    expect(screen.getByRole("button", { name: /Merge 2 selected/ }).getAttribute("disabled")).not.toBeNull();
  });

  it("drops the merge selection when a filter changes, so nothing invisible can be merged", async () => {
    listing([PLAIN_GLOBAL, RETIRED]);
    render(<MemoryStudioPanel />);

    await screen.findByText("a plain global memory");
    fireEvent.click(screen.getAllByLabelText("Select for merge")[0]);
    await screen.findByText(/needs at least two memories/);

    fireEvent.click(screen.getByRole("button", { name: "Retired by merge" }));
    await waitFor(() => expect(screen.queryByText(/needs at least two memories/)).toBeNull());
  });

  it("drops the merge selection when a search hides the selected rows", async () => {
    listing([PLAIN_GLOBAL, PLAIN_PROJECT]);
    render(<MemoryStudioPanel />);

    await screen.findByText("a plain global memory");
    fireEvent.click(screen.getAllByLabelText("Select for merge")[0]);
    await screen.findByText(/needs at least two memories/);

    fireEvent.change(screen.getByPlaceholderText("Search memory text…"), {
      target: { value: "nothing matches this" },
    });
    await waitFor(() => expect(screen.queryByText(/needs at least two memories/)).toBeNull());
    expect(screen.queryByRole("button", { name: /Merge \d+ selected/ })).toBeNull();
  });

  it("offers no Undo merge on a merge that was itself merged into a newer one", async () => {
    listing([LIVE_MERGE, RETIRED_MERGE]);
    render(<MemoryStudioPanel />);

    // The live outer merge can be undone.
    await screen.findByText("the outer merge");
    expect(screen.getAllByRole("button", { name: "Undo merge" })).toHaveLength(1);

    // The inner one is retired: `unmerge_impl` would refuse it, so the
    // button that can only fail is not rendered.
    fireEvent.click(screen.getByRole("button", { name: "Retired by merge" }));
    await screen.findByText("a merge that was merged again");
    expect(screen.queryByText("the outer merge")).toBeNull();
    expect(screen.queryAllByRole("button", { name: "Undo merge" })).toHaveLength(0);
  });
});
