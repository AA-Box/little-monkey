import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", async (importOriginal) => ({
  ...await importOriginal<typeof import("@tauri-apps/api/core")>(),
  invoke: vi.fn(),
  isTauri: () => false,
}));

import MessageBubble from "./MessageBubble";

describe("compact message translation actions", () => {
  it("keeps the language picker collapsed until the user opens it", () => {
    const html = renderToStaticMarkup(createElement(MessageBubble, {
      message: { role: "assistant", content: "Hello" },
      index: 1,
      sessionId: "session",
    }));

    expect(html).toContain('aria-label="Translate"');
    expect(html).not.toContain("<select");
    expect(html).not.toContain("Translation language");
  });

  it("does not add a translation action to an image-only message", () => {
    const html = renderToStaticMarkup(createElement(MessageBubble, {
      message: {
        role: "user",
        content: [{ type: "image_url", image_url: { url: "data:image/png;base64,AA==" } }],
      },
      index: 0,
      sessionId: "session",
    }));

    expect(html).not.toContain('aria-label="Translate"');
  });
});
