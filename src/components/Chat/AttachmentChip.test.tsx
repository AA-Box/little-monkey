// @vitest-environment jsdom
/**
 * The attachment chip's right-click menu. Two claims worth holding: it opens
 * only for attachments that name a real file (terminal evidence carries a
 * synthetic path and must not offer to reveal it), and picking the action
 * reaches the same `reveal_in_finder` command the rest of the app uses, with
 * the chip's own path.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

const invoke = vi.fn((..._args: unknown[]) => Promise.resolve());
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

import { AttachmentChip } from "./AttachmentChip";

afterEach(() => {
  cleanup();
  invoke.mockClear();
});

describe("AttachmentChip", () => {
  it("reveals the attachment's path on right-click", () => {
    render(<AttachmentChip name="notes.md" isDir={false} revealPath="/repos/app/notes.md" onRemove={() => {}} />);

    expect(screen.queryByText("Show in Finder")).toBeNull();
    fireEvent.contextMenu(screen.getByText("notes.md"));
    fireEvent.click(screen.getByText("Show in Finder"));

    expect(invoke).toHaveBeenCalledWith("reveal_in_finder", { path: "/repos/app/notes.md" });
    // The menu closes behind the action rather than lingering over the composer.
    expect(screen.queryByText("Show in Finder")).toBeNull();
  });

  it("offers no menu for an attachment with no file behind it", () => {
    render(<AttachmentChip name="Terminal output" isDir={false} onRemove={() => {}} />);

    fireEvent.contextMenu(screen.getByText("Terminal output"));

    expect(screen.queryByText("Show in Finder")).toBeNull();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("closes on Escape without revealing anything", () => {
    render(<AttachmentChip name="notes.md" isDir={false} revealPath="/repos/app/notes.md" onRemove={() => {}} />);

    fireEvent.contextMenu(screen.getByText("notes.md"));
    expect(screen.getByText("Show in Finder")).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });

    expect(screen.queryByText("Show in Finder")).toBeNull();
    expect(invoke).not.toHaveBeenCalled();
  });
});
