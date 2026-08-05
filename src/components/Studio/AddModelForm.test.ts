import { describe, expect, it } from "vitest";

import { describeWeightFile } from "./AddModelForm";

/** Naming a model is the one thing the add form may guess at: unlike a slot, a
 *  wrong family costs a retype rather than a tensor-shape error deep in the
 *  engine. It still has to be right often enough to be worth having. */
describe("describeWeightFile", () => {
  it("reads a name and a family out of real weight file names", () => {
    for (const [file, name, family] of [
      ["/Users/ahmad/Downloads/h3/sd-turbo.safetensors", "sd turbo", "SD"],
      ["split_files/diffusion_models/wan2.2_ti2v_5B_fp16.safetensors", "wan2 2 ti2v 5B", "Wan"],
      ["minimax_h3_ref2va_pruned-Q4_K_M.gguf", "minimax h3 ref2va", "MiniMax"],
      // Qwen3-VL, but shipped as MiniMax H3's text encoder — the model it
      // belongs to is the one worth naming.
      ["qwen3vl_32b_minimax_h3-Q2_K_M.gguf", "qwen3vl 32b minimax h3", "MiniMax"],
      ["flux1-dev.safetensors", "flux1 dev", "FLUX"],
      ["Qwen3-TTS-12Hz-1.7B-Base-Q4_K_M.gguf", "Qwen3 TTS 12Hz 1 7B Base", "Qwen"],
    ] as const) {
      expect(describeWeightFile(file), file).toEqual({ name, family });
    }
  });

  it("leaves the family blank rather than inventing one", () => {
    expect(describeWeightFile("/models/something_custom.safetensors")).toEqual({
      name: "something custom",
      family: "",
    });
  });
});
