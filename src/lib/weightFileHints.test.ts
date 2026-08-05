import { describe, expect, it } from "vitest";

import { describeWeightFile } from "./weightFileHints";

/** The add form may guess at all three of these, but they are not equally
 *  forgiving: a wrong name costs a retype, a wrong slot costs a tensor-shape
 *  error several minutes into a load. So the slot answers only when the file
 *  name actually says something. */
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
      const hint = describeWeightFile(file);
      expect({ name: hint.name, family: hint.family }, file).toEqual({ name, family });
    }
  });

  it("names the slot when the file name names the component", () => {
    for (const [file, slot] of [
      ["clip_l.safetensors", "clip_l"],
      ["split_files/text_encoders/clip_g.safetensors", "clip_g"],
      ["clip_vision_h.safetensors", "clip_vision"],
      ["t5xxl_fp16.safetensors", "t5xxl"],
      ["umt5_xxl_fp8_e4m3fn_scaled.safetensors", "t5xxl"],
      ["minimax_h3_audio_vae_fp32.safetensors", "audio_vae"],
      ["minimax_h3_video_vae_fp16.safetensors", "vae"],
      ["wan2.2_t2v_high_noise_14B_fp16.safetensors", "high_noise_diffusion_model"],
      ["mmproj-Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf", "mmproj"],
      ["WavTokenizer-Large-75-F16.gguf", "vocoder"],
      ["taesd_decoder.safetensors", "taesd"],
      ["qwen3vl_32b_minimax_h3-Q2_K_M.gguf", "llm"],
      // The directory is the only thing that names this one, and repositories
      // lay these files out by component.
      ["split_files/diffusion_models/wan2.2_ti2v_5B_fp16.safetensors", "diffusion_model"],
      ["split_files/text_encoders/umt5_xxl_fp8_scaled.safetensors", "t5xxl"],
      ["C:\\models\\vae\\wan2.2_vae.safetensors", "vae"],
    ] as const) {
      expect(describeWeightFile(file).slot, file).toBe(slot);
    }
  });

  it("stays quiet when the file name says nothing, rather than inventing", () => {
    // An all-in-one checkpoint is exactly this case: the name carries no
    // component token, so the row keeps its own default instead of being
    // pushed onto --diffusion-model, which is what broke SD turbo.
    expect(describeWeightFile("/Users/ahmad/Downloads/h3/sd-turbo.safetensors").slot).toBeNull();
    expect(describeWeightFile("/models/something_custom.safetensors")).toEqual({
      name: "something custom",
      family: "",
      slot: null,
    });
  });
});
