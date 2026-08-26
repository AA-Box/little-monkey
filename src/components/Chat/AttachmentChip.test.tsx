// @vitest-environment jsdom
/**
 * Attachment-chip interaction contracts. Real files expose the Finder menu;
 * virtual attachments do not. Editable virtual attachments expose a real
 * primary button next to, never around, the independent remove button.
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
    expect(screen.queryByText("Show in Finder")).toBeNull();
  });

  it("offers no Finder menu for an attachment with no file behind it", () => {
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

  it("opens an editable virtual attachment through a real button", () => {
    const onOpen = vi.fn();
    render(
      <AttachmentChip
        name="Pasted text (1).md"
        detail="12 KB · ~3.0k tokens"
        isDir={false}
        onOpen={onOpen}
        onRemove={() => {}}
      />,
    );

    const openButton = screen.getByRole("button", { name: "Open Pasted text (1).md" });
    expect(openButton.tagName).toBe("BUTTON");
    expect(openButton.querySelector("button")).toBeNull();
    expect(screen.getByText("12 KB · ~3.0k tokens")).toBeTruthy();
    fireEvent.click(openButton);

    expect(onOpen).toHaveBeenCalledOnce();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("keeps remove as a sibling control and does not open when removing", () => {
    const onOpen = vi.fn();
    const onRemove = vi.fn();
    render(
      <AttachmentChip
        name="Pasted text (1).md"
        isDir={false}
        onOpen={onOpen}
        onRemove={onRemove}
      />,
    );

    const openButton = screen.getByRole("button", { name: "Open Pasted text (1).md" });
    const removeButton = screen.getByLabelText("Remove attachment");
    expect(openButton.contains(removeButton)).toBe(false);
    fireEvent.click(removeButton);

    expect(onRemove).toHaveBeenCalledOnce();
    expect(onOpen).not.toHaveBeenCalled();
  });
});
