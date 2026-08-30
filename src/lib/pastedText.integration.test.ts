import { describe, expect, it } from "vitest";

import { parseBuiltInSlashCommand } from "./slashCommands";
import {
  composePromptWithPastedText,
  rebasePastedTextPlacements,
  type PastedTextPlacement,
} from "./pastedText";

/**
 * Feature-level semantic acceptance for the large-paste composer. Component
 * tests separately cover the card/editor UI; this file exercises the complete
 * local draft lifecycle that ChatWindow uses before it hands a turn to normal
 * command/skill/routing/model execution.
 */
describe("large-paste composer semantic integration", () => {
  it("matches native paste replacement, later edits, and boundary whitespace byte-for-byte", () => {
    const pastedPath = "pasted://spec";
    const pasted = "\n  # exact spec\nkeep trailing whitespace  \n\n";

    // Native paste into the selected word would replace it at offset 7.
    let visible = "before REMOVE after";
    const selectionStart = 7;
    const selectionEnd = 13;
    const afterCollapsedPaste = visible.slice(0, selectionStart) + visible.slice(selectionEnd);
    let placements: PastedTextPlacement[] = rebasePastedTextPlacements(visible, afterCollapsedPaste, []);
    placements.push({ path: pastedPath, offset: selectionStart, order: 0 });
    visible = afterCollapsedPaste;

    // The user can keep editing the compact composer. Edits before/at the
    // zero-width card anchor rebase exactly like an expanded textarea would.
    const afterPrefixEdit = `>>${visible}`;
    placements = rebasePastedTextPlacements(visible, afterPrefixEdit, placements);
    visible = afterPrefixEdit;

    const anchor = placements[0].offset;
    const afterAnchorEdit = visible.slice(0, anchor) + "FOLLOW" + visible.slice(anchor);
    placements = rebasePastedTextPlacements(visible, afterAnchorEdit, placements);
    visible = afterAnchorEdit;

    const semantic = composePromptWithPastedText(visible, [
      { path: pastedPath, label: "Pasted text (1).md", content: pasted },
    ], placements);

    expect(semantic).toBe(`>>before ${pasted}FOLLOW after`);
    expect(semantic.includes("### Pasted text")).toBe(false);
    expect(semantic.endsWith(" after")).toBe(true);
  });

  it("feeds a collapsed built-in command through the same parser as expanded text", () => {
    const pastedPath = "pasted://command";
    const expanded = "/model qwen3:8b";
    const semantic = composePromptWithPastedText("", [
      { path: pastedPath, content: expanded },
    ], [
      { path: pastedPath, offset: 0, order: 0 },
    ]);

    expect(semantic).toBe(expanded);
    const parsed = parseBuiltInSlashCommand(semantic);
    expect(parsed?.definition.command).toBe("model");
    expect(parsed?.arguments).toBe("qwen3:8b");
  });

  it("keeps consecutive collapsed pastes and visible text in exact textarea order", () => {
    const attachments = [
      { path: "pasted://a", content: "AAA" },
      { path: "pasted://b", content: "BBB\n" },
      { path: "/workspace/reference.md", content: "must not be folded into user text" },
    ];
    const placements: PastedTextPlacement[] = [
      { path: "pasted://a", offset: 0, order: 0 },
      { path: "pasted://b", offset: 0, order: 1 },
    ];

    expect(composePromptWithPastedText("tail", attachments, placements)).toBe("AAABBB\ntail");
  });
});
