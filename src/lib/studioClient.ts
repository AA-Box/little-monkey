import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type FrameGrid = "down_to4n_plus1" | "up_to17k_plus5";

export type GenerationTask =
  | "text_to_image"
  | "image_to_image"
  | "text_to_video"
  | "image_to_video"
  /** Served by llama-tts, not sd-server. */
  | "text_to_speech";

export type ComponentSlot =
  | "checkpoint"
  | "diffusion_model"
  | "high_noise_diffusion_model"
  | "clip_l"
  | "clip_g"
  | "clip_vision"
  | "t5xxl"
  | "llm"
  | "llm_vision"
  | "uncond_diffusion_model"
  | "embeddings_connectors"
  | "motion_module"
  | "vae"
  | "audio_vae"
  | "taesd"
  /** The three conditioning slots: filling one is what makes the matching
   *  per-run image mean anything to the engine. See CONDITIONING_SLOTS. */
  | "control_net"
  | "ip_adapter"
  | "photo_maker"
  | "pulid_weights"
  /** The YOLOv8 detector ADetailer re-renders around. A launch flag, unlike the
   *  ad prompts beside it, which are per-run request fields. */
  | "ad_model"
  /** Speech only, and served by llama-tts rather than sd-server. */
  | "mmproj"
  | "vocoder";

/** Which per-run conditioning image a loaded slot unlocks. Mirrors
 *  `ConditioningImage` in generation.rs — the engine reads the three from three
 *  different request fields, so they are not interchangeable. */
export type ConditioningImage = "control" | "ip_adapter" | "reference";

/** The slot → conditioning-image mapping, mirroring
 *  `ComponentSlot::conditioning_image`. The generation form reads this to
 *  decide which conditioning inputs a model can actually use; the backend
 *  re-checks it, so this only governs what is offered. */
export const CONDITIONING_SLOTS: Partial<Record<ComponentSlot, ConditioningImage>> = {
  control_net: "control",
  ip_adapter: "ip_adapter",
  photo_maker: "reference",
  pulid_weights: "reference",
};

/** Reference images one run may carry. Mirrors the backend's own ceiling —
 *  each is decoded and held in memory beside the others. */
export const MAX_REF_IMAGES = 8;

/** The engine's own feature flag behind each conditioning image. Two gates
 *  guard the same input for different reasons: the weight slot decides whether
 *  the loaded model can read the image at all, the flag decides whether this
 *  build of the engine accepts the field. */
const CONDITIONING_FEATURES: Record<ConditioningImage, string> = {
  control: "control_image",
  ip_adapter: "ip_adapter_image",
  reference: "ref_images",
};

/** Whether the running engine accepts a per-run field.
 *
 *  With no engine running there is nothing to ask, so everything is offered and
 *  the backend has the last word — the alternative is a panel whose inputs all
 *  appear only after the first generation. A running engine that does not name
 *  the flag is taken at its word: an older build that never heard of
 *  `mask_image` rejects the whole request when one is sent. */
export function engineSupports(
  capabilities: EngineCapabilities | null,
  feature: string,
): boolean {
  return capabilities === null || capabilities.features[feature] === true;
}

/** Which conditioning images this set of filled slots unlocks, narrowed to the
 *  ones the running engine still accepts. */
export function availableConditioning(
  slots: ComponentSlot[],
  capabilities: EngineCapabilities | null = null,
): Set<ConditioningImage> {
  return new Set(
    slots
      .map((slot) => CONDITIONING_SLOTS[slot])
      .filter((kind): kind is ConditioningImage => !!kind)
      .filter((kind) => engineSupports(capabilities, CONDITIONING_FEATURES[kind])),
  );
}

/** Where a component's bytes come from. A file the user already has is a
 *  first-class source, referenced where it lies and never fetched. */
export type ComponentSource =
  | { kind: "hugging_face"; repo: string; file: string }
  | { kind: "local_file"; path: string };

export interface ModelComponent {
  slot: ComponentSlot;
  source: ComponentSource;
  sizeBytes: number;
}

/** The flat file name a component takes on disk. */
export function componentFileName(component: ModelComponent): string {
  const raw =
    component.source.kind === "hugging_face"
      ? component.source.file
      : component.source.path;
  const parts = raw.split(/[/\\]/);
  return parts[parts.length - 1] ?? raw;
}

/** A LoRA in the user's library — added once with a file picker, then chosen
 *  by name for each run rather than typed as a path every time. */
export interface LoraAsset {
  name: string;
  path: string;
}

/** One LoRA applied to a generation. Any model takes any number of them. */
export interface LoraSelection {
  path: string;
  multiplier: number;
  isHighNoise: boolean;
}

export interface GenerationDefaults {
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  sampleMethod: string;
  flowShift: number | null;
  fps: number;
  videoFrames: number;
  frameGrid: FrameGrid;
}

/** A model's terms. `excludedTerritories` being non-empty is what makes the
 *  license sheet blocking rather than informational. */
export interface LicenseGate {
  id: string;
  name: string;
  url: string;
  excludedTerritories: string[];
  acceptanceRequired: boolean;
}

/**
 * Which engine renders a model.
 *
 * `stable_diffusion_cpp` is the bundled engine and the default every model
 * written before this field existed still deserializes to. `mlx_video` is the
 * video service inside the installed MLX package: Apple silicon only, and the
 * only engine that can read an MLX-quantized checkpoint.
 */
export type GenerationEngineKind = "stable_diffusion_cpp" | "mlx_video";

/** A model exactly as stored in the user's library. */
export interface GenerationModelSpec {
  id: string;
  name: string;
  family: string;
  tasks: GenerationTask[];
  components: ModelComponent[];
  defaults: GenerationDefaults;
  minRamBytes: number;
  license: LicenseGate;
  extraLaunchArgs: string[];
  engine: GenerationEngineKind;
}

export interface GenerationModel {
  id: string;
  name: string;
  family: string;
  tasks: GenerationTask[];
  components: ModelComponent[];
  defaults: GenerationDefaults;
  minRamBytes: number;
  license: LicenseGate;
  extraLaunchArgs: string[];
  engine: GenerationEngineKind;
  installed: boolean;
  /** Measured on this machine, not declared in the entry. */
  totalBytes: number;
  missingBytes: number;
  licenseAccepted: boolean;
  fitsInMemory: boolean;
}

export interface GenerationEntry {
  entryId: string;
  artifactId: string;
  modelId: string;
  task: GenerationTask;
  prompt: string;
  negativePrompt: string;
  mediaType: string;
  sizeBytes: number;
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  seed: number;
  frameCount: number;
  fps: number;
  durationMs: number;
  createdAtMs: number;
}

/** What the engine that is running right now reports about itself, read off
 *  `GET /sdcpp/v1/capabilities`. The lists below it — SAMPLERS, SCHEDULERS,
 *  UPSCALERS — are only the fallback for before it has ever launched: the
 *  engine builds these from its own enums, so a stable-diffusion.cpp release
 *  that adds a sampler is usable the moment it is installed. */
export interface EngineCapabilities {
  samplers: string[];
  schedulers: string[];
  upscalers: string[];
  /** `init_image`, `mask_image`, `control_image`, `ip_adapter_image`,
   *  `ref_images`, `lora`, `hires`, `cancel_queued`… verbatim from the engine,
   *  so a flag this app has never heard of still arrives. */
  features: Record<string, boolean>;
}

export interface GenerationEngineStatus {
  supported: boolean;
  engineInstalled: boolean;
  loadedModelId: string | null;
  totalRamBytes: number;
}

/** The engine's high-resolution fix: sample, upscale, denoise again. */
export interface HiresSettings {
  scale: number;
  /** 0 reuses the first pass's step count. */
  steps: number;
  denoisingStrength: number;
  upscaler: string;
}

/** A loose weight file in the library — a CLIP, a text encoder, a VAE. A model
 *  entry has to be a whole model, so the pieces shared between models live
 *  here instead: added once in the Models tab, picked per generation. */
export interface PartAsset {
  slot: ComponentSlot;
  name: string;
  path: string;
}

/** One per-run choice: fill `slot` with this library part. */
export interface ComponentOverride {
  slot: ComponentSlot;
  path: string;
}

/**
 * Splits the extra-engine-arguments field into the argv the engine is handed.
 *
 * Quote-aware, because several `sd-server` flags take a path and a path can
 * contain a space: `--hires-upscalers-dir`, `--embd-dir`, `--upscale-model`,
 * `--ad-model`. Splitting on whitespace turned `--embd-dir /My Weights/embs`
 * into three arguments, and the engine either rejected the unknown ones or read
 * a truncated path — so those flags were unusable for anyone whose folder had a
 * space in it, which on macOS is most people.
 *
 * Deliberately not a shell: no variable expansion, no globbing, no backslash
 * escapes. Arguments go straight to `Command::args`, never through a shell, and
 * a parser that pretended otherwise would imply substitutions that cannot
 * happen. Quotes group, and a quote is included literally by using the other
 * kind around it.
 */
export function parseLaunchArgs(value: string): string[] {
  const args: string[] = [];
  let current = "";
  let quote: '"' | "'" | null = null;
  let started = false;
  for (const character of value) {
    if (quote) {
      if (character === quote) quote = null;
      else current += character;
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
      // An empty quoted string is still an argument the user typed.
      started = true;
      continue;
    }
    if (/\s/.test(character)) {
      if (started) args.push(current);
      current = "";
      started = false;
      continue;
    }
    current += character;
    started = true;
  }
  if (started) args.push(current);
  return args;
}

/** Renders argv back into the field, re-quoting anything that would not
 *  survive [`parseLaunchArgs`]. Without this the round trip through the input
 *  loses the grouping the user typed the moment they edit the field again. */
export function formatLaunchArgs(args: string[]): string {
  return args
    .map((argument) => {
      if (argument === "") return '""';
      if (!/[\s"']/.test(argument)) return argument;
      // Single quotes unless the argument contains one, in which case double
      // quotes group it — matching what the parser accepts.
      return argument.includes("'") ? `"${argument}"` : `'${argument}'`;
    })
    .join(" ");
}

/** The value given to `flag` in an argument list, or null when it is absent.
 *
 *  A flag the user typed into the args field by hand and one a picker wrote are
 *  the same thing on the launch line, so both read back through here rather than
 *  the UI keeping a second copy that could disagree with what actually runs. */
export function launchArgValue(args: string[], flag: string): string | null {
  const at = args.indexOf(flag);
  if (at < 0) return null;
  const value = args[at + 1];
  // A trailing flag with nothing after it, or one followed by another flag, has
  // no value — treat it as absent rather than swallowing the next flag.
  return value === undefined || value.startsWith("--") ? null : value;
}

/** `args` with `flag` set to `value`, replacing an existing one in place, or
 *  removed entirely when `value` is null or blank. */
/** Whether a valueless flag like `--vae-tiling` is present. */
export function hasLaunchFlag(args: string[], flag: string): boolean {
  return args.includes(flag);
}

/**
 * Adds or removes a flag that carries no value.
 *
 * Separate from [`setLaunchArg`], which cannot express one: there, an empty
 * value *means* remove, so there is no way to say "present, with nothing after
 * it". Removal takes exactly one slot — a valueless flag has no argument, and
 * taking two would swallow whatever the user typed next.
 */
export function setLaunchFlag(args: string[], flag: string, on: boolean): string[] {
  const at = args.indexOf(flag);
  if (on) return at >= 0 ? [...args] : [...args, flag];
  if (at < 0) return [...args];
  return [...args.slice(0, at), ...args.slice(at + 1)];
}

export function setLaunchArg(args: string[], flag: string, value: string | null): string[] {
  const at = args.indexOf(flag);
  const trimmed = value?.trim() ?? "";
  // Replacing in place rather than removing and appending keeps the order the
  // user arranged, which is the whole reason the field stays editable by hand.
  const without =
    at < 0
      ? [...args]
      : [...args.slice(0, at), ...args.slice(at + (launchArgValue(args, flag) === null ? 1 : 2))];
  if (!trimmed) return without;
  return at < 0 ? [...without, flag, trimmed] : [...without.slice(0, at), flag, trimmed, ...without.slice(at)];
}

/** The slots the generation page offers a chooser for: everything the library
 *  has a part for. A denoiser is what the model *is*, so it is never one. */
export function choosableSlots(parts: PartAsset[]): ComponentSlot[] {
  const fixed: ComponentSlot[] = ["checkpoint", "diffusion_model"];
  return [...new Set(parts.map((part) => part.slot))].filter(
    (slot) => !fixed.includes(slot),
  );
}

export interface GenerationRequest {
  modelId: string;
  task: GenerationTask;
  prompt: string;
  negativePrompt: string;
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  /** Empty falls back to the model's own default. */
  sampleMethod: string;
  /** Empty leaves the engine's own sigma schedule. */
  scheduler: string;
  /** -1 is whatever the model was trained with. */
  clipSkip: number;
  eta: number | null;
  /** How far an init image is redrawn. Only used by the image-driven tasks. */
  strength: number | null;
  /** The second, higher-resolution pass. Null disables it. */
  hires: HiresSettings | null;
  seed: number;
  /** How many images to sample from this one prompt. Image tasks only — a
   *  clip and an utterance are one artifact per run. */
  batchCount: number;
  videoFrames: number;
  fps: number;
  /** Speech only: a reference clip whose voice the utterance is spoken in. */
  speakerFile: string | null;
  /** Speech only: ISO 639-1 code. Null leaves the model's own default. */
  language: string | null;
  initImageBase64: string | null;
  /** Single-channel mask over the init image: white is repainted, black is
   *  kept. This is inpainting, so it needs an init image to paint over. */
  maskImageBase64: string | null;
  /** ADetailer's own prompts. Null inherits the main ones, which is the
   *  engine's default — so they are sent only when deliberately different. */
  adPrompt: string | null;
  adNegativePrompt: string | null;
  /** A *pre-processed* control image — depth map, pose skeleton, edge map. The
   *  engine runs no detector, so a plain photograph is taken as structure. */
  controlImageBase64: string | null;
  /** How strongly the control image binds. Null leaves the engine default. */
  controlStrength: number | null;
  /** A reference image whose style/content is borrowed. */
  ipAdapterImageBase64: string | null;
  ipAdapterStrength: number | null;
  /** Reference images for the identity- and edit-conditioned architectures. */
  refImagesBase64: string[];
  increaseRefIndex: boolean;
  loras: LoraSelection[];
  componentOverrides: ComponentOverride[];
}

/** How a remote backend is spoken to. */
export type RemoteBackendKind = "comfy_ui" | "open_ai_compatible";

/** One registered remote generation endpoint: a ComfyUI the user runs, or a
 *  hosted OpenAI-compatible image API. Neither ships with the app — both are
 *  reached over HTTP, which is what keeps ComfyUI's GPL-3.0 at arm's length. */
export interface RemoteBackend {
  id: string;
  label: string;
  kind: RemoteBackendKind;
  /** Empty on an OpenAI-compatible backend falls back to the provider's base. */
  baseUrl: string;
  /** Which saved provider key authenticates. OpenAI-compatible only. */
  providerId: string | null;
  /** API-format workflow with `{{prompt}}`-style placeholders. ComfyUI only. */
  workflowTemplate: unknown | null;
  /** Whether this endpoint accepts an init image on `/images/edits`. */
  supportsEditing: boolean;
  /** The model names to offer in the picker. */
  models: string[];
}

/** The `modelId` prefix that routes a run to a backend instead of the managed
 *  engine. Backend and library models share one picker, so they share one id
 *  space. */
export const REMOTE_MODEL_PREFIX = "remote:";

export function remoteModelId(backendId: string, model: string): string {
  return `${REMOTE_MODEL_PREFIX}${backendId}:${model}`;
}

export function isRemoteModelId(modelId: string | null): boolean {
  return modelId !== null && modelId.startsWith(REMOTE_MODEL_PREFIX);
}

/** Presents each backend's models as library entries so the picker, the task
 *  filter and the run path need no notion of a backend at all.
 *
 *  The fields a backend genuinely has no answer for are not guessed: it weighs
 *  nothing locally, needs no download and carries no license to accept, so
 *  those read as installed, zero-byte and accepted rather than as a model stuck
 *  half-configured. */
export function backendModels(backends: RemoteBackend[]): GenerationModel[] {
  return backends.flatMap((backend) =>
    backend.models.map((model) => ({
      id: remoteModelId(backend.id, model),
      name: model,
      family: backend.label,
      tasks:
        backend.kind === "open_ai_compatible" && backend.supportsEditing
          ? (["text_to_image", "image_to_image"] as GenerationTask[])
          : (["text_to_image"] as GenerationTask[]),
      components: [],
      defaults: {
        width: 1024,
        height: 1024,
        steps: 25,
        cfgScale: 7,
        sampleMethod: "",
        flowShift: null,
        fps: 1,
        videoFrames: 1,
        // Never read: a backend offers no video task for it to constrain.
        frameGrid: "down_to4n_plus1" as FrameGrid,
      },
      minRamBytes: 0,
      license: {
        id: "",
        name: "",
        url: "",
        excludedTerritories: [],
        acceptanceRequired: false,
      },
      extraLaunchArgs: [],
      // Never launched locally, so the field only has to be a valid one.
      engine: "stable_diffusion_cpp" as GenerationEngineKind,
      installed: true,
      totalBytes: 0,
      missingBytes: 0,
      licenseAccepted: true,
      fitsInMemory: true,
    })),
  );
}

export interface GenerationProgressPayload {
  jobId: string;
  phase: string;
  queuePosition: number;
  /** Absent while the engine is still loading weights, which it does not
   *  count towards the sampling pass. */
  percent: number | null;
  step: number | null;
  totalSteps: number | null;
}

export const studioClient = {
  engineStatus: () => invoke<GenerationEngineStatus>("generation_engine_status"),
  /** Null when no engine is running, which is every moment before the first
   *  generation — the caller falls back to the compiled-in lists. */
  capabilities: () => invoke<EngineCapabilities | null>("generation_capabilities"),
  models: () => invoke<GenerationModel[]>("generation_models"),
  addModel: (spec: GenerationModelSpec) =>
    invoke<GenerationModelSpec[]>("generation_add_model", { spec }),
  removeModel: (modelId: string) =>
    invoke<void>("generation_remove_model", { modelId }),
  acceptLicense: (licenseId: string) =>
    invoke<void>("generation_accept_license", { licenseId }),
  downloadModel: (modelId: string) =>
    invoke<void>("generation_download_model", { modelId }),
  cancelDownload: (modelId: string) =>
    invoke<boolean>("generation_cancel_download", { modelId }),
  parts: () => invoke<PartAsset[]>("generation_parts"),
  addPart: (asset: PartAsset) => invoke<PartAsset[]>("generation_add_part", { asset }),
  removePart: (path: string) => invoke<PartAsset[]>("generation_remove_part", { path }),
  loras: () => invoke<LoraAsset[]>("generation_loras"),
  addLora: (asset: LoraAsset) => invoke<LoraAsset[]>("generation_add_lora", { asset }),
  removeLora: (path: string) => invoke<LoraAsset[]>("generation_remove_lora", { path }),
  backends: () => invoke<RemoteBackend[]>("generation_backends"),
  addBackend: (backend: RemoteBackend) =>
    invoke<RemoteBackend[]>("generation_add_backend", { backend }),
  removeBackend: (backendId: string) =>
    invoke<RemoteBackend[]>("generation_remove_backend", { backendId }),
  /** Resolves to every artifact the run produced: one for a clip or an
   *  utterance, `batchCount` of them for a batch of images. */
  run: (request: GenerationRequest) =>
    invoke<GenerationEntry[]>("generation_run", { request }),
  cancel: (jobId: string) => invoke<boolean>("generation_cancel", { jobId }),
  gallery: () => invoke<GenerationEntry[]>("generation_gallery"),
  deleteEntry: (entryId: string) =>
    invoke<void>("generation_delete_entry", { entryId }),
  mediaDataUrl: (artifactId: string) =>
    invoke<string>("generation_media_data_url", { artifactId }),
  unloadEngine: () => invoke<void>("generation_unload_engine"),
  onProgress: (
    listener: (payload: GenerationProgressPayload) => void,
  ): Promise<UnlistenFn> =>
    listen<GenerationProgressPayload>("studio://progress", (event) =>
      listener(event.payload),
    ),
};

/** The stored half of a library model, without the fields the backend
 *  measured. Sending a view straight back to `addModel` would persist those as
 *  if the user had typed them. */
export function toSpec(model: GenerationModel): GenerationModelSpec {
  const {
    installed: _installed,
    totalBytes: _totalBytes,
    missingBytes: _missingBytes,
    licenseAccepted: _licenseAccepted,
    fitsInMemory: _fitsInMemory,
    ...spec
  } = model;
  return spec;
}

/** Each family snaps clip length differently — Wan rounds down onto `4n + 1`,
 *  MiniMax H3 rounds up onto `17k + 5`. The slider has to snap the same way the
 *  backend will, or the duration it shows is one the clip never has. Mirrors
 *  `normalize_video_frames` in generation.rs. */
export function normalizeVideoFrames(grid: FrameGrid, value: number): number {
  const clamped = Math.min(Math.max(Math.trunc(value), 1), 361);
  if (grid === "down_to4n_plus1") {
    if (clamped < 5) return 1;
    return Math.floor((clamped - 1) / 4) * 4 + 1;
  }
  if (clamped <= 5) return 5;
  const steps = Math.ceil((clamped - 5) / 17);
  const aligned = steps * 17 + 5;
  return aligned > 361 ? (steps - 1) * 17 + 5 : aligned;
}

/** Canvas edges must be multiples of 32, rounded up as the backend does. */
export function normalizeDimension(value: number): number {
  const clamped = Math.min(Math.max(Math.trunc(value), 32), 4096);
  return Math.min(Math.ceil(clamped / 32) * 32, 4096);
}

/** The same grid, taken to the nearest edge rather than the one above.
 *
 *  What the controls set, because a size asked for is a size wanted: 645 is
 *  answered with 640 rather than the 672 the backend would round it to, which
 *  is four percent larger than the picture the user was looking at. The two
 *  never disagree about what gets rendered — the controls only ever hand the
 *  backend a value already on the grid, and [normalizeDimension] leaves those
 *  alone. It stays the truth for sizes that arrive from elsewhere. */
export function alignDimension(value: number): number {
  const clamped = Math.min(Math.max(Math.trunc(value), 32), 4096);
  return Math.min(Math.round(clamped / 32) * 32, 4096);
}

export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 GB";
  const gigabytes = bytes / 1_000_000_000;
  if (gigabytes >= 1) return `${gigabytes.toFixed(1)} GB`;
  return `${Math.max(1, Math.round(bytes / 1_000_000))} MB`;
}

export function isVideoTask(task: GenerationTask): boolean {
  return task === "text_to_video" || task === "image_to_video";
}

export function needsInitImage(task: GenerationTask): boolean {
  return task === "image_to_image" || task === "image_to_video";
}

/** Speech runs on llama-tts, which has no canvas and no sampler — the whole
 *  diffusion control set is meaningless for it. */
export function isSpeechTask(task: GenerationTask): boolean {
  return task === "text_to_speech";
}

/** The task that takes an existing asset back as its starting point, so a
 *  generated image can be edited by generating from it. */
export function editTaskFor(model: GenerationModel): GenerationTask | null {
  return (
    (["image_to_image", "image_to_video"] as GenerationTask[]).find((task) =>
      model.tasks.includes(task),
    ) ?? null
  );
}

/** Every slot the engine exposes, with the flag it maps to. Shown verbatim in
 *  the picker because the flag is the least ambiguous label there is — the app
 *  never guesses which slot a file fills. */
export const COMPONENT_SLOTS: { slot: ComponentSlot; flag: string }[] = [
  { slot: "checkpoint", flag: "--model" },
  { slot: "diffusion_model", flag: "--diffusion-model" },
  { slot: "high_noise_diffusion_model", flag: "--high-noise-diffusion-model" },
  { slot: "clip_l", flag: "--clip_l" },
  { slot: "clip_g", flag: "--clip_g" },
  { slot: "clip_vision", flag: "--clip_vision" },
  { slot: "t5xxl", flag: "--t5xxl" },
  { slot: "llm", flag: "--llm" },
  { slot: "llm_vision", flag: "--llm_vision" },
  { slot: "uncond_diffusion_model", flag: "--uncond-diffusion-model" },
  { slot: "embeddings_connectors", flag: "--embeddings-connectors" },
  { slot: "motion_module", flag: "--motion-module" },
  { slot: "vae", flag: "--vae" },
  { slot: "audio_vae", flag: "--audio-vae" },
  { slot: "taesd", flag: "--taesd" },
  { slot: "control_net", flag: "--control-net" },
  { slot: "ip_adapter", flag: "--ip-adapter" },
  { slot: "photo_maker", flag: "--photo-maker" },
  { slot: "pulid_weights", flag: "--pulid-weights" },
  { slot: "ad_model", flag: "--ad-model" },
  { slot: "mmproj", flag: "--mmproj" },
  { slot: "vocoder", flag: "--model-vocoder" },
];

export const ALL_TASKS: GenerationTask[] = [
  "text_to_image",
  "image_to_image",
  "text_to_video",
  "image_to_video",
  "text_to_speech",
];

/** Images one run may ask for. Mirrors the backend's own ceiling — the engine
 *  samples a batch serially, so a bigger number is a longer run, not a wider
 *  one, and the request is rejected rather than clamped above this. */
export const MAX_BATCH_COUNT = 8;

/** Samplers the pinned engine accepts, in the order it reports them. Used only
 *  until a running engine can be asked — see [EngineCapabilities]. */
export const SAMPLERS = [
  "euler",
  "euler_a",
  "heun",
  "dpm2",
  "dpm++2s_a",
  "dpm++2m",
  "dpm++2mv2",
  "ipndm",
  "ipndm_v",
  "lcm",
  "ddim_trailing",
  "tcd",
  "res_multistep",
  "res_2s",
  "er_sde",
  "euler_cfg_pp",
  "euler_a_cfg_pp",
  "euler_ge",
  "dpm++2m_sde",
  "dpm++2m_sde_bt",
  "lms",
];

/** Sigma schedules the pinned engine accepts, in the order it reports them.
 *  The fallback for [EngineCapabilities.schedulers]. */
export const SCHEDULERS = [
  "discrete",
  "normal",
  "karras",
  "exponential",
  "ays",
  "gits",
  "sgm_uniform",
  "simple",
  "smoothstep",
  "kl_optimal",
  "lcm",
  "bong_tangent",
  "ltx2",
  "logit_normal",
  "flux2",
  "flux",
  "beta",
];

/** The engine's built-in upscalers, as the fallback for
 *  [EngineCapabilities.upscalers] — which is the same list plus whatever the
 *  engine found on disk. Offered as suggestions rather than a closed list
 *  either way: a model dropped in the directory passed to
 *  `--hires-upscalers-dir` joins them under its own name, which is how an
 *  R-ESRGAN becomes selectable before the engine has ever run. */
export const UPSCALERS = [
  "Latent",
  "Latent (nearest)",
  "Latent (nearest-exact)",
  "Latent (antialiased)",
  "Latent (bicubic)",
  "Latent (bicubic antialiased)",
  "Lanczos",
  "Nearest",
  "None",
];

/** Canvas presets, matching the sizes these families are trained at. */
export const ASPECT_PRESETS: { id: string; width: number; height: number }[] = [
  { id: "portrait", width: 768, height: 1152 },
  { id: "landscape", width: 1152, height: 768 },
  { id: "square", width: 1024, height: 1024 },
];

/** A blank model, ready for the user to fill in. */
export function emptyModelSpec(): GenerationModelSpec {
  return {
    id: "",
    name: "",
    family: "",
    tasks: [],
    components: [],
    defaults: {
      width: 1024,
      height: 1024,
      steps: 20,
      cfgScale: 7,
      sampleMethod: "euler",
      flowShift: null,
      fps: 24,
      videoFrames: 33,
      frameGrid: "down_to4n_plus1",
    },
    minRamBytes: 0,
    license: {
      id: "",
      name: "",
      url: "",
      excludedTerritories: [],
      acceptanceRequired: false,
    },
    extraLaunchArgs: [],
    engine: "stable_diffusion_cpp",
  };
}
