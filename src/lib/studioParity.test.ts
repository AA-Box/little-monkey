import { describe, expect, it } from "vitest";
import { hardwareFit, inferComponentSlot, modelSpecForDownload, type DiscoveryItem } from "./studioParity";
const item: DiscoveryItem = { id: "hf:x", source: "hugging_face", name: "Qwen Image", assetKind: "model", family: "Qwen", repo: "x/y", fileName: "qwen-image-Q4_K.gguf", downloadUrl: "https://example.com/x", pageUrl: "https://example.com", sha256: "a".repeat(64), sizeBytes: 10_000, tags: [] };
describe("studio parity", () => {
  it("recognises diffusion checkpoints", () => expect(inferComponentSlot(item)).toBe("diffusion_model"));
  it("creates a local model spec", () => expect(modelSpecForDownload(item, "/tmp/a.gguf", 10_000).components[0]?.source).toEqual({ kind: "local_file", path: "/tmp/a.gguf" }));
  it("labels hardware headroom", () => { expect(hardwareFit(32, 16)).toBe("fits"); expect(hardwareFit(16, 15)).toBe("tight"); expect(hardwareFit(8, 16)).toBe("too_large"); });
});
