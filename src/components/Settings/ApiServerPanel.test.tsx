import { describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";

import { ApiServerPanel } from "./ApiServerPanel";
import { RuntimeHubLan } from "./runtimeHub/RuntimeHubLan";

describe("API server token migration", () => {
  it("retires ordinary legacy minting while retaining compatibility-token management", () => {
    const markup = renderToStaticMarkup(<ApiServerPanel onOpenRuntimeHubPairing={vi.fn()} />);

    expect(markup).toContain("Use Runtime Hub pairing for new clients");
    expect(markup).toContain("Open Runtime Hub pairing");
    expect(markup).toContain("General-purpose minting is retired");
    expect(markup).toContain("publishing a Local App still creates its restricted compatibility token automatically");

    expect(markup).not.toContain("Create a token");
    expect(markup).not.toContain("Create token");
    expect(markup).not.toContain("Reference token");
    expect(markup).not.toContain("Label, e.g.");
  });

  it("points widgets at a one-time paired token instead of a legacy-token picker", () => {
    const markup = renderToStaticMarkup(<ApiServerPanel onOpenRuntimeHubPairing={vi.fn()} />);

    expect(markup).toContain("Paired chat token");
    expect(markup).toContain("Paste the one-time token from Runtime Hub pairing");
    expect(markup).not.toContain("Reference token");
  });

  it("labels Runtime Hub pairing as the recommended new-client flow", () => {
    const markup = renderToStaticMarkup(<RuntimeHubLan />);

    expect(markup).toContain("Pair a new API client (recommended)");
    expect(markup).toContain("every new IDE, script, agent, or widget");
  });
});
