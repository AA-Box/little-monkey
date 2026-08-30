// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import PastedTextEditorModal from "./PastedTextEditorModal";

afterEach(cleanup);

describe("PastedTextEditorModal", () => {
  it("edits and saves the exact local text without any model dependency", () => {
    const onSave = vi.fn();
    const onClose = vi.fn();
    render(
      <PastedTextEditorModal
        name="Pasted text (1).md"
        content="# Initial\nbody"
        onSave={onSave}
        onClose={onClose}
      />,
    );

    expect(screen.getByText("Pasted text (1).md")).toBeTruthy();
    expect(screen.getByText(/fully local and uses no AI or tokens/i)).toBeTruthy();

    const editor = screen.getByRole("textbox");
    fireEvent.change(editor, { target: { value: "# Edited\nexact body" } });
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    expect(onSave).toHaveBeenCalledWith("# Edited\nexact body");
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("closes on Escape without saving", () => {
    const onSave = vi.fn();
    const onClose = vi.fn();
    render(
      <PastedTextEditorModal
        name="Pasted text (1).md"
        content="content"
        onSave={onSave}
        onClose={onClose}
      />,
    );

    fireEvent.keyDown(document, { key: "Escape" });

    expect(onClose).toHaveBeenCalledOnce();
    expect(onSave).not.toHaveBeenCalled();
  });
});
