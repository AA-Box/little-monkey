import type { ComponentSlot } from "./studioClient";

/** What a weight file's own name suggests about it. Every field is a starting
 *  point the add form prefills and the user can overwrite. */
export interface WeightFileHint {
  name: string;
  family: string;
  slot: ComponentSlot | null;
}

/** Architecture families, matched against a weight file's own name. */
const FAMILY_HINTS: [RegExp, string][] = [
  [/wan[._-]?\d/i, "Wan"],
  [/minimax|(^|[^a-z])h3([^a-z]|$)/i, "MiniMax"],
  [/hunyuan/i, "Hunyuan"],
  [/ltx/i, "LTX"],
  [/flux/i, "FLUX"],
  [/qwen/i, "Qwen"],
  [/sdxl|xl[._-]?base|xl[._-]?refiner/i, "SDXL"],
  [/(^|[^a-z])sd[._-]?[0-9x]|stable[._-]?diffusion|turbo/i, "SD"],
  [/outetts|wavtokenizer|[._-]tts/i, "TTS"],
];

/**
 * Slots, matched against the same name. Order is load-bearing: `audio_vae`
 * contains `vae`, `high_noise` names a diffusion model, and `clip_vision`
 * would otherwise be caught by the `clip` patterns.
 *
 * Only tokens that name a component outright are here. A file whose name says
 * nothing gets `null` and keeps whatever the row already had, because the cost
 * of a wrong slot is a tensor-shape error deep inside the engine — a guess is
 * worth making from evidence and not worth inventing without any.
 */
const SLOT_HINTS: [RegExp, ComponentSlot][] = [
  [/mmproj/i, "mmproj"],
  [/vocoder|wavtokenizer/i, "vocoder"],
  [/taesd/i, "taesd"],
  [/clip[._\-/]?vision|clip_?vit|vision[._\-/]?encoder/i, "clip_vision"],
  [/clip[._-]?l(?![a-z])/i, "clip_l"],
  [/clip[._-]?g(?![a-z])/i, "clip_g"],
  // t5, mt5, umt5 — bounded so a name like `gpt5` is not swept in.
  [/(^|[^a-z])u?m?t5/i, "t5xxl"],
  [/audio[._\-/]?vae/i, "audio_vae"],
  [/(^|[^a-z])vae([^a-z]|$)/i, "vae"],
  [/high[._-]?noise/i, "high_noise_diffusion_model"],
  [/qwen.*vl|(^|[^a-z])llm([^a-z]|$)|mistral|text[._\-/]?encoder/i, "llm"],
  [/unet|diffusion[._\-/]?model|transformer/i, "diffusion_model"],
];

/**
 * Reads a name, a family and a slot out of a weight file's own file name.
 *
 * The app still shows every one of these in an editable control — this fills
 * blanks, it does not decide. Naming is free to be wrong; the slot is not,
 * which is why it only answers when the file name actually says something.
 */
export function describeWeightFile(raw: string): WeightFileHint {
  const base = (raw.split(/[/\\]/).pop() ?? raw)
    .replace(/\.(safetensors|gguf|ckpt|pt|bin|pth)$/i, "")
    // Repeated as one group: `_pruned-Q4_K_M` is two tags, and an anchored
    // single match would only ever strip the last one.
    .replace(/([._-](q\d[_a-z0-9]*|fp\d+|bf\d+|int\d+|pruned))+$/i, "");
  // The slot reads the whole path, not just the file name: repositories lay
  // these out as `split_files/diffusion_models/…`, `text_encoders/…`, `vae/…`,
  // so the directory names the component even when the file does not.
  const path = raw.replace(/\\/g, "/");
  return {
    name: base.replace(/[._-]+/g, " ").replace(/\s+/g, " ").trim(),
    family: FAMILY_HINTS.find(([pattern]) => pattern.test(base))?.[1] ?? "",
    slot: SLOT_HINTS.find(([pattern]) => pattern.test(path))?.[1] ?? null,
  };
}
