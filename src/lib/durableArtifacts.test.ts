import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { artifactDataUrl } from "./durableArtifacts";

describe("artifactDataUrl", () => {
  it("builds a normalized data URL at the UI/model boundary", () => {
    expect(artifactDataUrl(" Image/PNG ", "aGVsbG8=")).toBe("data:image/png;base64,aGVsbG8=");
  });

  it("rejects media-type and base64 injection", () => {
    expect(() => artifactDataUrl("image/png;name=x", "aGVsbG8=")).toThrow("media type");
    expect(() => artifactDataUrl("image/png", "not base64")).toThrow("base64");
  });
});
