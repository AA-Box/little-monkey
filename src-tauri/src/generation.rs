//! Model-agnostic image and video generation over the managed
//! stable-diffusion.cpp runtime.
//!
//! `sd-server` binds its model set at launch: every weight file is a
//! command-line flag, not a per-request field. So a generation model here is a
//! *set* of component files ([`GenerationModelSpec`]), and switching models
//! means relaunching the server. That is the same shape as the managed
//! `llama-server` instances in `llama.rs`, and the spawn/health-poll/kill body
//! below deliberately mirrors that module.
//!
//! Nothing in this file is specific to one model, and nothing is built in:
//! every model is one the user added, so Flux, LTX or HunyuanVideo is a set of
//! component slots and a table of defaults rather than new code. Wan 2.2 wants
//! `--t5xxl` and a plain VAE, MiniMax H3 wants `--llm` plus a second
//! `--audio-vae`, and both reach the same argv builder.
//!
//! Speech is the one task that leaves this shape. It is served by `llama-tts`,
//! a one-shot process with no server and no job queue, so it has its own
//! command-line builder ([`speech_args`]) and never touches `sd-server`.
//!
//! This module is Tauri-free so the desktop commands and the CLI can share it.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_STEPS: u32 = 200;
/// 15 s at 24 fps, the longest clip any currently supported model produces.
const MAX_VIDEO_FRAMES: u32 = 361;
const MAX_FPS: u32 = 60;
/// Text-encoder layers a request may skip. Beyond a handful the conditioning
/// is gone rather than stylized.
const MAX_CLIP_SKIP: i32 = 12;
/// Ceiling on the high-resolution pass's multiplier.
const MAX_HIRES_SCALE: f64 = 4.0;
const MAX_INIT_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// LoRAs per generation. The engine accepts an unbounded list; this exists so
/// one request cannot make the engine open an arbitrary number of files.
const MAX_LORAS: usize = 32;
/// A 15 s 2K clip with audio stays far under this; it exists so a runaway
/// server response can never be buffered without bound.
const MAX_MEDIA_BYTES: usize = 256 * 1024 * 1024;
/// Weights are tens of gigabytes and are read lazily from disk on first use,
/// so first-token latency after launch is dominated by IO, not compute.
const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// One `sd-server` command-line weight slot. The mapping to a flag is the
/// entire model-specific surface: a new architecture picks different slots
/// rather than needing different code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentSlot {
    /// A single all-in-one checkpoint (`-m`), as SD1.x/SDXL ship.
    Checkpoint,
    DiffusionModel,
    /// Wan 2.2's A14B mixture routes high-noise timesteps to a second model.
    HighNoiseDiffusionModel,
    ClipL,
    ClipG,
    ClipVision,
    T5xxl,
    /// A large language model used as the text encoder (Qwen3-VL for MiniMax
    /// H3, Qwen2.5-VL for Qwen Image, Mistral for FLUX.2).
    Llm,
    Vae,
    /// Models that generate synchronized audio decode it through its own VAE.
    AudioVae,
    Taesd,
    /// Speech only: the multimodal projector beside a TTS backbone. Belongs to
    /// a different engine than every slot above it.
    Mmproj,
    /// Speech, older shape: a standalone vocoder. The pinned speech runtime
    /// takes an [`Self::Mmproj`] instead, but the slot stays so a library
    /// entry written against another build still loads and still says what it
    /// meant, rather than failing the whole registry to parse.
    Vocoder,
}

impl ComponentSlot {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Checkpoint => "--model",
            Self::DiffusionModel => "--diffusion-model",
            Self::HighNoiseDiffusionModel => "--high-noise-diffusion-model",
            Self::ClipL => "--clip_l",
            Self::ClipG => "--clip_g",
            Self::ClipVision => "--clip_vision",
            Self::T5xxl => "--t5xxl",
            Self::Llm => "--llm",
            Self::Vae => "--vae",
            Self::AudioVae => "--audio-vae",
            Self::Taesd => "--taesd",
            Self::Mmproj => "--mmproj",
            Self::Vocoder => "--model-vocoder",
        }
    }

    /// Whether this slot belongs to `llama-tts` rather than `sd-server`. The
    /// two engines share no flags, so each builder consults this rather than
    /// listing the other's slots and drifting out of step.
    pub fn is_speech_only(self) -> bool {
        matches!(self, Self::Mmproj | Self::Vocoder)
    }
}

/// One downloadable weight file filling one slot.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelComponent {
    pub slot: ComponentSlot,
    pub source: ComponentSource,
    pub size_bytes: u64,
}

/// Where a component's bytes come from.
///
/// The engine takes an absolute path per slot and does not care how the file
/// got there, so a model the user already has on disk is a first-class source
/// rather than a special case bolted onto the curated list.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComponentSource {
    /// Fetched from Hugging Face into the app's own model directory.
    HuggingFace {
        repo: String,
        /// Path within the repo. The basename is what lands on disk.
        file: String,
    },
    /// A file the user already has. Referenced where it lies and never copied,
    /// moved, or deleted by the app — these weights are not ours to manage.
    LocalFile { path: String },
}

impl ModelComponent {
    pub fn huggingface(slot: ComponentSlot, repo: &str, file: &str, size_bytes: u64) -> Self {
        Self {
            slot,
            source: ComponentSource::HuggingFace {
                repo: repo.to_string(),
                file: file.to_string(),
            },
            size_bytes,
        }
    }

    /// The flat on-disk name a downloaded component takes. Two components of
    /// one model sharing a basename would overwrite each other, which
    /// [`validate_model_spec`] rejects before the model is stored.
    pub fn file_name(&self) -> &str {
        match &self.source {
            ComponentSource::HuggingFace { file, .. } => {
                file.rsplit('/').next().unwrap_or(file.as_str())
            }
            ComponentSource::LocalFile { path } => path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(path.as_str()),
        }
    }

    /// Where this component actually lives once available.
    pub fn resolved_path(&self, model_root: &Path, model_id: &str) -> PathBuf {
        match &self.source {
            ComponentSource::HuggingFace { .. } => {
                model_root.join(model_id).join(self.file_name())
            }
            ComponentSource::LocalFile { path } => PathBuf::from(path),
        }
    }

    /// Only downloadable components can be fetched; a user's own file is
    /// present or it is not.
    pub fn is_downloadable(&self) -> bool {
        matches!(self.source, ComponentSource::HuggingFace { .. })
    }
}

/// A LoRA in the user's library.
///
/// Added once, then picked by name for each run. The engine still takes an
/// absolute path per generation — this is only so the user names the file with
/// a file picker instead of typing its path into the generation page every
/// time. Like a model's own weights, the file is referenced where it lies and
/// never copied, moved or deleted by the app.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoraAsset {
    pub name: String,
    pub path: String,
}

/// Validates a library LoRA before it is stored.
///
/// Existence is deliberately not checked here: this is the Tauri-free core and
/// the caller that has a real filesystem does that, so a missing file is caught
/// with a real message at the boundary rather than in a pure function.
pub fn validate_lora_asset(asset: &LoraAsset) -> Result<(), String> {
    if asset.name.trim().is_empty() || asset.name.len() > 200 {
        return Err("A LoRA needs a name".to_string());
    }
    if !Path::new(&asset.path).is_absolute() {
        return Err("A LoRA needs an absolute path".to_string());
    }
    Ok(())
}

/// One LoRA applied to a generation. The engine accepts an unbounded list, so
/// this is a per-request stack rather than a single slot.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoraSelection {
    /// Absolute path to the LoRA file.
    pub path: String,
    /// Strength. Negative values are meaningful — they subtract a style.
    pub multiplier: f64,
    /// Applies only to a mixture model's high-noise stage (Wan 2.2 A14B).
    #[serde(default)]
    pub is_high_noise: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationTask {
    TextToImage,
    ImageToImage,
    TextToVideo,
    ImageToVideo,
    /// Speech, optionally in a voice cloned from a reference clip. Served by
    /// `llama-tts` rather than `sd-server` — see [`speech_args`].
    TextToSpeech,
}

impl GenerationTask {
    pub fn is_video(self) -> bool {
        matches!(self, Self::TextToVideo | Self::ImageToVideo)
    }

    pub fn needs_init_image(self) -> bool {
        matches!(self, Self::ImageToImage | Self::ImageToVideo)
    }

    /// Speech runs on a different engine entirely, so callers must route it
    /// before reaching for anything `sd-server`-shaped.
    pub fn is_speech(self) -> bool {
        matches!(self, Self::TextToSpeech)
    }

    /// The native async endpoint this task submits to.
    pub fn endpoint(self) -> &'static str {
        if self.is_video() {
            "/sdcpp/v1/vid_gen"
        } else {
            "/sdcpp/v1/img_gen"
        }
    }
}

/// A model's terms, surfaced and accepted before its weights are fetched.
///
/// `excluded_territories` is not decoration: MiniMax H3's community license
/// defines its Applicable Territory as worldwide *excluding* the EU, UK, South
/// Korea and the USA, so the app must never mirror those weights and must show
/// the terms before the user's own download.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseGate {
    pub id: String,
    pub name: String,
    pub url: String,
    pub excluded_territories: Vec<String>,
    pub acceptance_required: bool,
}

/// How a family rounds a requested clip length. Not cosmetic: asking for a
/// count off the grid gets silently rewritten by the backend, so the UI has to
/// round the same way or it promises a duration the clip will not have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameGrid {
    /// Largest `4n + 1` at or below the request — the core video path's
    /// general normalization, used by Wan.
    #[default]
    DownTo4nPlus1,
    /// Smallest `17k + 5` at or above the request, minimum 5 — MiniMax H3.
    UpTo17kPlus5,
}

/// Per-model starting point for the request fields a user does not set.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationDefaults {
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f64,
    pub sample_method: String,
    /// `None` leaves the backend's own choice in place.
    pub flow_shift: Option<f64>,
    pub fps: u32,
    pub video_frames: u32,
    pub frame_grid: FrameGrid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationModelSpec {
    pub id: String,
    pub name: String,
    /// Human-readable architecture family, shown in the picker.
    pub family: String,
    pub tasks: Vec<GenerationTask>,
    pub components: Vec<ModelComponent>,
    pub defaults: GenerationDefaults,
    /// Total RAM (or unified memory) below which this model will swap rather
    /// than run. Checked before launch so the failure is a sentence, not a
    /// forty-minute stall.
    pub min_ram_bytes: u64,
    pub license: LicenseGate,
    /// Model-specific `sd-server` launch flags beyond the component slots.
    pub extra_launch_args: Vec<String>,
}

impl GenerationModelSpec {
    pub fn total_bytes(&self) -> u64 {
        self.components.iter().map(|entry| entry.size_bytes).sum()
    }

    pub fn supports(&self, task: GenerationTask) -> bool {
        self.tasks.contains(&task)
    }

    /// Every component file this model needs, in slot order.
    pub fn component_paths(&self, model_root: &Path) -> Vec<PathBuf> {
        self.components
            .iter()
            .map(|component| component.resolved_path(model_root, &self.id))
            .collect()
    }

    /// What this model actually weighs, measured rather than declared.
    ///
    /// `size_bytes` is only ever a promise about a file that has not arrived
    /// yet: a component the user pointed at on their own disk has no declared
    /// size at all, and a downloaded one has a real length that beats whatever
    /// the entry claimed. So every present file is stat'd and only the absent
    /// ones fall back to the declared number.
    pub fn size_on_disk(&self, model_root: &Path) -> u64 {
        self.components
            .iter()
            .map(|component| {
                std::fs::metadata(component.resolved_path(model_root, &self.id))
                    .map(|entry| entry.len())
                    .unwrap_or(component.size_bytes)
            })
            .sum()
    }

    /// Components still missing from disk, so the UI can show what a download
    /// would actually fetch rather than re-fetching a partially present set.
    pub fn missing_components(&self, model_root: &Path) -> Vec<&ModelComponent> {
        self.components
            .iter()
            .filter(|component| {
                component.is_downloadable()
                    && !component.resolved_path(model_root, &self.id).is_file()
            })
            .collect()
    }
}

/// Validates a user-defined model before it is stored or launched.
///
/// There is no built-in catalogue: every model in Studio was added by the
/// person using it, either as files already on their disk or as a Hugging Face
/// repo and file to fetch. This is the only gate between "what the user typed"
/// and "arguments handed to an engine", so it is deliberately strict about the
/// things that would otherwise fail deep inside the engine or, worse, silently.
pub fn validate_model_spec(spec: &GenerationModelSpec) -> Result<(), String> {
    if spec.id.trim().is_empty() || spec.id.len() > 128 {
        return Err("A model needs an id".to_string());
    }
    // The id becomes a directory name under the app's model root, so it must
    // not be able to climb out of it.
    if !spec
        .id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || spec.id.starts_with('.')
    {
        return Err(
            "A model id may only contain letters, digits, dots, dashes and underscores".to_string(),
        );
    }
    if spec.name.trim().is_empty() || spec.name.len() > 200 {
        return Err("A model needs a name".to_string());
    }
    if spec.tasks.is_empty() {
        return Err("Pick at least one thing this model can do".to_string());
    }
    if spec.components.is_empty() {
        return Err("A model needs at least one weight file".to_string());
    }
    let denoisers = spec
        .components
        .iter()
        .filter(|component| {
            matches!(
                component.slot,
                ComponentSlot::Checkpoint | ComponentSlot::DiffusionModel
            )
        })
        .count();
    if denoisers != 1 {
        return Err(
            "Exactly one file must be the checkpoint or the diffusion model".to_string(),
        );
    }
    let mut slots = BTreeSet::new();
    let mut names = BTreeSet::new();
    for component in &spec.components {
        if !slots.insert(component.slot) {
            return Err(format!(
                "Two files are both assigned to {}",
                component.slot.flag()
            ));
        }
        // Components land in one flat directory per model, so two files
        // sharing a basename would overwrite each other.
        if !names.insert(component.file_name().to_string()) {
            return Err(format!("Two files are both named {}", component.file_name()));
        }
        match &component.source {
            ComponentSource::HuggingFace { repo, file } => {
                if repo.trim().is_empty() || file.trim().is_empty() {
                    return Err("A downloaded file needs a repo and a path".to_string());
                }
                if repo.contains("..") || file.contains("..") {
                    return Err("Repo and file paths may not contain ..".to_string());
                }
            }
            ComponentSource::LocalFile { path } => {
                if !Path::new(path).is_absolute() {
                    return Err("A file on this machine needs an absolute path".to_string());
                }
            }
        }
    }
    if spec.defaults.steps == 0 || spec.defaults.steps > MAX_STEPS {
        return Err(format!("Steps must be between 1 and {MAX_STEPS}"));
    }
    if spec.defaults.fps == 0 || spec.defaults.fps > MAX_FPS {
        return Err(format!("Frame rate must be between 1 and {MAX_FPS}"));
    }
    if spec.defaults.sample_method.trim().is_empty()
        || spec.defaults.sample_method.len() > 64
    {
        return Err("A model needs a sampling method".to_string());
    }
    for argument in &spec.extra_launch_args {
        if argument.trim().is_empty() || argument.len() > 256 {
            return Err("Engine arguments must be short and non-empty".to_string());
        }
    }
    Ok(())
}

/// Builds the `sd-server` command line for a model. Every weight path is
/// absolute and app-owned; nothing is read from a user shell or PATH.
pub fn launch_args(spec: &GenerationModelSpec, model_root: &Path, port: u16) -> Vec<String> {
    let mut args = vec![
        "--listen-ip".to_string(),
        "127.0.0.1".to_string(),
        "--listen-port".to_string(),
        port.to_string(),
    ];
    for component in &spec.components {
        // Speech slots belong to `llama-tts`; handing one of their flags to
        // `sd-server` is an unknown argument, not an unused one.
        if component.slot.is_speech_only() {
            continue;
        }
        args.push(component.slot.flag().to_string());
        args.push(
            component
                .resolved_path(model_root, &spec.id)
                .to_string_lossy()
                .to_string(),
        );
    }
    args.extend(spec.extra_launch_args.iter().cloned());
    args
}

/// What a running engine was launched with, port aside.
///
/// A warm engine is reused on this rather than on the model id. Editing a
/// model's files — adding the VAE it was missing, swapping a text encoder —
/// leaves the id alone, so an id-keyed reuse would keep serving the old file
/// set until the app restarted, and the fix the user just made would appear to
/// do nothing.
fn launch_signature(spec: &GenerationModelSpec, model_root: &Path) -> Vec<String> {
    launch_args(spec, model_root, 0)
}

/// The weight file that identifies a loaded model to `sd-server`, which
/// reports it back as `model.path` in its capabilities.
fn identifying_component(spec: &GenerationModelSpec) -> Option<&ModelComponent> {
    spec.components.iter().find(|component| {
        matches!(
            component.slot,
            ComponentSlot::Checkpoint | ComponentSlot::DiffusionModel
        )
    })
}

/// Builds the `llama-tts` command line for one utterance.
///
/// Speech is not served by `sd-server` and is not a server at all: `llama-tts`
/// loads its weights, writes one wav and exits, so there is nothing to keep
/// warm and no job to poll. The model is still an ordinary library entry — the
/// backbone fills [`ComponentSlot::Checkpoint`] and its projector fills
/// [`ComponentSlot::Mmproj`], both of which map to flags `llama-tts` accepts.
///
/// Which slots a given build wants is the build's business, not this
/// function's: every speech slot the user assigned is passed through, and an
/// engine that does not know one says so itself.
pub fn speech_args(
    spec: &GenerationModelSpec,
    model_root: &Path,
    request: &GenerationRequest,
    output: &Path,
) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    for component in &spec.components {
        if !component.slot.is_speech_only() && component.slot != ComponentSlot::Checkpoint {
            continue;
        }
        args.push(component.slot.flag().to_string());
        args.push(
            component
                .resolved_path(model_root, &spec.id)
                .to_string_lossy()
                .to_string(),
        );
    }
    args.push("--prompt".to_string());
    args.push(request.prompt.clone());
    args.push("--output".to_string());
    args.push(output.to_string_lossy().to_string());
    // A reference clip is a plain wav: the engine listens to it and speaks the
    // prompt in that voice. This is the whole of voice cloning here.
    if let Some(path) = request
        .speaker_file
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("--tts-speaker-file".to_string());
        args.push(path.to_string());
    }
    if let Some(language) = request
        .language
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("--tts-lang".to_string());
        args.push(language.to_ascii_lowercase());
    }
    args.extend(spec.extra_launch_args.iter().cloned());
    Ok(args)
}

/// Snaps a canvas edge to the multiple of 32 the samplers require. The backend
/// aligns upward, so this does too — rounding the other way would quietly hand
/// back a smaller canvas than the one that gets rendered.
pub fn normalize_dimension(value: u32) -> u32 {
    let clamped = value.clamp(32, MAX_DIMENSION);
    let rounded = clamped.div_ceil(32) * 32;
    rounded.min(MAX_DIMENSION / 32 * 32)
}

/// Snaps a requested clip length onto the family's grid, matching what the
/// backend will do with the same number.
pub fn normalize_video_frames(grid: FrameGrid, value: u32) -> u32 {
    let clamped = value.clamp(1, MAX_VIDEO_FRAMES);
    match grid {
        FrameGrid::DownTo4nPlus1 => {
            if clamped < 5 {
                1
            } else {
                ((clamped - 1) / 4) * 4 + 1
            }
        }
        FrameGrid::UpTo17kPlus5 => {
            if clamped <= 5 {
                return 5;
            }
            let steps = (clamped - 5).div_ceil(17);
            // Aligning upward can overshoot the cap, so step back one rung
            // rather than returning a length the backend would reject.
            let aligned = steps * 17 + 5;
            if aligned > MAX_VIDEO_FRAMES {
                (steps - 1) * 17 + 5
            } else {
                aligned
            }
        }
    }
}

/// The engine's high-resolution fix: sample at the requested canvas, upscale,
/// then denoise the larger image. Present means enabled — a disabled pass has
/// no settings worth carrying.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiresSettings {
    /// Multiplier on the canvas, so 2.0 turns 512×768 into 1024×1536.
    pub scale: f64,
    /// Steps for the second pass. Zero reuses the first pass's count.
    #[serde(default)]
    pub steps: u32,
    /// How far the upscaled image is re-sampled. 0 keeps it, 1 redraws it.
    pub denoising_strength: f64,
    /// Named upscaler. The built-in set is fixed, but a model dropped in the
    /// directory given to `--hires-upscalers-dir` joins it under its own name,
    /// which is why this is a free string rather than an enum.
    pub upscaler: String,
}

/// One generation job's user-controlled inputs.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationRequest {
    pub model_id: String,
    pub task: GenerationTask,
    pub prompt: String,
    #[serde(default)]
    pub negative_prompt: String,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f64,
    /// Sampler for this run. Empty falls back to the model's own default —
    /// the canvas and sampling controls belong to the generation, not to the
    /// library entry, so every one of them can be changed per run.
    #[serde(default)]
    pub sample_method: String,
    /// Sigma schedule. Empty leaves the backend's own choice in place.
    #[serde(default)]
    pub scheduler: String,
    /// How many text-encoder layers to skip. `-1` is the model's own setting.
    #[serde(default = "default_clip_skip")]
    pub clip_skip: i32,
    /// Sampler noise multiplier. `None` leaves the backend's default.
    #[serde(default)]
    pub eta: Option<f64>,
    /// How far an init image is redrawn: 0 keeps it, 1 ignores it. Only
    /// meaningful for the image-driven tasks.
    #[serde(default)]
    pub strength: Option<f64>,
    /// The second, higher-resolution pass. `None` disables it.
    #[serde(default)]
    pub hires: Option<HiresSettings>,
    /// Negative asks the backend for a random seed.
    pub seed: i64,
    #[serde(default)]
    pub video_frames: u32,
    #[serde(default)]
    pub fps: u32,
    /// Speech only: a reference clip whose voice the utterance is spoken in.
    #[serde(default)]
    pub speaker_file: Option<String>,
    /// Speech only: ISO 639-1 code. `None` leaves the model's own default.
    #[serde(default)]
    pub language: Option<String>,
    /// Base64 PNG/JPEG starting frame, required by the image-driven tasks.
    #[serde(default)]
    pub init_image_base64: Option<String>,
    /// LoRAs to apply, in order. Any model can take any number.
    #[serde(default)]
    pub loras: Vec<LoraSelection>,
    /// Per-run swaps of which library file fills one of this model's slots.
    #[serde(default)]
    pub component_overrides: Vec<ComponentOverride>,
}

/// One per-run swap: fill `slot` with the file another library model uses for
/// the same slot.
///
/// A model id rather than a path on purpose. The caller is the UI, and letting
/// it name an arbitrary absolute path would make every generation request a
/// way to hand the engine any file on the machine. Naming a library entry
/// keeps the set of loadable files exactly the set the user added.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentOverride {
    pub slot: ComponentSlot,
    pub model_id: String,
}

/// Rewrites `spec` so each override's slot is filled by the donor model's file.
///
/// The donor's file is resolved to an absolute path first: a component carries
/// no model id of its own, so one copied between entries unchanged would be
/// looked up under the *borrowing* model's directory and not be there.
///
/// Only a slot the model already loads can be swapped. Adding one it does not
/// have is a different model, not a setting, and belongs in the library rather
/// than in a run.
pub fn apply_component_overrides(
    spec: &GenerationModelSpec,
    donors: &[GenerationModelSpec],
    overrides: &[ComponentOverride],
    model_root: &Path,
) -> Result<GenerationModelSpec, String> {
    let mut effective = spec.clone();
    for swap in overrides {
        if swap.model_id == spec.id {
            continue;
        }
        let donor = donors
            .iter()
            .find(|entry| entry.id == swap.model_id)
            .ok_or_else(|| format!("{} is not in your library", swap.model_id))?;
        let source = donor
            .components
            .iter()
            .find(|component| component.slot == swap.slot)
            .ok_or_else(|| format!("{} has no {}", donor.name, swap.slot.flag()))?;
        let target = effective
            .components
            .iter_mut()
            .find(|component| component.slot == swap.slot)
            .ok_or_else(|| format!("{} does not load a {}", spec.name, swap.slot.flag()))?;
        target.source = ComponentSource::LocalFile {
            path: source
                .resolved_path(model_root, &donor.id)
                .to_string_lossy()
                .to_string(),
        };
        target.size_bytes = source.size_bytes;
    }
    Ok(effective)
}

/// `-1` means "whatever the model was trained with", which is the only sane
/// default for a setting most models do not want changed.
fn default_clip_skip() -> i32 {
    -1
}

/// Rejects a request that is out of bounds, and returns it with dimensions and
/// frame count snapped to what the backend will actually use.
pub fn validate_request(
    spec: &GenerationModelSpec,
    request: &GenerationRequest,
) -> Result<GenerationRequest, String> {
    if !spec.supports(request.task) {
        return Err(format!("{} does not support this task", spec.name));
    }
    if request.prompt.trim().is_empty() {
        return Err("A prompt is required".to_string());
    }
    if request.prompt.len() > MAX_PROMPT_BYTES || request.negative_prompt.len() > MAX_PROMPT_BYTES {
        return Err("Prompt exceeds its size limit".to_string());
    }
    // Speech runs on `llama-tts`, which has no canvas, no sampler and no
    // guidance — validating it against the diffusion bounds below would reject
    // a request over fields that do not reach the engine at all.
    if request.task.is_speech() {
        if let Some(path) = &request.speaker_file {
            if !path.trim().is_empty() && !Path::new(path).is_absolute() {
                return Err("A reference clip needs an absolute path".to_string());
            }
        }
        if let Some(code) = request
            .language
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err("Language must be a two-letter ISO 639-1 code".to_string());
            }
        }
        let mut normalized = request.clone();
        normalized.width = 0;
        normalized.height = 0;
        normalized.video_frames = 1;
        normalized.fps = 1;
        return Ok(normalized);
    }
    if request.sample_method.len() > 64 || request.scheduler.len() > 64 {
        return Err("Sampler name is too long".to_string());
    }
    if !(-1..=MAX_CLIP_SKIP).contains(&request.clip_skip) {
        return Err(format!("Clip skip must be between -1 and {MAX_CLIP_SKIP}"));
    }
    if let Some(eta) = request.eta {
        if !eta.is_finite() || !(0.0..=1.0).contains(&eta) {
            return Err("Eta must be between 0 and 1".to_string());
        }
    }
    if let Some(strength) = request.strength {
        if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
            return Err("Denoising strength must be between 0 and 1".to_string());
        }
    }
    if let Some(hires) = &request.hires {
        if !hires.scale.is_finite() || !(1.0..=MAX_HIRES_SCALE).contains(&hires.scale) {
            return Err(format!("Upscale must be between 1x and {MAX_HIRES_SCALE}x"));
        }
        if hires.steps > MAX_STEPS {
            return Err(format!("Upscale steps may not exceed {MAX_STEPS}"));
        }
        if !hires.denoising_strength.is_finite()
            || !(0.0..=1.0).contains(&hires.denoising_strength)
        {
            return Err("Upscale denoising strength must be between 0 and 1".to_string());
        }
        if hires.upscaler.trim().is_empty() || hires.upscaler.len() > 64 {
            return Err("Pick an upscaler".to_string());
        }
        // The canvas the second pass renders is bounded by the same ceiling as
        // the first, so an upscale that would blow past it is rejected here
        // rather than deep inside the engine.
        let scaled = (f64::from(request.width.max(request.height)) * hires.scale).round();
        if scaled > f64::from(MAX_DIMENSION) {
            return Err(format!(
                "{}× would exceed the {MAX_DIMENSION} px limit",
                hires.scale
            ));
        }
    }
    if request.steps == 0 || request.steps > MAX_STEPS {
        return Err(format!("Steps must be between 1 and {MAX_STEPS}"));
    }
    if !request.cfg_scale.is_finite() || !(0.0..=100.0).contains(&request.cfg_scale) {
        return Err("Guidance scale is out of range".to_string());
    }
    if request.width > MAX_DIMENSION || request.height > MAX_DIMENSION {
        return Err(format!("Canvas may not exceed {MAX_DIMENSION} px"));
    }
    match (request.task.needs_init_image(), &request.init_image_base64) {
        (true, None) => return Err("This task requires a source image".to_string()),
        (_, Some(encoded)) if encoded.len() > MAX_INIT_IMAGE_BYTES => {
            return Err("Source image exceeds its size limit".to_string())
        }
        _ => {}
    }

    if request.loras.len() > MAX_LORAS {
        return Err(format!("At most {MAX_LORAS} LoRAs can be applied at once"));
    }
    for lora in &request.loras {
        if lora.path.trim().is_empty() || !Path::new(&lora.path).is_absolute() {
            return Err("Each LoRA needs an absolute path".to_string());
        }
        if !lora.multiplier.is_finite() || !(-10.0..=10.0).contains(&lora.multiplier) {
            return Err("LoRA strength is out of range".to_string());
        }
    }

    let mut normalized = request.clone();
    normalized.width = normalize_dimension(request.width);
    normalized.height = normalize_dimension(request.height);
    if request.task.is_video() {
        let fps = if request.fps == 0 {
            spec.defaults.fps
        } else {
            request.fps
        };
        if fps > MAX_FPS {
            return Err(format!("Frame rate may not exceed {MAX_FPS}"));
        }
        normalized.fps = fps;
        normalized.video_frames = normalize_video_frames(
            spec.defaults.frame_grid,
            if request.video_frames == 0 {
                spec.defaults.video_frames
            } else {
                request.video_frames
            },
        );
    } else {
        normalized.fps = 1;
        normalized.video_frames = 1;
    }
    Ok(normalized)
}

/// Builds the `/sdcpp/v1/{img,vid}_gen` request body. Optional sampling fields
/// are omitted rather than guessed, so the backend's own defaults apply.
pub fn request_body(spec: &GenerationModelSpec, request: &GenerationRequest) -> Value {
    let sampler = if request.sample_method.trim().is_empty() {
        spec.defaults.sample_method.as_str()
    } else {
        request.sample_method.trim()
    };
    let mut sample_params = json!({
        "sample_method": sampler,
        "sample_steps": request.steps,
        "guidance": { "txt_cfg": request.cfg_scale },
    });
    if let Some(flow_shift) = spec.defaults.flow_shift {
        sample_params["flow_shift"] = json!(flow_shift);
    }
    if !request.scheduler.trim().is_empty() {
        sample_params["scheduler"] = json!(request.scheduler.trim());
    }
    if let Some(eta) = request.eta {
        sample_params["eta"] = json!(eta);
    }

    let mut body = json!({
        "prompt": request.prompt,
        "negative_prompt": request.negative_prompt,
        "width": request.width,
        "height": request.height,
        "seed": request.seed,
        "clip_skip": request.clip_skip,
        "sample_params": sample_params,
    });
    // Only the image-driven tasks have something to denoise away from.
    if let (true, Some(strength)) = (request.task.needs_init_image(), request.strength) {
        body["strength"] = json!(strength);
    }
    if let Some(hires) = &request.hires {
        body["hires"] = json!({
            "enabled": true,
            "scale": hires.scale,
            "steps": hires.steps,
            "denoising_strength": hires.denoising_strength,
            "upscaler": hires.upscaler.trim(),
        });
    }
    if !request.loras.is_empty() {
        body["lora"] = Value::Array(
            request
                .loras
                .iter()
                .map(|lora| {
                    json!({
                        "path": lora.path,
                        "multiplier": lora.multiplier,
                        "is_high_noise": lora.is_high_noise,
                    })
                })
                .collect(),
        );
    }
    if let Some(image) = &request.init_image_base64 {
        body["init_image"] = json!(image);
    }
    if request.task.is_video() {
        body["video_frames"] = json!(request.video_frames);
        body["fps"] = json!(request.fps);
        body["output_format"] = json!("webm");
    } else {
        body["output_format"] = json!("png");
    }
    body
}

/// A finished job's decoded payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedMedia {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub frame_count: u32,
    pub fps: u32,
}

/// Where a polled job currently stands. `Running` carries the queue position so
/// the UI can distinguish "waiting behind another job" from "sampling".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobProgress {
    Running { queue_position: u32 },
    Completed(Box<GeneratedMedia>),
    Failed(String),
    Cancelled,
}

/// Media type for a container format the engine names but does not classify.
/// `img_gen` states its `output_format` once for the batch and leaves the media
/// type to the caller; without this the artifact would be stored as an opaque
/// blob and the gallery could not tell an image from a clip.
fn media_type_for_format(format: &str) -> String {
    match format.trim().to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp4" => "video/mp4",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Reads one `GET /sdcpp/v1/jobs/{id}` body.
pub fn decode_job_status(value: &Value) -> Result<JobProgress, String> {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .ok_or("Generation job status is missing")?;
    match status {
        "queued" | "generating" => Ok(JobProgress::Running {
            queue_position: value
                .get("queue_position")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32,
        }),
        "cancelled" => Ok(JobProgress::Cancelled),
        "failed" => Ok(JobProgress::Failed(
            engine_error_text(value).unwrap_or("Generation failed").to_string(),
        )),
        "completed" => {
            let result = value
                .get("result")
                .ok_or("Completed generation job carried no result")?;
            // The two modes return different shapes. `img_gen` yields a list of
            // encoded images with no media type, since the format is stated once
            // for the batch; `vid_gen` yields one encoded container inline with
            // its own `mime_type`. Read whichever is present rather than
            // assuming a mode from the caller.
            let encoded = result
                .pointer("/images/0/b64_json")
                .or_else(|| result.get("b64_json"))
                .and_then(Value::as_str)
                .ok_or("Generation result carried no payload")?;
            // Reject before decoding: base64 is 4/3 the size of its payload, so
            // this bounds the allocation rather than discovering it afterwards.
            if encoded.len() / 4 * 3 > MAX_MEDIA_BYTES {
                return Err("Generated media exceeds its size limit".to_string());
            }
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| "Generation result is not valid base64".to_string())?;
            if bytes.is_empty() {
                return Err("Generated media is empty".to_string());
            }
            Ok(JobProgress::Completed(Box::new(GeneratedMedia {
                media_type: result
                    .get("mime_type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        result
                            .get("output_format")
                            .and_then(Value::as_str)
                            .map(media_type_for_format)
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                frame_count: result
                    .get("frame_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1) as u32,
                fps: result.get("fps").and_then(Value::as_u64).unwrap_or(1) as u32,
                bytes,
            })))
        }
        other => Err(format!("Unknown generation job status {other}")),
    }
}

/// Bytes of engine stderr kept for the failure message.
///
/// The engine's diagnostics are the only place a real cause appears — a weight
/// file quantized for a different loader reports "wrong shape in model
/// metadata" here and nothing but a non-zero exit code anywhere else. Small
/// because this is a message a person reads, not a model's context.
const MAX_STDERR_TAIL: usize = 4_000;

/// Engine failures whose own wording does not say what to change, paired with
/// the sentence that does. Both entries below were hit for real: the first by
/// putting an all-in-one checkpoint on the wrong slot, the second by a
/// quantization built for a different loader.
const ENGINE_FAILURE_HINTS: &[(&str, &str)] = &[
    (
        "get sd version from file failed",
        "The engine could not read that file as a bare diffusion model. An all-in-one checkpoint belongs on --model; --diffusion-model is for a UNet on its own.",
    ),
    (
        "wrong shape in model metadata",
        "That file's tensors are laid out for a different loader. A ComfyUI-GGUF quantization will not load here even though the extension matches — use one built for stable-diffusion.cpp.",
    ),
];

/// Reduces the engine's output to the part a person can act on.
///
/// A failed launch prints forty lines of Metal device probing before the two
/// that matter, so quoting the raw tail buries the diagnosis in noise. Keep the
/// engine's own error lines, and add the sentence that turns a known one into a
/// change the user can make.
fn engine_failure_detail(tail: &str) -> String {
    let errors: Vec<&str> = tail
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("[ERROR]") || line.starts_with("error"))
        .collect();
    let body = if errors.is_empty() {
        // Nothing self-identifies as an error, so the last few lines are the
        // best available guess at where it stopped.
        let lines: Vec<&str> = tail.lines().collect();
        lines[lines.len().saturating_sub(6)..].join("\n")
    } else {
        errors.join("\n")
    };
    match ENGINE_FAILURE_HINTS
        .iter()
        .find(|(signature, _)| tail.contains(signature))
    {
        Some((_, hint)) => format!("{body}\n\n{hint}"),
        None => body,
    }
}

/// Sampling progress read off one line of engine output.
///
/// The job API reports `queued`/`generating`/`completed` and nothing in
/// between — no step counter, no percentage — so the only place a real
/// completion figure exists is the progress bar the engine writes to stderr:
///
/// ```text
///   |========>                                         | 4/25 - 2.42s/it
/// ```
///
/// The same bar shape is used for tensor loading (`| 212/686 - 1.06GB/s`),
/// which is why the rate suffix, not the bar, decides whether a line counts.
fn parse_sampling_progress(line: &str) -> Option<(u32, u32)> {
    if !line.contains('|') {
        return None;
    }
    let tail = line.rsplit('|').next()?;
    if !(tail.contains("s/it") || tail.contains("it/s")) {
        return None;
    }
    let (done, total) = tail.split('-').next()?.trim().split_once('/')?;
    let done = done.trim().parse().ok()?;
    let total = total.trim().parse().ok()?;
    if total == 0 {
        return None;
    }
    Some((done, total))
}

/// Latest `(step, total)` scraped from the engine's progress bar, shared
/// between the reader threads and whoever is reporting progress.
type SamplingProgress = Arc<Mutex<Option<(u32, u32)>>>;

/// Consumes one of the engine's output streams on its own thread, keeping the
/// tail for failure messages and the latest step count for the progress bar.
///
/// Splits on carriage returns as well as newlines: the engine redraws its
/// progress bar with `\r`, so a newline-only reader would hold an entire run in
/// one unterminated line and report no progress until the job was already over.
fn drain_engine_output(
    stream: impl std::io::Read + Send + 'static,
    tail: Arc<Mutex<String>>,
    sampling: SamplingProgress,
) {
    std::thread::spawn(move || {
        use std::io::{BufReader, Read as _};
        let mut line: Vec<u8> = Vec::new();
        let flush = |line: &mut Vec<u8>| {
            if line.is_empty() {
                return true;
            }
            let text = String::from_utf8_lossy(line).to_string();
            line.clear();
            if let Some(progress) = parse_sampling_progress(&text) {
                let Ok(mut cell) = sampling.lock() else { return false };
                *cell = Some(progress);
                // A redrawn bar is noise in a failure message; the tail is for
                // the lines a person reads.
                return true;
            }
            let Ok(mut buffer) = tail.lock() else { return false };
            buffer.push_str(text.trim_end());
            buffer.push('\n');
            if buffer.len() > MAX_STDERR_TAIL {
                let (capped, _) =
                    crate::output_cap::cap_tail(std::mem::take(&mut buffer), MAX_STDERR_TAIL);
                *buffer = capped;
            }
            true
        };
        for byte in BufReader::new(stream).bytes().map_while(Result::ok) {
            if byte == b'\n' || byte == b'\r' {
                if !flush(&mut line) {
                    return;
                }
            } else {
                line.push(byte);
            }
        }
        flush(&mut line);
    });
}

/// Reserves a free loopback port by binding and immediately releasing it.
///
/// The engine takes a port on its command line, and a fixed one is a trap: an
/// orphaned `sd-server` from a previous app run keeps listening and answers the
/// readiness probe with whatever model *it* holds, so the new job is submitted
/// to the wrong engine and comes back as a bare 400. Handing every launch its
/// own port removes that collision entirely.
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("Failed to reserve a port for the generation engine: {error}"))
}

/// The one running `sd-server`, plus which model it was launched against.
#[derive(Default)]
pub struct GenerationEngineState {
    inner: Mutex<EngineProcess>,
}

#[derive(Default)]
struct EngineProcess {
    child: Option<Child>,
    model_id: Option<String>,
    /// The port this instance was launched on. Chosen per launch, never fixed.
    port: Option<u16>,
    /// The command line it was launched with, port aside. Reuse is keyed on
    /// this, so an edited model gets a fresh engine.
    signature: Option<Vec<String>>,
    /// Tail of the engine's stderr, drained by a reader thread so the pipe can
    /// never fill and block the child.
    stderr_tail: Option<Arc<Mutex<String>>>,
    /// Latest `(step, total)` scraped from that same stream.
    sampling: Option<SamplingProgress>,
}

impl GenerationEngineState {
    pub fn loaded_model(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|state| state.model_id.clone())
    }

    /// Where the running instance is listening, if one is.
    pub fn base_url(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.port)
            .map(|port| format!("http://127.0.0.1:{port}"))
    }

    /// Sampling progress for the job in flight, as `(step, total)`.
    pub fn progress(&self) -> Option<(u32, u32)> {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.sampling.clone())
            .and_then(|cell| cell.lock().ok().and_then(|value| *value))
    }

    /// Drops the previous job's step count so a new job never briefly reports
    /// the last one's progress.
    pub fn clear_progress(&self) {
        if let Ok(state) = self.inner.lock() {
            if let Some(cell) = state.sampling.as_ref() {
                if let Ok(mut value) = cell.lock() {
                    *value = None;
                }
            }
        }
    }

    /// Kills the running server, if any. Called on model switch and on
    /// shutdown — the loaded weights are tens of gigabytes, so the engine is
    /// never kept warm across a switch.
    pub fn stop(&self) -> Result<(), String> {
        let mut state = self.inner.lock().map_err(|error| error.to_string())?;
        if let Some(mut child) = state.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        state.model_id = None;
        state.signature = None;
        state.port = None;
        // Keep the tail: `ensure_ready` stops a failed launch before reporting,
        // and dropping it here would throw away the only useful diagnosis.
        Ok(())
    }

    /// True when the child has already exited; used to fail a readiness wait
    /// fast instead of polling a dead process for five minutes.
    ///
    /// The message carries the engine's own stderr tail. Without it every
    /// launch failure — a weight file quantized for a different loader, a VAE
    /// that does not match the diffusion model, too little memory — reads as
    /// the same bare exit code, and none of them are actionable.
    fn child_exited(&self) -> Result<Option<String>, String> {
        let mut state = self.inner.lock().map_err(|error| error.to_string())?;
        let Some(child) = state.child.as_mut() else {
            return Ok(Some("Generation engine is not running".to_string()));
        };
        let outcome = match child.try_wait() {
            Ok(Some(status)) => format!("Generation engine exited early ({status})"),
            Ok(None) => return Ok(None),
            Err(error) => format!("Generation engine is unreachable: {error}"),
        };
        let detail = state
            .stderr_tail
            .as_ref()
            .and_then(|tail| tail.lock().ok().map(|value| engine_failure_detail(value.trim())))
            .filter(|value| !value.is_empty());
        Ok(Some(match detail {
            Some(detail) => format!("{outcome}:\n{detail}"),
            None => outcome,
        }))
    }

    /// Ensures a healthy `sd-server` is serving `spec`, relaunching if a
    /// different model is loaded. Returns the base URL to submit jobs to.
    pub async fn ensure_ready(
        &self,
        binary: &Path,
        spec: &GenerationModelSpec,
        model_root: &Path,
    ) -> Result<String, String> {
        let signature = launch_signature(spec, model_root);
        let warm = self
            .inner
            .lock()
            .map_err(|error| error.to_string())?
            .signature
            .as_deref()
            == Some(signature.as_slice());
        if warm && self.child_exited()?.is_none() {
            if let Some(base_url) = self.base_url() {
                return Ok(base_url);
            }
        }
        self.stop()?;
        let port = free_port()?;
        let base_url = format!("http://127.0.0.1:{port}");

        for path in spec.component_paths(model_root) {
            if !path.is_file() {
                return Err(format!(
                    "{} is missing a weight file: {}",
                    spec.name,
                    path.display()
                ));
            }
        }

        let mut child = Command::new(binary)
            .args(launch_args(spec, model_root, port))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Failed to spawn the generation engine: {error}"))?;
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let sampling: SamplingProgress = Arc::new(Mutex::new(None));
        // Both streams are drained. The engine splits its output in a way that
        // is not worth predicting — the loader's diagnosis goes to stderr, the
        // progress bar goes to stdout — and either way a piped stream nobody
        // reads fills its buffer and blocks the child.
        if let Some(stream) = child.stdout.take() {
            drain_engine_output(stream, Arc::clone(&stderr_tail), Arc::clone(&sampling));
        }
        if let Some(stream) = child.stderr.take() {
            drain_engine_output(stream, Arc::clone(&stderr_tail), Arc::clone(&sampling));
        }
        {
            let mut state = self.inner.lock().map_err(|error| error.to_string())?;
            state.child = Some(child);
            state.model_id = Some(spec.id.clone());
            state.signature = Some(signature);
            state.port = Some(port);
            state.stderr_tail = Some(stderr_tail);
            state.sampling = Some(sampling);
        }

        let client = reqwest::Client::new();
        let capabilities = format!("{base_url}/sdcpp/v1/capabilities");
        let expected_model_path = identifying_component(spec)
            .map(|component| {
                component
                    .resolved_path(model_root, &spec.id)
                    .to_string_lossy()
                    .to_string()
            })
            .unwrap_or_default();
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(failure) = self.child_exited()? {
                self.stop()?;
                return Err(failure);
            }
            if let Ok(response) = client
                .get(&capabilities)
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                // A foreign service could answer on this port while our child
                // is losing the bind. Prove the child is still alive after the
                // response, exactly as `llama.rs` does — and prove the answer
                // came from the model we asked for, because an engine holding
                // different weights accepts the connection and then rejects
                // every job with a bare 400.
                if response.status().is_success()
                    && response
                        .json::<Value>()
                        .await
                        .ok()
                        .and_then(|body| {
                            body.pointer("/model/path")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .is_some_and(|loaded| loaded == expected_model_path)
                {
                    if let Some(failure) = self.child_exited()? {
                        self.stop()?;
                        return Err(failure);
                    }
                    return Ok(base_url);
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.stop()?;
        Err("Generation engine did not become ready in time".to_string())
    }
}

impl Drop for GenerationEngineState {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The engine's own words for a failure.
///
/// A rejected submission answers `{"error": "loaded model does not support
/// img_gen"}` — a bare string, not the `{"error": {"message": ...}}` object the
/// job endpoint uses. Reading only the object form turned every rejection into
/// "400 Bad Request: no detail" and hid the one sentence that explains it.
fn engine_error_text(body: &Value) -> Option<&str> {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body.get("error").and_then(Value::as_str))
        .or_else(|| body.get("message").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Submits a job and returns its id.
pub async fn submit_job(
    client: &reqwest::Client,
    base_url: &str,
    spec: &GenerationModelSpec,
    request: &GenerationRequest,
) -> Result<String, String> {
    let response = client
        .post(format!("{base_url}{}", request.task.endpoint()))
        .json(&request_body(spec, request))
        .send()
        .await
        .map_err(|error| format!("Submit generation job: {error}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("Generation engine returned an unreadable response: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "Generation engine returned {status}: {}",
            engine_error_text(&body).unwrap_or("no detail")
        ));
    }
    body.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "Generation engine omitted a job id".to_string())
}

pub async fn poll_job(
    client: &reqwest::Client,
    base_url: &str,
    job_id: &str,
) -> Result<JobProgress, String> {
    let response = client
        .get(format!("{base_url}/sdcpp/v1/jobs/{job_id}"))
        .send()
        .await
        .map_err(|error| format!("Poll generation job: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Generation job {job_id} is no longer available ({})",
            response.status()
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("Generation engine returned an unreadable job: {error}"))?;
    decode_job_status(&body)
}

pub async fn cancel_job(client: &reqwest::Client, base_url: &str, job_id: &str) -> bool {
    client
        .post(format!("{base_url}/sdcpp/v1/jobs/{job_id}/cancel"))
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model exactly as a user would have added it: files they chose, slots
    /// they assigned. There is no built-in catalogue to borrow from.
    fn model(id: &str, tasks: Vec<GenerationTask>, grid: FrameGrid) -> GenerationModelSpec {
        GenerationModelSpec {
            id: id.to_string(),
            name: format!("{id} by hand"),
            family: "Wan".to_string(),
            tasks,
            components: vec![ModelComponent::huggingface(
                ComponentSlot::DiffusionModel,
                "someone/some-repo",
                "split_files/diffusion_models/model.safetensors",
                10,
            )],
            defaults: GenerationDefaults {
                width: 704,
                height: 1280,
                steps: 20,
                cfg_scale: 6.0,
                sample_method: "euler".to_string(),
                flow_shift: Some(3.0),
                fps: 24,
                video_frames: 33,
                frame_grid: grid,
            },
            min_ram_bytes: 16 * 1024 * 1024 * 1024,
            license: LicenseGate::default(),
            extra_launch_args: vec!["--diffusion-fa".to_string()],
        }
    }

    fn video_model() -> GenerationModelSpec {
        model(
            "wan-mine",
            vec![GenerationTask::TextToVideo, GenerationTask::ImageToVideo],
            FrameGrid::DownTo4nPlus1,
        )
    }

    fn video_request(task: GenerationTask) -> GenerationRequest {
        GenerationRequest {
            model_id: "wan-mine".to_string(),
            task,
            prompt: "a lovely cat".to_string(),
            negative_prompt: String::new(),
            width: 704,
            height: 1280,
            steps: 20,
            cfg_scale: 6.0,
            sample_method: String::new(),
            scheduler: String::new(),
            clip_skip: -1,
            eta: None,
            strength: None,
            hires: None,
            seed: -1,
            video_frames: 34,
            fps: 24,
            speaker_file: None,
            language: None,
            init_image_base64: None,
            loras: Vec::new(),
            component_overrides: Vec::new(),
        }
    }

    /// The registry is the user's, so validation is the only thing standing
    /// between what they typed and arguments handed to an engine.
    #[test]
    fn spec_validation_rejects_what_would_fail_inside_the_engine() {
        assert!(validate_model_spec(&video_model()).is_ok());

        let mut no_denoiser = video_model();
        no_denoiser.components[0].slot = ComponentSlot::Vae;
        assert!(validate_model_spec(&no_denoiser).is_err());

        let mut two_denoisers = video_model();
        two_denoisers.components.push(ModelComponent::huggingface(
            ComponentSlot::Checkpoint,
            "someone/some-repo",
            "other.safetensors",
            1,
        ));
        assert!(validate_model_spec(&two_denoisers).is_err());

        // Two files in one slot is a mistake the engine cannot report usefully.
        let mut duplicate_slot = video_model();
        duplicate_slot.components.push(ModelComponent::huggingface(
            ComponentSlot::DiffusionModel,
            "someone/some-repo",
            "second.safetensors",
            1,
        ));
        assert!(validate_model_spec(&duplicate_slot).is_err());

        // Components share one flat directory, so a basename collision would
        // silently overwrite.
        let mut collide = video_model();
        collide.components.push(ModelComponent::huggingface(
            ComponentSlot::Vae,
            "another/repo",
            "vae/model.safetensors",
            1,
        ));
        assert!(validate_model_spec(&collide).is_err());

        // The id becomes a directory name and must not climb out of the root.
        for bad in ["../escape", ".hidden", "has space", ""] {
            let mut spec = video_model();
            spec.id = bad.to_string();
            assert!(validate_model_spec(&spec).is_err(), "{bad}");
        }

        let mut relative = video_model();
        relative.components[0].source = ComponentSource::LocalFile {
            path: "models/mine.safetensors".to_string(),
        };
        assert!(validate_model_spec(&relative).is_err());

        let mut no_tasks = video_model();
        no_tasks.tasks.clear();
        assert!(validate_model_spec(&no_tasks).is_err());
    }

    /// A card built from declared sizes reads "0 GB" for a model that is
    /// plainly installed, because a file the user pointed at on their own disk
    /// never had a declared size to begin with. Measure what is there.
    #[test]
    fn a_models_weight_is_measured_on_disk_not_taken_from_the_entry() {
        let root = std::env::temp_dir().join(format!("lm-size-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("wan-mine")).unwrap();
        let elsewhere = root.join("my-own.safetensors");
        std::fs::write(&elsewhere, vec![7u8; 2048]).unwrap();

        let mut spec = video_model();
        spec.components = vec![
            // The user's own file: no declared size, but it is right there.
            ModelComponent {
                slot: ComponentSlot::Checkpoint,
                source: ComponentSource::LocalFile {
                    path: elsewhere.to_string_lossy().to_string(),
                },
                size_bytes: 0,
            },
            // Downloaded and present: the real length wins over the claim.
            ModelComponent::huggingface(ComponentSlot::Vae, "r", "vae.safetensors", 999),
            // Declared but not yet fetched: the claim is all there is.
            ModelComponent::huggingface(ComponentSlot::T5xxl, "r", "t5.safetensors", 64),
        ];
        std::fs::write(root.join("wan-mine/vae.safetensors"), vec![1u8; 512]).unwrap();

        assert_eq!(spec.size_on_disk(&root), 2048 + 512 + 64);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A user's own file is referenced where it lies, never copied into the
    /// app's model directory and never counted as something to download.
    #[test]
    fn local_components_resolve_in_place_and_are_never_fetched() {
        let mut spec = video_model();
        spec.components = vec![ModelComponent {
            slot: ComponentSlot::Checkpoint,
            source: ComponentSource::LocalFile {
                path: "/Users/somebody/models/my-own.safetensors".to_string(),
            },
            size_bytes: 1,
        }];
        assert_eq!(
            launch_args(&spec, Path::new("/app/models"), 8092)
                .windows(2)
                .find(|pair| pair[0] == "--model")
                .map(|pair| pair[1].clone()),
            Some("/Users/somebody/models/my-own.safetensors".to_string())
        );
        // Missing from disk, but still not downloadable — the app has no repo
        // to fetch it from and must not report a download size for it.
        assert!(spec.missing_components(Path::new("/app/models")).is_empty());
        assert!(!spec.components[0].is_downloadable());
    }

    #[test]
    fn launch_args_map_every_slot_to_its_flag() {
        let mut spec = video_model();
        spec.components = vec![
            ModelComponent::huggingface(ComponentSlot::DiffusionModel, "r", "unet.gguf", 1),
            ModelComponent::huggingface(ComponentSlot::Llm, "r", "encoder.gguf", 1),
            ModelComponent::huggingface(ComponentSlot::Vae, "r", "vae/video.safetensors", 1),
            ModelComponent::huggingface(ComponentSlot::AudioVae, "r", "vae/audio.safetensors", 1),
        ];
        let args = launch_args(&spec, Path::new("/models"), 8092);
        for (flag, file) in [
            ("--diffusion-model", "unet.gguf"),
            ("--llm", "encoder.gguf"),
            ("--vae", "video.safetensors"),
            ("--audio-vae", "audio.safetensors"),
        ] {
            let at = args.iter().position(|arg| arg == flag).expect(flag);
            assert_eq!(args[at + 1], format!("/models/wan-mine/{file}"), "{flag}");
        }
        assert!(args.windows(2).any(|pair| pair == ["--listen-port", "8092"]));
        assert!(args.contains(&"--diffusion-fa".to_string()));
    }

    /// A warm engine is reused on what it was launched with, not on the model
    /// id. Adding the VAE a model was missing keeps the same id, and reusing
    /// the engine across that edit would serve the old file set forever — the
    /// user's fix would look like it did nothing.
    #[test]
    fn adding_a_file_to_a_model_makes_the_running_engine_stale() {
        let root = Path::new("/models");
        let mut spec = video_model();
        spec.components = vec![ModelComponent::huggingface(
            ComponentSlot::DiffusionModel,
            "r",
            "unet.gguf",
            1,
        )];
        let before = launch_signature(&spec, root);
        spec.components
            .push(ModelComponent::huggingface(ComponentSlot::Vae, "r", "vae.safetensors", 1));
        assert_ne!(before, launch_signature(&spec, root));
        // The port is the one thing that legitimately differs between two
        // launches of the same file set, so it must not be in the key.
        assert_eq!(
            launch_signature(&spec, root),
            launch_args(&spec, root, 0),
        );
    }

    /// Swapping a VAE is a setting; the file it points at still has to resolve
    /// under the model that owns it, or the engine is handed a path to a file
    /// that was never there.
    #[test]
    fn a_borrowed_part_resolves_under_the_model_it_came_from() {
        let root = Path::new("/models");
        let mut mine = video_model();
        mine.components = vec![
            ModelComponent::huggingface(ComponentSlot::DiffusionModel, "r", "unet.gguf", 1),
            ModelComponent::huggingface(ComponentSlot::Vae, "r", "mine.safetensors", 1),
        ];
        let mut theirs = video_model();
        theirs.id = "other-model".to_string();
        theirs.name = "Other".to_string();
        theirs.components = vec![
            ModelComponent::huggingface(ComponentSlot::DiffusionModel, "r", "unet.gguf", 1),
            ModelComponent::huggingface(ComponentSlot::Vae, "r", "theirs.safetensors", 7),
        ];
        let donors = vec![mine.clone(), theirs.clone()];

        let swap = vec![ComponentOverride {
            slot: ComponentSlot::Vae,
            model_id: "other-model".to_string(),
        }];
        let effective = apply_component_overrides(&mine, &donors, &swap, root).expect("swap");
        let args = launch_args(&effective, root, 8092);
        let at = args.iter().position(|arg| arg == "--vae").expect("--vae");
        // Under the donor's directory, not the borrower's.
        assert_eq!(args[at + 1], "/models/other-model/theirs.safetensors");
        assert_eq!(args.iter().filter(|arg| *arg == "--vae").count(), 1);
        // Everything else is untouched.
        let unet = args.iter().position(|arg| arg == "--diffusion-model").unwrap();
        assert_eq!(args[unet + 1], "/models/wan-mine/unet.gguf");

        // A slot this model does not load is a different model, not a setting.
        let absent = vec![ComponentOverride {
            slot: ComponentSlot::ClipL,
            model_id: "other-model".to_string(),
        }];
        assert!(apply_component_overrides(&mine, &donors, &absent, root).is_err());

        let unknown = vec![ComponentOverride {
            slot: ComponentSlot::Vae,
            model_id: "not-in-the-library".to_string(),
        }];
        assert!(apply_component_overrides(&mine, &donors, &unknown, root).is_err());
    }

    #[test]
    fn a_library_lora_is_named_and_absolute() {
        let good = LoraAsset {
            name: "Detail slider".to_string(),
            path: "/Users/somebody/loras/detail.safetensors".to_string(),
        };
        assert!(validate_lora_asset(&good).is_ok());

        let mut nameless = good.clone();
        nameless.name = "  ".to_string();
        assert!(validate_lora_asset(&nameless).is_err());

        // The engine resolves nothing: a relative path is read against whatever
        // directory it happens to be running in.
        let mut relative = good.clone();
        relative.path = "loras/detail.safetensors".to_string();
        assert!(validate_lora_asset(&relative).is_err());
    }

    /// Wan rounds down onto 4n+1; MiniMax H3 rounds *up* onto 17k+5. Using one
    /// rule for both would misreport the duration of every H3 clip.
    #[test]
    fn frame_counts_snap_onto_each_familys_own_grid() {
        use FrameGrid::{DownTo4nPlus1, UpTo17kPlus5};
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 33), 33);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 34), 33);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 32), 29);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 4), 1);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, u32::MAX), MAX_VIDEO_FRAMES);

        assert_eq!(normalize_video_frames(UpTo17kPlus5, 56), 56);
        assert_eq!(normalize_video_frames(UpTo17kPlus5, 45), 56);
        assert_eq!(normalize_video_frames(UpTo17kPlus5, 39), 39);
        assert_eq!(normalize_video_frames(UpTo17kPlus5, 0), 5);
        let ceiling = normalize_video_frames(UpTo17kPlus5, u32::MAX);
        assert!(ceiling <= MAX_VIDEO_FRAMES);
        assert_eq!((ceiling - 5) % 17, 0);
    }

    #[test]
    fn dimensions_round_up_to_the_sampler_grid() {
        assert_eq!(normalize_dimension(1344), 1344);
        assert_eq!(normalize_dimension(1000), 1024);
        assert_eq!(normalize_dimension(0), 32);
        assert_eq!(normalize_dimension(u32::MAX), MAX_DIMENSION);
    }

    #[test]
    fn validation_normalizes_and_rejects_out_of_bounds_requests() {
        let wan = video_model();
        let normalized =
            validate_request(&wan, &video_request(GenerationTask::TextToVideo)).unwrap();
        assert_eq!(normalized.video_frames, 33);
        assert_eq!(normalized.fps, 24);

        // The same 34 becomes 39 on an upward 17k+5 grid, not 33.
        let h3 = model("h3-mine", vec![GenerationTask::TextToVideo], FrameGrid::UpTo17kPlus5);
        let mut on_h3 = video_request(GenerationTask::TextToVideo);
        on_h3.model_id = h3.id.clone();
        assert_eq!(validate_request(&h3, &on_h3).unwrap().video_frames, 39);

        // Image-driven tasks cannot silently fall back to text-only.
        assert!(validate_request(&wan, &video_request(GenerationTask::ImageToVideo)).is_err());

        // A model that does not declare video must refuse a video task.
        let stills = model("sd-mine", vec![GenerationTask::TextToImage], FrameGrid::DownTo4nPlus1);
        assert!(validate_request(&stills, &video_request(GenerationTask::TextToVideo)).is_err());

        let mut blank = video_request(GenerationTask::TextToVideo);
        blank.prompt = "   ".to_string();
        assert!(validate_request(&wan, &blank).is_err());

        let mut too_many_steps = video_request(GenerationTask::TextToVideo);
        too_many_steps.steps = MAX_STEPS + 1;
        assert!(validate_request(&wan, &too_many_steps).is_err());

        let mut bad_cfg = video_request(GenerationTask::TextToVideo);
        bad_cfg.cfg_scale = f64::NAN;
        assert!(validate_request(&wan, &bad_cfg).is_err());
    }

    #[test]
    fn request_bodies_carry_video_fields_only_for_video_tasks() {
        let wan = video_model();
        let body = request_body(&wan, &video_request(GenerationTask::TextToVideo));
        assert_eq!(body["video_frames"], json!(34));
        assert_eq!(body["output_format"], json!("webm"));
        assert_eq!(body["sample_params"]["flow_shift"], json!(3.0));
        assert_eq!(body["sample_params"]["guidance"]["txt_cfg"], json!(6.0));
        assert!(body.get("init_image").is_none());

        let mut stills = model("sd-mine", vec![GenerationTask::TextToImage], FrameGrid::DownTo4nPlus1);
        stills.defaults.flow_shift = None;
        let mut image = video_request(GenerationTask::TextToImage);
        image.model_id = stills.id.clone();
        let body = request_body(&stills, &image);
        assert_eq!(body["output_format"], json!("png"));
        assert!(body.get("video_frames").is_none());
        // A model that declares no flow shift must leave the field absent
        // rather than pinning a value the backend would otherwise choose.
        assert!(body["sample_params"].get("flow_shift").is_none());
    }

    /// The engine ignores prompt-embedded `<lora:...>` tags on purpose, so the
    /// structured array is the only way a LoRA reaches it — and any model can
    /// take any number of them.
    #[test]
    fn lora_stacks_reach_the_request_body_and_are_bounded() {
        let wan = video_model();
        let mut request = video_request(GenerationTask::TextToVideo);
        request.loras = vec![
            LoraSelection {
                path: "/loras/style.safetensors".to_string(),
                multiplier: 0.8,
                is_high_noise: false,
            },
            LoraSelection {
                path: "/loras/motion.safetensors".to_string(),
                multiplier: -0.4,
                is_high_noise: true,
            },
        ];
        let normalized = validate_request(&wan, &request).unwrap();
        let body = request_body(&wan, &normalized);
        let loras = body["lora"].as_array().expect("lora array");
        assert_eq!(loras.len(), 2);
        assert_eq!(loras[0]["path"], json!("/loras/style.safetensors"));
        // Negative strengths are meaningful — they subtract a style.
        assert_eq!(loras[1]["multiplier"], json!(-0.4));
        assert_eq!(loras[1]["is_high_noise"], json!(true));

        // No LoRAs means no key at all, not an empty array.
        let plain = request_body(&wan, &video_request(GenerationTask::TextToVideo));
        assert!(plain.get("lora").is_none());

        let mut relative = request.clone();
        relative.loras[0].path = "style.safetensors".to_string();
        assert!(validate_request(&wan, &relative).is_err());

        let mut wild = request.clone();
        wild.loras[0].multiplier = f64::INFINITY;
        assert!(validate_request(&wan, &wild).is_err());

        let mut too_many = request.clone();
        too_many.loras = std::iter::repeat_n(request.loras[0].clone(), MAX_LORAS + 1).collect();
        assert!(validate_request(&wan, &too_many).is_err());
    }

    /// A user-added model is stored as JSON and read back on the next launch,
    /// so the whole spec has to survive a round trip.
    #[test]
    fn a_user_added_model_survives_a_round_trip_through_disk() {
        let mut spec = video_model();
        spec.components.push(ModelComponent {
            slot: ComponentSlot::Vae,
            source: ComponentSource::LocalFile {
                path: "/Users/somebody/vae.safetensors".to_string(),
            },
            size_bytes: 5,
        });
        let encoded = serde_json::to_vec(&spec).unwrap();
        let decoded: GenerationModelSpec = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.id, spec.id);
        assert_eq!(decoded.defaults.sample_method, "euler");
        assert_eq!(decoded.defaults.frame_grid, FrameGrid::DownTo4nPlus1);
        assert_eq!(decoded.components.len(), 2);
        assert!(matches!(
            decoded.components[1].source,
            ComponentSource::LocalFile { .. }
        ));
        assert_eq!(
            launch_args(&decoded, Path::new("/m"), 1),
            launch_args(&spec, Path::new("/m"), 1)
        );
    }

    #[test]
    fn job_status_decoding_covers_every_terminal_state() {
        assert_eq!(
            decode_job_status(&json!({"status": "queued", "queue_position": 2})).unwrap(),
            JobProgress::Running { queue_position: 2 }
        );
        assert_eq!(
            decode_job_status(&json!({"status": "cancelled"})).unwrap(),
            JobProgress::Cancelled
        );
        assert_eq!(
            decode_job_status(&json!({"status": "failed", "error": {"message": "out of memory"}}))
                .unwrap(),
            JobProgress::Failed("out of memory".to_string())
        );

        // vid_gen: one container inline, carrying its own media type.
        let completed = decode_job_status(&json!({
            "status": "completed",
            "result": {
                "mime_type": "video/webm",
                "output_format": "webm",
                "fps": 24,
                "frame_count": 33,
                "b64_json": STANDARD.encode(b"webm-bytes"),
            }
        }))
        .unwrap();
        let JobProgress::Completed(media) = completed else {
            panic!("expected a completed job");
        };
        assert_eq!(media.bytes, b"webm-bytes");
        assert_eq!(media.media_type, "video/webm");
        assert_eq!(media.frame_count, 33);

        // img_gen: a list of encoded images and no media type at all — the
        // format is stated once for the batch. Verified against a real
        // sd-server response, which is how this shape was found.
        let completed = decode_job_status(&json!({
            "status": "completed",
            "result": {
                "output_format": "png",
                "images": [{"index": 0, "b64_json": STANDARD.encode(b"png-bytes")}],
            }
        }))
        .unwrap();
        let JobProgress::Completed(media) = completed else {
            panic!("expected a completed job");
        };
        assert_eq!(media.bytes, b"png-bytes");
        assert_eq!(media.media_type, "image/png");
        assert_eq!(media.frame_count, 1);

        // A completed job with no payload is a protocol error, not empty media.
        assert!(decode_job_status(&json!({"status": "completed", "result": {}})).is_err());
        assert!(decode_job_status(&json!({
            "status": "completed",
            "result": {"output_format": "png", "images": []}
        }))
        .is_err());
        assert!(decode_job_status(&json!({"status": "invented"})).is_err());
    }

    /// The canvas and sampling controls belong to the run, not the library
    /// entry, so a per-request sampler has to win over the model's default —
    /// while an empty one still falls back rather than sending "".
    #[test]
    fn per_request_sampling_overrides_the_models_default() {
        let wan = video_model();
        let mut request = video_request(GenerationTask::TextToVideo);
        request.sample_method = "  dpm++2m  ".to_string();
        let body = request_body(&wan, &validate_request(&wan, &request).unwrap());
        assert_eq!(body["sample_params"]["sample_method"], json!("dpm++2m"));

        let fallback = request_body(&wan, &video_request(GenerationTask::TextToVideo));
        assert_eq!(fallback["sample_params"]["sample_method"], json!("euler"));

        let mut absurd = request.clone();
        absurd.sample_method = "x".repeat(65);
        assert!(validate_request(&wan, &absurd).is_err());
    }

    /// Every control on the Image and Video tabs has to survive the trip to
    /// the engine, and every one of them has a bound — a canvas that grows
    /// past the ceiling on the second pass fails several minutes in, long
    /// after the first pass has already been paid for.
    #[test]
    fn the_full_control_set_reaches_the_body_and_is_bounded() {
        let wan = video_model();
        let mut request = video_request(GenerationTask::ImageToVideo);
        request.init_image_base64 = Some("aGk=".to_string());
        request.scheduler = " karras ".to_string();
        request.clip_skip = 2;
        request.eta = Some(0.3);
        request.strength = Some(0.55);
        request.hires = Some(HiresSettings {
            scale: 1.5,
            steps: 8,
            denoising_strength: 0.5,
            upscaler: "Lanczos".to_string(),
        });
        let body = request_body(&wan, &validate_request(&wan, &request).unwrap());
        assert_eq!(body["sample_params"]["scheduler"], json!("karras"));
        assert_eq!(body["sample_params"]["eta"], json!(0.3));
        assert_eq!(body["clip_skip"], json!(2));
        assert_eq!(body["strength"], json!(0.55));
        assert_eq!(body["hires"]["enabled"], json!(true));
        assert_eq!(body["hires"]["upscaler"], json!("Lanczos"));

        // Nothing optional is guessed: an untouched request leaves the
        // backend's own choices alone.
        let plain = request_body(&wan, &video_request(GenerationTask::TextToVideo));
        assert!(plain["sample_params"].get("scheduler").is_none());
        assert!(plain["sample_params"].get("eta").is_none());
        assert!(plain.get("hires").is_none());
        // Denoising strength is meaningless without something to denoise from.
        let mut text_only = request.clone();
        text_only.task = GenerationTask::TextToVideo;
        text_only.init_image_base64 = None;
        assert!(request_body(&wan, &validate_request(&wan, &text_only).unwrap())
            .get("strength")
            .is_none());

        for spoil in [
            (|r: &mut GenerationRequest| r.clip_skip = MAX_CLIP_SKIP + 1) as fn(&mut _),
            |r| r.clip_skip = -2,
            |r| r.eta = Some(1.5),
            |r| r.strength = Some(-0.1),
            |r| r.hires.as_mut().unwrap().scale = MAX_HIRES_SCALE + 1.0,
            |r| r.hires.as_mut().unwrap().denoising_strength = f64::NAN,
            |r| r.hires.as_mut().unwrap().upscaler = String::new(),
            // 1280 × 4 overshoots the 4096 px ceiling on the second pass.
            |r| r.hires.as_mut().unwrap().scale = 4.0,
        ] {
            let mut bad = request.clone();
            spoil(&mut bad);
            assert!(validate_request(&wan, &bad).is_err());
        }
    }

    /// The job API reports no step count, so the engine's redrawn progress bar
    /// is the only source of a percentage. Tensor loading draws the identical
    /// bar with a byte rate, which must not be mistaken for sampling.
    #[test]
    fn sampling_progress_is_read_only_from_the_sampling_bar() {
        assert_eq!(
            parse_sampling_progress("  |========>              | 4/25 - 2.42s/it\u{1b}[K"),
            Some((4, 25))
        );
        assert_eq!(
            parse_sampling_progress("  |======| 30/30 - 1.9it/s"),
            Some((30, 30))
        );
        // Model loading, not sampling.
        assert_eq!(
            parse_sampling_progress("  |####      | 212/686 - 647.34MB/s"),
            None
        );
        assert_eq!(parse_sampling_progress("[INFO ] main.cpp:148 - listening"), None);
        assert_eq!(parse_sampling_progress("  |==| 4/0 - 1.0s/it"), None);
    }

    /// A launch failure is forty lines of Metal probing and two that matter.
    /// Quoting the whole tail is what made a one-field mistake unreadable.
    #[test]
    fn a_failed_launch_reports_the_diagnosis_not_the_device_probe() {
        // Trimmed from a real failure: an all-in-one checkpoint assigned to
        // --diffusion-model.
        let tail = "\
ggml_metal_device_init: GPU name: MTL0 (Apple M4 Pro)
ggml_metal_device_init: has unified memory = true
ggml_metal_device_init: recommendedMaxWorkingSetSize = 40200.90 MB
[INFO ] stable-diffusion.cpp:717 - loading diffusion model from '/Users/x/sd-turbo.safetensors'
[INFO ] model_loader.cpp:242 - load /Users/x/sd-turbo.safetensors using safetensors format
[ERROR] stable-diffusion.cpp:902 - get sd version from file failed: ''
[ERROR] main.cpp:92 - new_sd_ctx_t failed";
        let detail = engine_failure_detail(tail);
        assert!(!detail.contains("ggml_metal_device_init"), "{detail}");
        assert!(detail.contains("get sd version from file failed"));
        assert!(detail.contains("belongs on --model"), "{detail}");

        // A quantization for another loader is the other failure that reads as
        // a broken download rather than a wrong file.
        assert!(engine_failure_detail(
            "[ERROR] ggml: tensor 'x' has wrong shape in model metadata: got [64, 2688]"
        )
        .contains("stable-diffusion.cpp"));

        // Nothing self-identifies as an error: fall back to where it stopped,
        // rather than reporting no detail at all.
        let quiet = engine_failure_detail("line one\nline two\nline three");
        assert_eq!(quiet, "line one\nline two\nline three");
    }

    /// A rejected submission answers with a bare string, and reading only the
    /// object form is what turned every 400 into "no detail".
    #[test]
    fn engine_errors_are_read_in_both_shapes_the_engine_uses() {
        assert_eq!(
            engine_error_text(&json!({"error": "loaded model does not support img_gen"})),
            Some("loaded model does not support img_gen")
        );
        assert_eq!(
            engine_error_text(&json!({"error": {"message": "out of memory"}})),
            Some("out of memory")
        );
        assert_eq!(engine_error_text(&json!({"error": null})), None);
        assert_eq!(engine_error_text(&json!({"error": "  "})), None);
    }

    /// Speech is a different engine with a different command line: no canvas,
    /// no sampler, a vocoder the diffusion server would reject as an unknown
    /// flag, and one wav written straight to disk.
    #[test]
    fn speech_builds_its_own_command_line_and_skips_the_diffusion_bounds() {
        let mut spec = model("voice", vec![GenerationTask::TextToSpeech], FrameGrid::default());
        spec.components = vec![
            ModelComponent::huggingface(ComponentSlot::Checkpoint, "r", "backbone.gguf", 1),
            ModelComponent::huggingface(ComponentSlot::Mmproj, "r", "mmproj.gguf", 1),
        ];
        spec.extra_launch_args = vec!["--tts-use-guide-tokens".to_string()];
        assert!(validate_model_spec(&spec).is_ok());

        let mut request = video_request(GenerationTask::TextToSpeech);
        request.model_id = spec.id.clone();
        request.speaker_file = Some("/Users/somebody/reference.wav".to_string());
        request.language = Some("EN".to_string());
        let normalized = validate_request(&spec, &request).unwrap();
        assert_eq!(normalized.width, 0);

        let args = speech_args(&spec, Path::new("/m"), &normalized, Path::new("/out.wav")).unwrap();
        for (flag, value) in [
            ("--model", "/m/voice/backbone.gguf"),
            ("--mmproj", "/m/voice/mmproj.gguf"),
            ("--prompt", "a lovely cat"),
            ("--output", "/out.wav"),
            ("--tts-speaker-file", "/Users/somebody/reference.wav"),
            ("--tts-lang", "en"),
        ] {
            let at = args.iter().position(|arg| arg == flag).expect(flag);
            assert_eq!(args[at + 1], value, "{flag}");
        }
        assert!(args.contains(&"--tts-use-guide-tokens".to_string()));

        // The projector is llama-tts's flag; sd-server would reject it outright.
        assert!(!launch_args(&spec, Path::new("/m"), 1).contains(&"--mmproj".to_string()));

        // A relative reference clip must not reach the command line.
        let mut relative = request.clone();
        relative.speaker_file = Some("reference.wav".to_string());
        assert!(validate_request(&spec, &relative).is_err());

        let mut wrong_language = request.clone();
        wrong_language.language = Some("english".to_string());
        assert!(validate_request(&spec, &wrong_language).is_err());
    }

    /// Speaks one utterance with the real pinned `llama-tts`, twice: once in
    /// the model's own voice and once cloned from the first take. Ignored by
    /// default because it needs weights on disk, but it is what proves the
    /// second pin was worth taking — the chat pin's `llama-tts` rejects these
    /// weights outright with `unknown model architecture: 'qwen3tts'`.
    ///
    /// ```text
    /// TTS_BINARY=…/llama-tts TTS_MODEL=…/backbone.gguf TTS_MMPROJ=…/mmproj.gguf \
    ///   cargo test --lib generation -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs the staged llama-tts and a speech model"]
    fn the_pinned_speech_engine_speaks_and_clones_a_voice() {
        let (Ok(binary), Ok(model), Ok(mmproj)) = (
            std::env::var("TTS_BINARY"),
            std::env::var("TTS_MODEL"),
            std::env::var("TTS_MMPROJ"),
        ) else {
            panic!("set TTS_BINARY, TTS_MODEL and TTS_MMPROJ");
        };
        let mut spec = model_spec_for_speech(&model, &mmproj);
        spec.extra_launch_args.clear();

        let directory = std::env::temp_dir().join(format!("lm-tts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let speak = |output: &Path, reference: Option<&Path>| {
            let mut request = video_request(GenerationTask::TextToSpeech);
            request.model_id = spec.id.clone();
            request.prompt = "Little Monkey now speaks.".to_string();
            request.speaker_file =
                reference.map(|path| path.to_string_lossy().to_string());
            let request = validate_request(&spec, &request).unwrap();
            let args = speech_args(&spec, Path::new("/unused"), &request, output).unwrap();
            let status = Command::new(&binary).args(&args).output().unwrap();
            assert!(
                status.status.success(),
                "{}",
                String::from_utf8_lossy(&status.stderr)
            );
            let bytes = std::fs::read(output).unwrap();
            // RIFF/WAVE, and long enough to be speech rather than a header.
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WAVE");
            assert!(bytes.len() > 8_000, "{} bytes", bytes.len());
            bytes
        };

        let first = directory.join("spoken.wav");
        speak(&first, None);
        // The clone reads the take above as its reference, which is the whole
        // of voice cloning here: a plain clip in, that voice out.
        speak(&directory.join("cloned.wav"), Some(&first));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    fn model_spec_for_speech(backbone: &str, mmproj: &str) -> GenerationModelSpec {
        let mut spec = model("voice", vec![GenerationTask::TextToSpeech], FrameGrid::default());
        spec.components = vec![
            ModelComponent {
                slot: ComponentSlot::Checkpoint,
                source: ComponentSource::LocalFile { path: backbone.to_string() },
                size_bytes: 0,
            },
            ModelComponent {
                slot: ComponentSlot::Mmproj,
                source: ComponentSource::LocalFile { path: mmproj.to_string() },
                size_bytes: 0,
            },
        ];
        spec
    }

    /// Drives a real `sd-server` end to end. Ignored by default because it
    /// needs a checkpoint on disk and minutes of sampling, but it is the only
    /// thing that proves the parts unit tests cannot reach: that the engine
    /// comes up on its own port even while an orphan holds the old fixed one,
    /// that its capabilities identify the model we asked for, and that a step
    /// count actually appears on stderr mid-job.
    ///
    /// ```text
    /// SD_SERVER=…/sd-server SD_CHECKPOINT=…/sd-turbo.safetensors \
    ///   cargo test --lib generation -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a local sd-server binary and checkpoint"]
    fn a_real_engine_reports_its_model_and_its_step_count() {
        let (Ok(binary), Ok(checkpoint)) = (
            std::env::var("SD_SERVER"),
            std::env::var("SD_CHECKPOINT"),
        ) else {
            panic!("set SD_SERVER and SD_CHECKPOINT");
        };
        let mut spec = model("live", vec![GenerationTask::TextToImage], FrameGrid::default());
        spec.components = vec![ModelComponent {
            slot: ComponentSlot::Checkpoint,
            source: ComponentSource::LocalFile { path: checkpoint },
            size_bytes: 0,
        }];
        spec.defaults.flow_shift = None;
        spec.extra_launch_args.clear();

        let mut request = video_request(GenerationTask::TextToImage);
        request.model_id = spec.id.clone();
        request.width = 256;
        request.height = 256;
        request.steps = 25;
        request.scheduler = "karras".to_string();
        request.clip_skip = 2;
        request.hires = Some(HiresSettings {
            scale: 2.0,
            steps: 4,
            denoising_strength: 0.5,
            upscaler: "Lanczos".to_string(),
        });
        let request = validate_request(&spec, &request).unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let engine = GenerationEngineState::default();
            let base_url = engine
                .ensure_ready(Path::new(&binary), &spec, Path::new("/unused"))
                .await
                .expect("engine ready");
            let client = reqwest::Client::new();
            let job = submit_job(&client, &base_url, &spec, &request)
                .await
                .expect("submit");

            let mut saw_progress = None;
            let media = loop {
                match poll_job(&client, &base_url, &job).await.unwrap() {
                    JobProgress::Running { .. } => {
                        saw_progress = saw_progress.or_else(|| engine.progress());
                    }
                    JobProgress::Completed(media) => break media,
                    other => panic!("{other:?}"),
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            };
            let (step, total) = saw_progress.expect("a step count reached the app mid-job");
            assert_eq!(total, 25, "total steps came from the request");
            assert!(step >= 1 && step <= total);
            assert_eq!(media.media_type, "image/png");
            assert!(media.bytes.starts_with(b"\x89PNG"));
            // IHDR carries the real canvas: a 2× hires pass on 256 px has to
            // come back at 512, or the second pass never ran.
            let dimension = |at: usize| u32::from_be_bytes(media.bytes[at..at + 4].try_into().unwrap());
            assert_eq!((dimension(16), dimension(20)), (512, 512));
            engine.stop().unwrap();
        });
    }

    #[test]
    fn video_and_image_tasks_reach_different_endpoints() {
        assert_eq!(GenerationTask::TextToVideo.endpoint(), "/sdcpp/v1/vid_gen");
        assert_eq!(GenerationTask::ImageToVideo.endpoint(), "/sdcpp/v1/vid_gen");
        assert_eq!(GenerationTask::TextToImage.endpoint(), "/sdcpp/v1/img_gen");
        assert!(GenerationTask::ImageToVideo.needs_init_image());
        assert!(!GenerationTask::TextToVideo.needs_init_image());
        // Speech runs on a different engine and must be routed before anything
        // sd-server-shaped is reached for.
        assert!(GenerationTask::TextToSpeech.is_speech());
        assert!(!GenerationTask::TextToVideo.is_speech());
    }
}
