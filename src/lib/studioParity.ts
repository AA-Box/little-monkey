import { invoke } from "@tauri-apps/api/core";
import type { ComponentSlot, GenerationModelSpec } from "./studioClient";

export type DiscoverySource = "civitai" | "hugging_face";
export type DiscoveryAssetKind = "model" | "lora";
export interface DiscoveryItem { id: string; source: string; name: string; assetKind: DiscoveryAssetKind; family: string; repo: string | null; fileName: string; downloadUrl: string; pageUrl: string; sha256: string | null; sizeBytes: number; tags: string[] }
export interface DiscoveryDownloadResult { path: string; sizeBytes: number; sha256: string }
export interface CharacterTrainingResult { loraPath: string; imageCount: number; logTail: string }
export interface CharacterTrainerCapabilities {
  backend: "mflux" | "musubi_zimage";
  supported: boolean;
  setupReady: boolean;
  pythonVersion: string | null;
  gpuName: string | null;
  vramMib: number | null;
  computeCapability: string | null;
  minimumVramMib: number | null;
  sourceVersion: string;
  reason: string | null;
}
export interface PortableTrainingStatus {
  status: "idle" | "setting_up" | "running" | "complete" | "error" | "cancelled";
  phase: string;
  logTail: string;
  loraPath: string | null;
  loraName: string | null;
  currentPid: number | null;
}
export interface PortableTrainingRequest {
  name: string;
  sourceDir: string;
  ditPath: string;
  vaePath: string;
  textEncoderPath: string;
  triggerWord: string;
  steps: number;
  resolution: number;
}
export interface WorkflowUpload { name: string; dataBase64: string }
export type WorkflowKind = "music" | "talking_character" | "motion_control" | "extend_video";

export const CURATED_STUDIO_PROFILES = [
  { id: "qwen-image", name: "Qwen Image 2.1", query: "Qwen Image 2.1 GGUF", source: "hugging_face" as const, kind: "model" as const, minRamBytes: 16 * 1024 ** 3, note: "Image generation/editing; prefer a GGUF or safetensors build with explicit license metadata." },
  { id: "flux", name: "FLUX image", query: "FLUX.1 dev GGUF", source: "hugging_face" as const, kind: "model" as const, minRamBytes: 12 * 1024 ** 3, note: "High-quality still images and broad LoRA ecosystem." },
  { id: "wan", name: "Wan video", query: "Wan 2.2 TI2V GGUF", source: "hugging_face" as const, kind: "model" as const, minRamBytes: 20 * 1024 ** 3, note: "Image/text-to-video family; exact fit depends heavily on quantization." },
  { id: "character-lora", name: "Character / style LoRAs", query: "character", source: "civitai" as const, kind: "lora" as const, minRamBytes: 0, note: "Browse verified LoRA files and add them directly to Studio." },
] as const;

export const WORKFLOW_PRESETS: Record<WorkflowKind, { label: string; description: string; placeholders: string[] }> = {
  music: { label: "Music", description: "Run an ACE-Step or compatible ComfyUI music workflow.", placeholders: ["prompt", "negative_prompt", "duration_seconds"] },
  talking_character: { label: "Talking Character", description: "Run a talking-character/lip-sync workflow with an uploaded portrait.", placeholders: ["input_image", "script", "prompt"] },
  motion_control: { label: "Motion Control", description: "Drive a video workflow from a reference image and motion prompt.", placeholders: ["input_image", "prompt", "negative_prompt"] },
  extend_video: { label: "Extend Video", description: "Upload an existing clip and run a continuation/extension workflow.", placeholders: ["input_video", "prompt", "negative_prompt"] },
};

export const studioParityClient = {
  search: (source: DiscoverySource, query: string, assetKind: DiscoveryAssetKind) => invoke<DiscoveryItem[]>("studio_discovery_search", { source, query, assetKind }),
  download: (item: DiscoveryItem) => invoke<DiscoveryDownloadResult>("studio_discovery_download", { request: { name: item.name, assetKind: item.assetKind, downloadUrl: item.downloadUrl, sha256: item.sha256 ?? "", sizeBytes: item.sizeBytes, fileName: item.fileName } }),
  characterCapabilities: () => invoke<CharacterTrainerCapabilities>("character_trainer_capabilities"),
  setupPortableCharacterTrainer: () => invoke<CharacterTrainerCapabilities>("character_trainer_setup"),
  trainCharacter: (request: { name: string; sourceDir: string; baseModelPath: string; triggerWord: string; epochs: number; rank: number; maxResolution: number }) => invoke<CharacterTrainingResult>("character_training_start", { request }),
  startPortableCharacterTraining: (request: PortableTrainingRequest) => invoke<PortableTrainingStatus>("character_portable_training_start", { request }),
  portableCharacterStatus: () => invoke<PortableTrainingStatus>("character_training_status"),
  cancelPortableCharacterTraining: () => invoke<void>("character_training_cancel"),
  runWorkflow: (request: { kind: WorkflowKind; baseUrl: string; workflow: unknown; values: Record<string, unknown>; inputImage?: WorkflowUpload | null; inputVideo?: WorkflowUpload | null }) => invoke<import("./studioClient").GenerationEntry[]>("studio_workflow_run", { request }),
};

export function inferComponentSlot(item: DiscoveryItem): ComponentSlot {
  const value = `${item.name} ${item.fileName} ${item.tags.join(" ")}`.toLowerCase();
  return /qwen.?image|diffusion|transformer/.test(value) ? "diffusion_model" : "checkpoint";
}

export function modelSpecForDownload(item: DiscoveryItem, path: string, sizeBytes: number): GenerationModelSpec {
  const lower = `${item.name} ${item.fileName} ${item.tags.join(" ")}`.toLowerCase();
  const video = /wan|ltx|video/.test(lower);
  const quantization = item.fileName.match(/(?:q|iq)(\d)/i)?.[1];
  return {
    id: `community-${item.id.replace(/[^A-Za-z0-9._-]/g, "-").slice(0, 100)}`,
    name: item.name,
    family: item.family,
    tasks: video ? ["text_to_video", "image_to_video"] : ["text_to_image", "image_to_image"],
    components: [{ slot: inferComponentSlot(item), source: { kind: "local_file", path }, sizeBytes }],
    source: { kind: "components" },
    defaults: { width: video ? 832 : 1024, height: video ? 480 : 1024, steps: video ? 20 : 24, cfgScale: 4, sampleMethod: "euler", flowShift: null, fps: 24, videoFrames: 81, frameGrid: "down_to4n_plus1" },
    minRamBytes: Math.ceil(Math.max(sizeBytes * 1.35, video ? 16 * 1024 ** 3 : 8 * 1024 ** 3)),
    license: { id: `community-${item.id}`, name: "Upstream model terms", url: item.pageUrl, excludedTerritories: [], acceptanceRequired: false },
    extraLaunchArgs: [], engine: "stable_diffusion_cpp", quantizationBits: quantization ? Number(quantization) : null,
  };
}

export function hardwareFit(totalRamBytes: number, minimum: number): "fits" | "tight" | "too_large" | "unknown" {
  if (!totalRamBytes) return "unknown";
  if (minimum > totalRamBytes) return "too_large";
  if (minimum > totalRamBytes * 0.8) return "tight";
  return "fits";
}
