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
use std::collections::{BTreeMap, BTreeSet};
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
/// Reference images per generation. Each one is decoded and held in memory
/// beside the others, so the bound is on the whole set rather than only on the
/// size of any single image.
const MAX_REF_IMAGES: usize = 8;
/// A 15 s 2K clip with audio stays far under this; it exists so a runaway
/// server response can never be buffered without bound.
const MAX_MEDIA_BYTES: usize = 256 * 1024 * 1024;
/// Images one request may ask for. The engine samples a batch serially, so a
/// large count is a long run rather than a parallel one; this keeps a
/// mistyped number from turning into an hour of sampling.
const MAX_BATCH_COUNT: u32 = 8;
/// Layers `llama-tts` is asked to put on the GPU. Speech backbones are around
/// a billion parameters, so "all of them" fits everywhere the flag does
/// anything, and llama.cpp clamps the number to the layers a model actually
/// has rather than erroring on an overshoot.
const SPEECH_GPU_LAYERS: u32 = 999;
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
    /// The vision tower beside an [`Self::Llm`] text encoder, for the
    /// architectures whose LLM reads an image as well as the prompt.
    LlmVision,
    /// A second, unconditional diffusion model some distilled architectures
    /// pair with the main one.
    UncondDiffusionModel,
    /// LTXAV's embeddings connectors.
    EmbeddingsConnectors,
    /// AnimateDiff's motion module, which is what turns an SD 1.5 checkpoint
    /// into a video model.
    MotionModule,
    Vae,
    /// Models that generate synchronized audio decode it through its own VAE.
    AudioVae,
    Taesd,
    /// Structural conditioning: the run supplies a pre-processed control image
    /// (a depth map, a pose skeleton, an edge map) and the sampler is held to
    /// it. Loading this is what makes `control_image` mean anything.
    ControlNet,
    /// Reference-image conditioning. `--ip-adapter` requires
    /// [`Self::ClipVision`] alongside it — see [`validate_model_spec`].
    IpAdapter,
    /// Subject identity from reference photographs (PhotoMaker).
    PhotoMaker,
    /// Subject identity, the FLUX-family alternative to
    /// [`Self::PhotoMaker`].
    PulidWeights,
    /// The YOLOv8 detector ADetailer re-renders around: faces, hands, whatever
    /// the model was trained to find. Loaded at launch rather than per run
    /// because `ad_model_path` is the one ADetailer field the server does not
    /// read from the request body — the prompts beside it are per run.
    AdModel,
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
            Self::LlmVision => "--llm_vision",
            Self::UncondDiffusionModel => "--uncond-diffusion-model",
            Self::EmbeddingsConnectors => "--embeddings-connectors",
            Self::MotionModule => "--motion-module",
            Self::Vae => "--vae",
            Self::AudioVae => "--audio-vae",
            Self::Taesd => "--taesd",
            Self::ControlNet => "--control-net",
            Self::IpAdapter => "--ip-adapter",
            Self::PhotoMaker => "--photo-maker",
            Self::PulidWeights => "--pulid-weights",
            Self::AdModel => "--ad-model",
            Self::Mmproj => "--mmproj",
            Self::Vocoder => "--model-vocoder",
        }
    }

    /// Whether filling this slot enables a per-run conditioning image rather
    /// than contributing to the model itself. The generation form asks this to
    /// decide which conditioning inputs to offer, and [`validate_request`] asks
    /// it to refuse an image the loaded engine would silently discard.
    pub fn conditioning_image(self) -> Option<ConditioningImage> {
        match self {
            Self::ControlNet => Some(ConditioningImage::Control),
            Self::IpAdapter => Some(ConditioningImage::IpAdapter),
            Self::PhotoMaker | Self::PulidWeights => Some(ConditioningImage::Reference),
            _ => None,
        }
    }

    /// Whether this slot belongs to `llama-tts` rather than `sd-server`. The
    /// two engines share no flags, so each builder consults this rather than
    /// listing the other's slots and drifting out of step.
    pub fn is_speech_only(self) -> bool {
        matches!(self, Self::Mmproj | Self::Vocoder)
    }
}

/// Which per-run image a loaded conditioning slot unlocks. Named rather than
/// boolean because the three are not interchangeable: the engine reads them
/// from three different request fields with three different meanings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditioningImage {
    /// `control_image`: structure to follow.
    Control,
    /// `ip_adapter_image`: style/content to borrow.
    IpAdapter,
    /// `ref_images`: subjects to keep consistent.
    Reference,
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
            ComponentSource::LocalFile { path } => {
                path.rsplit(['/', '\\']).next().unwrap_or(path.as_str())
            }
        }
    }

    /// Where this component actually lives once available.
    pub fn resolved_path(&self, model_root: &Path, model_id: &str) -> PathBuf {
        match &self.source {
            ComponentSource::HuggingFace { .. } => model_root.join(model_id).join(self.file_name()),
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
        return Err("Exactly one file must be the checkpoint or the diffusion model".to_string());
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
            return Err(format!(
                "Two files are both named {}",
                component.file_name()
            ));
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
    if spec.defaults.sample_method.trim().is_empty() || spec.defaults.sample_method.len() > 64 {
        return Err("A model needs a sampling method".to_string());
    }
    // The engine's own constraint: an IP-Adapter reads its reference image
    // through a CLIP vision tower, and `--ip-adapter` without `--clip_vision`
    // fails inside `sd-server` at load. Caught here so the model is rejected
    // while it is being added rather than the first time it is run.
    let slots: BTreeSet<ComponentSlot> = spec
        .components
        .iter()
        .map(|component| component.slot)
        .collect();
    if slots.contains(&ComponentSlot::IpAdapter) && !slots.contains(&ComponentSlot::ClipVision) {
        return Err("An IP-Adapter also needs a CLIP vision encoder".to_string());
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
    // `llama-tts` is llama.cpp, and llama.cpp offloads nothing unless asked.
    // The macOS arm64 archive this app pins ships `libggml-metal.dylib`, so
    // without this speech synthesizes on the CPU of a machine holding a GPU
    // that already has the weights' worth of unified memory. A build with no
    // GPU backend parses the flag and ignores it, so this is safe on every
    // target rather than gated to one. `extra_launch_args` is appended after,
    // and llama.cpp takes the last occurrence of a flag, so a model that wants
    // a different split — or none — still sets one.
    let mut args = vec!["-ngl".to_string(), SPEECH_GPU_LAYERS.to_string()];
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
    /// How many images to sample from this one prompt. Image tasks only:
    /// video and speech produce one artifact per run by construction. Each
    /// image after the first uses the next seed, so a pinned seed still
    /// yields a varied batch rather than the same picture N times.
    #[serde(default = "default_batch_count")]
    pub batch_count: u32,
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
    /// Base64 single-channel mask over [`Self::init_image_base64`]: white is
    /// repainted, black is kept. This is inpainting — it is meaningless without
    /// an init image to mask, and [`validate_request`] enforces that rather
    /// than letting the engine silently ignore it.
    #[serde(default)]
    pub mask_image_base64: Option<String>,
    /// What ADetailer paints into each region its detector found. Empty
    /// inherits the main prompt, which is the engine's own default, so this is
    /// sent only when the user wrote something different.
    #[serde(default)]
    pub ad_prompt: Option<String>,
    /// The negative prompt for those same regions. Inherits the main one when
    /// absent, exactly as `ad_prompt` does.
    #[serde(default)]
    pub ad_negative_prompt: Option<String>,
    /// Base64 *pre-processed* control image — a depth map, a pose skeleton, a
    /// Canny edge map. Not a photograph: the engine applies no detector, so
    /// whatever is sent is taken as the structure to follow verbatim.
    #[serde(default)]
    pub control_image_base64: Option<String>,
    /// How strongly the control image binds. `None` leaves the engine default.
    #[serde(default)]
    pub control_strength: Option<f64>,
    /// Base64 reference image whose style/content is borrowed.
    #[serde(default)]
    pub ip_adapter_image_base64: Option<String>,
    /// How strongly the IP-Adapter image applies. `None` leaves the default.
    #[serde(default)]
    pub ip_adapter_strength: Option<f64>,
    /// Base64 reference images for the identity- and edit-conditioned
    /// architectures (PhotoMaker, PuLID, Kontext, Qwen-Edit).
    #[serde(default)]
    pub ref_images_base64: Vec<String>,
    /// Whether each reference image gets its own index rather than sharing
    /// one. Architecture-specific; the engine's own default is `false`.
    #[serde(default)]
    pub increase_ref_index: bool,
    /// LoRAs to apply, in order. Any model can take any number.
    #[serde(default)]
    pub loras: Vec<LoraSelection>,
    /// Per-run swaps of which library file fills one of this model's slots.
    #[serde(default)]
    pub component_overrides: Vec<ComponentOverride>,
}

/// A loose weight file in the user's library: a CLIP, a text encoder, a VAE.
///
/// A model entry has to be a whole model — exactly one checkpoint or diffusion
/// model, see [`validate_model_spec`] — so the encoders and VAEs that are used
/// *across* models cannot live there. This is where they live: added once, and
/// picked per generation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartAsset {
    pub slot: ComponentSlot,
    pub name: String,
    pub path: String,
}

pub fn validate_part_asset(asset: &PartAsset) -> Result<(), String> {
    if asset.name.trim().is_empty() || asset.name.len() > 200 {
        return Err("A part needs a name".to_string());
    }
    if !Path::new(&asset.path).is_absolute() {
        return Err("A part needs an absolute path".to_string());
    }
    Ok(())
}

/// One per-run choice: fill `slot` with this file from the parts library.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentOverride {
    pub slot: ComponentSlot,
    pub path: String,
}

/// Rewrites `spec` to load the parts chosen for this run.
///
/// A chosen part replaces the model's own file for that slot, or is added when
/// the model has no file for it — which is the common case, since a checkpoint
/// that needs a separate VAE does not name one.
///
/// Every path is checked against the library rather than trusted. The caller is
/// the UI, and accepting an arbitrary absolute path would turn every generation
/// request into a way to hand the engine any file on the machine; checking it
/// keeps the loadable set exactly the set the user added.
pub fn apply_component_overrides(
    spec: &GenerationModelSpec,
    parts: &[PartAsset],
    overrides: &[ComponentOverride],
) -> Result<GenerationModelSpec, String> {
    let mut effective = spec.clone();
    for choice in overrides {
        let part = parts
            .iter()
            .find(|entry| entry.slot == choice.slot && entry.path == choice.path)
            .ok_or_else(|| {
                format!(
                    "{} is not a {} in your library",
                    choice.path,
                    choice.slot.flag()
                )
            })?;
        // A denoiser is what the model *is*; replacing it from a per-run
        // dropdown would silently make this a different model.
        if matches!(
            part.slot,
            ComponentSlot::Checkpoint | ComponentSlot::DiffusionModel
        ) {
            return Err("The checkpoint is chosen with the model, not per run".to_string());
        }
        let source = ComponentSource::LocalFile {
            path: part.path.clone(),
        };
        match effective
            .components
            .iter_mut()
            .find(|component| component.slot == part.slot)
        {
            Some(existing) => {
                existing.source = source;
                existing.size_bytes = 0;
            }
            None => effective.components.push(ModelComponent {
                slot: part.slot,
                source,
                size_bytes: 0,
            }),
        }
    }
    Ok(effective)
}

/// Refuses a conditioning image the loaded engine has no weights to read it
/// with.
///
/// Separate from [`validate_request`] because it is the only check that needs
/// the *effective* spec: a ControlNet can arrive from the model entry or from a
/// per-run component override, and overrides are resolved after the request is
/// validated. Called by the run command once both are known.
///
/// This matters more than a normal input check. `sd-server` accepts
/// `control_image` whether or not `--control-net` was passed and simply ignores
/// it when it was not, so the failure is a perfectly ordinary-looking image
/// that took three minutes to sample and followed none of the structure it was
/// given — with nothing anywhere saying why.
pub fn validate_conditioning(
    spec: &GenerationModelSpec,
    request: &GenerationRequest,
) -> Result<(), String> {
    let available: BTreeSet<ConditioningImage> = spec
        .components
        .iter()
        .filter_map(|component| component.slot.conditioning_image())
        .collect();
    let required = [
        (
            request.control_image_base64.is_some(),
            ConditioningImage::Control,
            "a control image",
            "ControlNet",
        ),
        (
            request.ip_adapter_image_base64.is_some(),
            ConditioningImage::IpAdapter,
            "a reference image",
            "IP-Adapter",
        ),
        (
            !request.ref_images_base64.is_empty(),
            ConditioningImage::Reference,
            "reference images",
            "PhotoMaker or PuLID",
        ),
    ];
    for (sent, kind, what, weights) in required {
        if sent && !available.contains(&kind) {
            return Err(format!(
                "{} has no {weights} weights, so {what} would be ignored. Add one to the model or pick one for this run.",
                spec.name
            ));
        }
    }
    // ADetailer fails the same silent way, but is not a conditioning *image* so
    // it cannot ride the table above: its prompts are request fields while the
    // detector they need is a launch flag, and an engine started without
    // `--ad-model` has nothing to detect with and drops the prompts.
    if (request.ad_prompt.is_some() || request.ad_negative_prompt.is_some())
        && !spec
            .components
            .iter()
            .any(|component| component.slot == ComponentSlot::AdModel)
    {
        return Err(format!(
            "{} has no ADetailer detector, so its prompt would be ignored. Add one to the model or pick one for this run.",
            spec.name
        ));
    }
    Ok(())
}

/// `-1` means "whatever the model was trained with", which is the only sane
/// default for a setting most models do not want changed.
fn default_clip_skip() -> i32 {
    -1
}

/// One image, matching every request written before batching existed.
fn default_batch_count() -> u32 {
    1
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
        normalized.batch_count = 1;
        return Ok(normalized);
    }
    if !(1..=MAX_BATCH_COUNT).contains(&request.batch_count) {
        return Err(format!(
            "Batch size must be between 1 and {MAX_BATCH_COUNT}"
        ));
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
        if !hires.denoising_strength.is_finite() || !(0.0..=1.0).contains(&hires.denoising_strength)
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

    // A mask names which pixels of the source image to repaint, so without one
    // there is nothing for it to address. The engine would take the pair and
    // ignore the mask, which reads as "inpainting is broken" rather than as the
    // request being incomplete.
    if request.mask_image_base64.is_some() && request.init_image_base64.is_none() {
        return Err("A mask needs a source image to paint over".to_string());
    }
    for (label, image) in [
        ("Mask", &request.mask_image_base64),
        ("Control image", &request.control_image_base64),
        ("Reference image", &request.ip_adapter_image_base64),
    ] {
        if image
            .as_ref()
            .is_some_and(|encoded| encoded.len() > MAX_INIT_IMAGE_BYTES)
        {
            return Err(format!("{label} exceeds its size limit"));
        }
    }
    if request.ref_images_base64.len() > MAX_REF_IMAGES {
        return Err(format!(
            "At most {MAX_REF_IMAGES} reference images can be used at once"
        ));
    }
    for image in &request.ref_images_base64 {
        if image.trim().is_empty() {
            return Err("A reference image is empty".to_string());
        }
        if image.len() > MAX_INIT_IMAGE_BYTES {
            return Err("Reference image exceeds its size limit".to_string());
        }
    }
    for (label, strength) in [
        ("Control strength", request.control_strength),
        ("Reference strength", request.ip_adapter_strength),
    ] {
        if let Some(strength) = strength {
            if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
                return Err(format!("{label} must be between 0 and 1"));
            }
        }
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
        // A clip is one artifact per run: the engine's batch field counts
        // images, and asking a video job for eight of them would multiply the
        // longest run in the app by eight without the UI ever offering it.
        normalized.batch_count = 1;
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
    if let Some(mask) = &request.mask_image_base64 {
        body["mask_image"] = json!(mask);
    }
    if let Some(prompt) = &request.ad_prompt {
        body["ad_prompt"] = json!(prompt);
    }
    if let Some(prompt) = &request.ad_negative_prompt {
        body["ad_negative_prompt"] = json!(prompt);
    }
    if let Some(control) = &request.control_image_base64 {
        body["control_image"] = json!(control);
    }
    if let Some(strength) = request.control_strength {
        body["control_strength"] = json!(strength);
    }
    if let Some(reference) = &request.ip_adapter_image_base64 {
        body["ip_adapter_image"] = json!(reference);
    }
    if let Some(strength) = request.ip_adapter_strength {
        body["ip_adapter_strength"] = json!(strength);
    }
    if !request.ref_images_base64.is_empty() {
        body["ref_images"] = json!(request.ref_images_base64);
        // Only sent alongside the images it describes: on its own it would ask
        // an engine build that predates the field for something new on every
        // ordinary run.
        body["increase_ref_index"] = json!(request.increase_ref_index);
    }
    if request.task.is_video() {
        body["video_frames"] = json!(request.video_frames);
        body["fps"] = json!(request.fps);
        body["output_format"] = json!("webm");
    } else {
        body["output_format"] = json!("png");
        // Sent only when it is more than the default, so an engine build that
        // predates the field is asked nothing new for the ordinary run.
        if request.batch_count > 1 {
            body["batch_count"] = json!(request.batch_count);
        }
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
    Running {
        queue_position: u32,
    },
    /// Every artifact the job produced, in the engine's own order. One for a
    /// clip or a single image; `batch_count` of them for a batch.
    Completed(Vec<GeneratedMedia>),
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
            engine_error_text(value)
                .unwrap_or("Generation failed")
                .to_string(),
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
            //
            // The list is read whole. A batch of four comes back as four
            // entries in it, and taking only the first would spend the whole
            // run and then throw three quarters of it away.
            let media_type = result
                .get("mime_type")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    result
                        .get("output_format")
                        .and_then(Value::as_str)
                        .map(media_type_for_format)
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let frame_count = result
                .get("frame_count")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32;
            let fps = result.get("fps").and_then(Value::as_u64).unwrap_or(1) as u32;

            let encoded: Vec<&str> = match result.get("images").and_then(Value::as_array) {
                Some(images) => images
                    .iter()
                    .map(|image| {
                        image
                            .get("b64_json")
                            .and_then(Value::as_str)
                            .ok_or("A generated image carried no payload")
                    })
                    .collect::<Result<_, _>>()?,
                None => vec![result
                    .get("b64_json")
                    .and_then(Value::as_str)
                    .ok_or("Generation result carried no payload")?],
            };
            if encoded.is_empty() {
                return Err("Generation result carried no payload".to_string());
            }
            let media = encoded
                .into_iter()
                .map(|encoded| {
                    // Reject before decoding: base64 is 4/3 the size of its
                    // payload, so this bounds the allocation rather than
                    // discovering it afterwards.
                    if encoded.len() / 4 * 3 > MAX_MEDIA_BYTES {
                        return Err("Generated media exceeds its size limit".to_string());
                    }
                    let bytes = STANDARD
                        .decode(encoded)
                        .map_err(|_| "Generation result is not valid base64".to_string())?;
                    if bytes.is_empty() {
                        return Err("Generated media is empty".to_string());
                    }
                    Ok(GeneratedMedia {
                        media_type: media_type.clone(),
                        frame_count,
                        fps,
                        bytes,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(JobProgress::Completed(media))
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
        "The engine could not detect a model version in that file. A standalone UNet or DiT belongs on --diffusion-model; only an all-in-one checkpoint belongs on --model. Check which slot the file is on — either direction produces this error.",
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
                let Ok(mut cell) = sampling.lock() else {
                    return false;
                };
                *cell = Some(progress);
                // A redrawn bar is noise in a failure message; the tail is for
                // the lines a person reads.
                return true;
            }
            let Ok(mut buffer) = tail.lock() else {
                return false;
            };
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
    /// Whether this instance has answered once, so it is serving rather than
    /// still loading weights. Anything that only *reads* from the engine has to
    /// wait for this: a probe abandoned mid-load leaves its handler thread
    /// blocked, which is what `ensure_ready` waits out its whole deadline on one
    /// request to avoid.
    ready: bool,
    /// Tail of the engine's stderr, drained by a reader thread so the pipe can
    /// never fill and block the child.
    stderr_tail: Option<Arc<Mutex<String>>>,
    /// Latest `(step, total)` scraped from that same stream.
    sampling: Option<SamplingProgress>,
}

impl GenerationEngineState {
    pub fn loaded_model(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.model_id.clone())
    }

    /// Where the running instance is listening, if one is.
    pub fn base_url(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.port)
            .map(|port| format!("http://127.0.0.1:{port}"))
    }

    /// [`base_url`](Self::base_url), but only once the engine has answered.
    ///
    /// For callers that merely ask the engine something rather than driving a
    /// run: an instance that is still loading a 20 GB model accepts the
    /// connection and holds it, so probing it with a short deadline burns a
    /// worker thread for nothing.
    pub fn ready_base_url(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .filter(|state| state.ready)
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
        state.ready = false;
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
            .and_then(|tail| {
                tail.lock()
                    .ok()
                    .map(|value| engine_failure_detail(value.trim()))
            })
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
            // Wait out the whole deadline on one probe rather than abandoning a
            // short one every half second. `sd-server` answers `/capabilities`
            // from its worker pool, and a big model warms up for a minute or
            // more before the first answer comes back — every probe we give up
            // on leaves its handler thread blocked, so a 2s timeout drains the
            // pool in seconds and the engine can never answer at all. A dead
            // child drops the connection, so this still fails fast.
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1));
            if let Ok(response) =
                crate::egress::send(client.get(&capabilities).timeout(remaining)).await
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
                    // It has answered once, so everything else may now ask it
                    // things without waiting out a load it cannot see.
                    if let Ok(mut state) = self.inner.lock() {
                        state.ready = true;
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
    let response = crate::egress::send(
        client
            .post(format!("{base_url}{}", request.task.endpoint()))
            .json(&request_body(spec, request)),
    )
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
    let response = crate::egress::send(client.get(format!("{base_url}/sdcpp/v1/jobs/{job_id}")))
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
    crate::egress::send(client.post(format!("{base_url}/sdcpp/v1/jobs/{job_id}/cancel")))
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

/// What the engine that is running right now says it can do.
///
/// Every field here used to be a list compiled into the frontend, which meant a
/// new stable-diffusion.cpp release was unusable until somebody edited an array
/// by hand. The engine builds all of them from its own enums —
/// `sd_sample_method_name` over `SAMPLE_METHOD_COUNT`, `sd_scheduler_name` over
/// `SCHEDULER_COUNT`, the upscaler directory rescanned per call — so asking it
/// is the only way to be right about the build that is actually loaded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineCapabilities {
    pub samplers: Vec<String>,
    /// Includes the `normal` alias the engine reports beside `discrete`.
    pub schedulers: Vec<String>,
    /// Built-ins plus whatever models were found in `--hires-upscalers-dir`.
    pub upscalers: Vec<String>,
    /// The feature flags for the mode this engine is in, verbatim:
    /// `init_image`, `mask_image`, `control_image`, `ip_adapter_image`,
    /// `ref_images`, `lora`, `vae_tiling`, `hires`, `cache`, `cancel_queued`,
    /// `cancel_generating`. Not an enum, because a build newer than this one
    /// naming a flag we have never heard of should still reach the UI.
    pub features: BTreeMap<String, bool>,
}

/// Reads `[{"name": ...}, ...]`, which is how the engine reports its upscalers
/// and LoRAs — unlike samplers and schedulers, which are bare strings.
fn named_entries(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn string_entries(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn decode_capabilities(body: &Value) -> EngineCapabilities {
    EngineCapabilities {
        samplers: string_entries(body, "samplers"),
        schedulers: string_entries(body, "schedulers"),
        upscalers: named_entries(body, "upscalers"),
        features: body
            .get("features")
            .and_then(Value::as_object)
            .map(|flags| {
                flags
                    .iter()
                    .filter_map(|(name, value)| value.as_bool().map(|flag| (name.clone(), flag)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Asks the running engine what it supports.
///
/// The same endpoint `ensure_ready` polls to decide the engine is up, so a
/// successful answer here means the server is serving and holding weights.
pub async fn fetch_capabilities(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<EngineCapabilities, String> {
    let response = crate::egress::send(client.get(format!("{base_url}/sdcpp/v1/capabilities")))
        .await
        .map_err(|error| format!("Read engine capabilities: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Generation engine returned {} for its capabilities",
            response.status()
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("Generation engine returned unreadable capabilities: {error}"))?;
    Ok(decode_capabilities(&body))
}

// ---------------------------------------------------------------------------
// Remote backends
// ---------------------------------------------------------------------------
//
// The managed `sd-server` above runs weight files the app itself downloaded and
// verified, which is why every model there is a set of component slots. Two
// other engines generate images without any of that machinery, and neither can
// be bundled: ComfyUI is a Python process the user installs and GPL-3.0, and a
// hosted OpenAI-compatible endpoint has no local weights at all. Both are
// reached over HTTP at arm's length — nothing is linked and nothing is shipped,
// so this app's MIT license is unaffected.
//
// A remote backend is deliberately *not* a [`GenerationModelSpec`]: it has no
// components to validate, nothing to download, and no launch line. It is a
// destination plus the model names that destination serves.

/// How a remote backend is spoken to. The two protocols share nothing beyond
/// "POST a prompt, get an image back".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteBackendKind {
    /// A ComfyUI server. Generation is a workflow graph the user supplies;
    /// the app substitutes prompt and canvas into it and never authors nodes.
    ComfyUi,
    /// `POST /images/generations` with the key already in the OS keychain.
    OpenAiCompatible,
}

/// The `model_id` prefix that routes a run away from the managed engine.
///
/// Remote entries share the model picker with library models, so they share the
/// id space too. A prefix keeps one field doing one job — a request either
/// names a library model or names a backend and one of its models — instead of
/// adding a mode flag that every other field then has to be read against.
pub const REMOTE_MODEL_PREFIX: &str = "remote:";

/// Builds the picker id for one model on one backend.
pub fn remote_model_id(backend_id: &str, model: &str) -> String {
    format!("{REMOTE_MODEL_PREFIX}{backend_id}:{model}")
}

/// Splits a [`remote_model_id`] back into its parts.
///
/// The model name is the remainder after the *first* separator, not up to the
/// last one: hosted model names contain colons (`black-forest-labs/flux:1.1`),
/// and splitting from the right would silently address a different model.
pub fn parse_remote_model_id(model_id: &str) -> Option<(&str, &str)> {
    let rest = model_id.strip_prefix(REMOTE_MODEL_PREFIX)?;
    let (backend_id, model) = rest.split_once(':')?;
    if backend_id.is_empty() || model.is_empty() {
        return None;
    }
    Some((backend_id, model))
}

/// One user-registered remote generation endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBackend {
    pub id: String,
    pub label: String,
    pub kind: RemoteBackendKind,
    /// Base URL of the ComfyUI server, or of the OpenAI-compatible API. Empty
    /// on an OpenAI-compatible backend falls back to the provider's own base.
    pub base_url: String,
    /// Which saved provider key to authenticate with. OpenAI-compatible only.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// The ComfyUI API-format workflow, with `{{prompt}}`-style placeholders.
    #[serde(default)]
    pub workflow_template: Option<Value>,
    /// Whether this endpoint accepts an init image on `/images/edits`.
    #[serde(default)]
    pub supports_editing: bool,
    /// Model names to offer in the picker. A ComfyUI workflow names its own
    /// checkpoint, so one placeholder entry is enough there.
    pub models: Vec<String>,
}

impl RemoteBackend {
    /// The tasks this backend can be asked for.
    ///
    /// Video and speech are absent on purpose: a ComfyUI graph *can* produce
    /// video, but which node does and how to read it back is workflow-specific
    /// and cannot be inferred from a template the app did not author.
    pub fn tasks(&self) -> Vec<GenerationTask> {
        match self.kind {
            RemoteBackendKind::ComfyUi => vec![GenerationTask::TextToImage],
            RemoteBackendKind::OpenAiCompatible if self.supports_editing => {
                vec![GenerationTask::TextToImage, GenerationTask::ImageToImage]
            }
            RemoteBackendKind::OpenAiCompatible => vec![GenerationTask::TextToImage],
        }
    }
}

pub fn validate_remote_backend(backend: &RemoteBackend) -> Result<(), String> {
    if backend.id.trim().is_empty() || backend.id.len() > 64 {
        return Err("A backend needs an id of at most 64 characters".to_string());
    }
    // The id is half of a `remote:<id>:<model>` picker id, so a colon in it
    // would make that id parse back to a different backend than it names.
    if backend
        .id
        .contains(|c: char| c == ':' || c == '/' || c.is_whitespace())
    {
        return Err("A backend id may not contain colons, slashes or spaces".to_string());
    }
    if backend.label.trim().is_empty() || backend.label.len() > 120 {
        return Err("A backend needs a label of at most 120 characters".to_string());
    }
    if backend.models.is_empty() {
        return Err("List at least one model this backend serves".to_string());
    }
    if backend.models.len() > 64 {
        return Err("At most 64 models can be listed for one backend".to_string());
    }
    for model in &backend.models {
        if model.trim().is_empty() || model.len() > 200 {
            return Err("Each model name must be 1 to 200 characters".to_string());
        }
    }
    match backend.kind {
        RemoteBackendKind::ComfyUi => {
            if backend.workflow_template.is_none() {
                return Err("A ComfyUI backend needs an API-format workflow".to_string());
            }
            if backend.base_url.trim().is_empty() {
                return Err("A ComfyUI backend needs a base URL".to_string());
            }
        }
        RemoteBackendKind::OpenAiCompatible => {
            if backend
                .provider_id
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            {
                return Err(
                    "An OpenAI-compatible backend needs the provider whose key it uses".to_string(),
                );
            }
        }
    }
    if !backend.base_url.trim().is_empty() {
        let url = backend.base_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("A backend base URL must start with http:// or https://".to_string());
        }
        if url.len() > 400 {
            return Err("Backend base URL is too long".to_string());
        }
    }
    Ok(())
}

/// The remote counterpart of [`validate_request`].
///
/// It cannot reuse that function: every bound there is read off a
/// [`GenerationModelSpec`] — supported tasks, default sampler, frame grid — and
/// a remote backend has none of those. What is shared is the part that protects
/// the *caller* rather than the engine, so those bounds are repeated here
/// rather than relaxed.
pub fn validate_remote_request(
    backend: &RemoteBackend,
    request: &GenerationRequest,
) -> Result<GenerationRequest, String> {
    if !backend.tasks().contains(&request.task) {
        return Err(format!("{} does not support this task", backend.label));
    }
    if request.prompt.trim().is_empty() {
        return Err("A prompt is required".to_string());
    }
    if request.prompt.len() > MAX_PROMPT_BYTES || request.negative_prompt.len() > MAX_PROMPT_BYTES {
        return Err("Prompt exceeds its size limit".to_string());
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

    let mut normalized = request.clone();
    normalized.width = normalize_dimension(request.width);
    normalized.height = normalize_dimension(request.height);
    normalized.fps = 1;
    normalized.video_frames = 1;
    // Nothing below reaches a remote engine: LoRAs are local files, component
    // overrides name local weight slots, hires is an `sd-server` pass, and the
    // conditioning images are `sd-server` request fields that neither the
    // ComfyUI workflow encoder nor the OpenAI-compatible one emits. They are
    // dropped rather than rejected so switching the picker from a library model
    // to a backend does not invalidate the controls already set — but they are
    // dropped *here*, so a run can never appear to be conditioned by an image
    // the backend was never sent.
    normalized.loras.clear();
    normalized.component_overrides.clear();
    normalized.hires = None;
    normalized.mask_image_base64 = None;
    normalized.ad_prompt = None;
    normalized.ad_negative_prompt = None;
    normalized.control_image_base64 = None;
    normalized.control_strength = None;
    normalized.ip_adapter_image_base64 = None;
    normalized.ip_adapter_strength = None;
    normalized.ref_images_base64.clear();
    normalized.increase_ref_index = false;
    Ok(normalized)
}

/// Substitutes generation parameters into a ComfyUI workflow template.
///
/// Every string leaf is scanned, so a placeholder works wherever the user put
/// it — the app has no idea which node in *their* graph is the sampler.
///
/// A leaf that is *only* a placeholder is replaced by a typed value, not by
/// text: ComfyUI validates `steps` and `width` as numbers and rejects the graph
/// outright if they arrive quoted. A placeholder embedded in a longer string
/// (`"masterpiece, {{prompt}}"`) can only be text, so it is spliced instead.
pub fn replace_workflow_placeholders(value: &mut Value, request: &GenerationRequest, model: &str) {
    match value {
        Value::String(text) => {
            *value = match text.as_str() {
                "{{prompt}}" => Value::String(request.prompt.clone()),
                "{{negative_prompt}}" => Value::String(request.negative_prompt.clone()),
                "{{model}}" => Value::String(model.to_string()),
                "{{width}}" => Value::from(request.width),
                "{{height}}" => Value::from(request.height),
                "{{steps}}" => Value::from(request.steps),
                "{{cfg_scale}}" => Value::from(request.cfg_scale),
                "{{seed}}" => Value::from(request.seed),
                _ => {
                    if !text.contains("{{") {
                        return;
                    }
                    Value::String(
                        text.replace("{{prompt}}", &request.prompt)
                            .replace("{{negative_prompt}}", &request.negative_prompt)
                            .replace("{{model}}", model)
                            .replace("{{width}}", &request.width.to_string())
                            .replace("{{height}}", &request.height.to_string())
                            .replace("{{steps}}", &request.steps.to_string())
                            .replace("{{cfg_scale}}", &request.cfg_scale.to_string())
                            .replace("{{seed}}", &request.seed.to_string()),
                    )
                }
            };
        }
        Value::Array(items) => {
            for item in items {
                replace_workflow_placeholders(item, request, model);
            }
        }
        Value::Object(entries) => {
            for entry in entries.values_mut() {
                replace_workflow_placeholders(entry, request, model);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path the host agrees is absolute.
    ///
    /// `/Users/somebody/x` is absolute on Unix and *not* on Windows, which has
    /// no drive letter to anchor it — and absoluteness is exactly what the
    /// rules under test are about, so a hardcoded POSIX literal tests nothing
    /// on Windows but the fixture.
    fn absolute(parts: &[&str]) -> String {
        let mut path = std::env::temp_dir();
        path.extend(parts);
        path.to_string_lossy().to_string()
    }

    /// The same join the code under test performs, so an expectation carries
    /// the host's own separator rather than a `/` that only Unix produces.
    fn under(root: &str, parts: &[&str]) -> String {
        let mut path = PathBuf::from(root);
        path.extend(parts);
        path.to_string_lossy().to_string()
    }

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
            ad_prompt: None,
            ad_negative_prompt: None,
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
            batch_count: 1,
            video_frames: 34,
            fps: 24,
            speaker_file: None,
            language: None,
            init_image_base64: None,
            mask_image_base64: None,
            control_image_base64: None,
            control_strength: None,
            ip_adapter_image_base64: None,
            ip_adapter_strength: None,
            ref_images_base64: Vec::new(),
            increase_ref_index: false,
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
                path: absolute(&["models", "my-own.safetensors"]),
            },
            size_bytes: 1,
        }];
        assert_eq!(
            launch_args(&spec, Path::new("/app/models"), 8092)
                .windows(2)
                .find(|pair| pair[0] == "--model")
                .map(|pair| pair[1].clone()),
            Some(absolute(&["models", "my-own.safetensors"]))
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
            assert_eq!(
                args[at + 1],
                under("/models", &["wan-mine", file]),
                "{flag}"
            );
        }
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--listen-port", "8092"]));
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
        spec.components.push(ModelComponent::huggingface(
            ComponentSlot::Vae,
            "r",
            "vae.safetensors",
            1,
        ));
        assert_ne!(before, launch_signature(&spec, root));
        // The port is the one thing that legitimately differs between two
        // launches of the same file set, so it must not be in the key.
        assert_eq!(launch_signature(&spec, root), launch_args(&spec, root, 0),);
    }

    /// A checkpoint that needs a separate VAE does not name one, so the common
    /// case is adding a slot the model never had — not swapping one it did.
    #[test]
    fn a_chosen_part_is_added_or_swapped_and_never_invented() {
        let root = Path::new("/models");
        let mut spec = video_model();
        spec.components = vec![
            ModelComponent::huggingface(ComponentSlot::DiffusionModel, "r", "unet.gguf", 1),
            ModelComponent::huggingface(ComponentSlot::Vae, "r", "own.safetensors", 1),
        ];
        let parts = vec![
            PartAsset {
                slot: ComponentSlot::Vae,
                name: "Better VAE".to_string(),
                path: absolute(&["parts", "better_vae.safetensors"]),
            },
            PartAsset {
                slot: ComponentSlot::T5xxl,
                name: "umt5".to_string(),
                path: absolute(&["parts", "umt5.safetensors"]),
            },
        ];

        let chosen = vec![
            ComponentOverride {
                slot: ComponentSlot::Vae,
                path: parts[0].path.clone(),
            },
            ComponentOverride {
                slot: ComponentSlot::T5xxl,
                path: parts[1].path.clone(),
            },
        ];
        let effective = apply_component_overrides(&spec, &parts, &chosen).expect("chosen");
        let args = launch_args(&effective, root, 8092);
        let vae = args.iter().position(|arg| arg == "--vae").expect("--vae");
        assert_eq!(args[vae + 1], parts[0].path);
        // Swapped, not duplicated.
        assert_eq!(args.iter().filter(|arg| *arg == "--vae").count(), 1);
        // The model had no text encoder at all; choosing one adds it.
        let t5 = args
            .iter()
            .position(|arg| arg == "--t5xxl")
            .expect("--t5xxl");
        assert_eq!(args[t5 + 1], parts[1].path);
        let unet = args
            .iter()
            .position(|arg| arg == "--diffusion-model")
            .unwrap();
        assert_eq!(args[unet + 1], under("/models", &["wan-mine", "unet.gguf"]));

        // A path the library does not hold is not loadable, whatever the UI says.
        let forged = vec![ComponentOverride {
            slot: ComponentSlot::Vae,
            path: absolute(&["not-in-the-library.safetensors"]),
        }];
        assert!(apply_component_overrides(&spec, &parts, &forged).is_err());

        // Nor is a real part offered under the wrong slot.
        let mismatched = vec![ComponentOverride {
            slot: ComponentSlot::ClipL,
            path: parts[0].path.clone(),
        }];
        assert!(apply_component_overrides(&spec, &parts, &mismatched).is_err());

        // The denoiser is what the model is, and is not a per-run dropdown.
        let denoiser = vec![PartAsset {
            slot: ComponentSlot::Checkpoint,
            name: "Another model".to_string(),
            path: absolute(&["parts", "other.safetensors"]),
        }];
        assert!(apply_component_overrides(
            &spec,
            &denoiser,
            &[ComponentOverride {
                slot: ComponentSlot::Checkpoint,
                path: denoiser[0].path.clone(),
            }],
        )
        .is_err());
    }

    #[test]
    fn a_library_lora_is_named_and_absolute() {
        let good = LoraAsset {
            name: "Detail slider".to_string(),
            path: absolute(&["loras", "detail.safetensors"]),
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
        assert_eq!(
            normalize_video_frames(DownTo4nPlus1, u32::MAX),
            MAX_VIDEO_FRAMES
        );

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
        let h3 = model(
            "h3-mine",
            vec![GenerationTask::TextToVideo],
            FrameGrid::UpTo17kPlus5,
        );
        let mut on_h3 = video_request(GenerationTask::TextToVideo);
        on_h3.model_id = h3.id.clone();
        assert_eq!(validate_request(&h3, &on_h3).unwrap().video_frames, 39);

        // Image-driven tasks cannot silently fall back to text-only.
        assert!(validate_request(&wan, &video_request(GenerationTask::ImageToVideo)).is_err());

        // A model that does not declare video must refuse a video task.
        let stills = model(
            "sd-mine",
            vec![GenerationTask::TextToImage],
            FrameGrid::DownTo4nPlus1,
        );
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

        let mut stills = model(
            "sd-mine",
            vec![GenerationTask::TextToImage],
            FrameGrid::DownTo4nPlus1,
        );
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

    fn image_model() -> GenerationModelSpec {
        let mut spec = model(
            "sdxl-mine",
            vec![GenerationTask::TextToImage, GenerationTask::ImageToImage],
            FrameGrid::DownTo4nPlus1,
        );
        spec.components[0].slot = ComponentSlot::Checkpoint;
        spec
    }

    fn image_request(task: GenerationTask) -> GenerationRequest {
        let mut request = video_request(task);
        request.model_id = "sdxl-mine".to_string();
        request.width = 1024;
        request.height = 1024;
        request
    }

    fn local_component(slot: ComponentSlot, name: &str) -> ModelComponent {
        ModelComponent {
            slot,
            source: ComponentSource::LocalFile {
                path: absolute(&["weights", name]),
            },
            size_bytes: 1,
        }
    }

    /// Inpainting, ControlNet and the reference-image conditioners all reach
    /// the engine as extra fields on the ordinary request, so the check is that
    /// each one is carried verbatim and that none of them appears when it was
    /// not asked for — an engine build predating a field must not be sent it on
    /// every ordinary run.
    #[test]
    fn conditioning_images_reach_the_request_body_only_when_supplied() {
        let sdxl = image_model();
        let mut request = image_request(GenerationTask::ImageToImage);
        request.init_image_base64 = Some("aW5pdA==".to_string());
        request.mask_image_base64 = Some("bWFzaw==".to_string());
        request.control_image_base64 = Some("Y29udHJvbA==".to_string());
        request.control_strength = Some(0.65);
        request.ip_adapter_image_base64 = Some("cmVm".to_string());
        request.ip_adapter_strength = Some(0.4);
        request.ref_images_base64 = vec!["b25l".to_string(), "dHdv".to_string()];
        request.increase_ref_index = true;

        let normalized = validate_request(&sdxl, &request).unwrap();
        let body = request_body(&sdxl, &normalized);
        assert_eq!(body["mask_image"], json!("bWFzaw=="));
        assert_eq!(body["control_image"], json!("Y29udHJvbA=="));
        assert_eq!(body["control_strength"], json!(0.65));
        assert_eq!(body["ip_adapter_image"], json!("cmVm"));
        assert_eq!(body["ip_adapter_strength"], json!(0.4));
        assert_eq!(body["ref_images"], json!(["b25l", "dHdv"]));
        assert_eq!(body["increase_ref_index"], json!(true));

        let plain = request_body(&sdxl, &image_request(GenerationTask::TextToImage));
        for absent in [
            "mask_image",
            "control_image",
            "control_strength",
            "ip_adapter_image",
            "ip_adapter_strength",
            "ref_images",
            "increase_ref_index",
        ] {
            assert!(plain.get(absent).is_none(), "{absent} was sent unasked");
        }
    }

    #[test]
    fn conditioning_inputs_are_bounded_and_a_mask_needs_something_to_mask() {
        let sdxl = image_model();

        // The engine takes the pair and ignores the mask, so this has to be
        // caught here or inpainting silently degrades to plain img2img.
        let mut orphan_mask = image_request(GenerationTask::TextToImage);
        orphan_mask.mask_image_base64 = Some("bWFzaw==".to_string());
        assert!(validate_request(&sdxl, &orphan_mask).is_err());

        let mut wild_strength = image_request(GenerationTask::TextToImage);
        wild_strength.control_strength = Some(1.5);
        assert!(validate_request(&sdxl, &wild_strength).is_err());
        wild_strength.control_strength = Some(f64::NAN);
        assert!(validate_request(&sdxl, &wild_strength).is_err());

        let mut too_many_refs = image_request(GenerationTask::TextToImage);
        too_many_refs.ref_images_base64 = vec!["b25l".to_string(); MAX_REF_IMAGES + 1];
        assert!(validate_request(&sdxl, &too_many_refs).is_err());

        let mut huge = image_request(GenerationTask::TextToImage);
        huge.control_image_base64 = Some("A".repeat(MAX_INIT_IMAGE_BYTES + 1));
        assert!(validate_request(&sdxl, &huge).is_err());
    }

    /// `sd-server` accepts `control_image` with no `--control-net` loaded and
    /// quietly ignores it, so the only symptom would be a three-minute render
    /// that followed none of the structure it was given.
    #[test]
    fn a_conditioning_image_is_refused_when_its_weights_are_not_loaded() {
        let plain = image_model();
        let mut request = image_request(GenerationTask::TextToImage);
        request.control_image_base64 = Some("Y29udHJvbA==".to_string());
        assert!(validate_conditioning(&plain, &request).is_err());

        let mut with_control_net = plain.clone();
        with_control_net.components.push(local_component(
            ComponentSlot::ControlNet,
            "canny.safetensors",
        ));
        assert!(validate_conditioning(&with_control_net, &request).is_ok());

        // A ControlNet does not stand in for an IP-Adapter: the three fields
        // are read by three different sets of weights.
        let mut reference = image_request(GenerationTask::TextToImage);
        reference.ip_adapter_image_base64 = Some("cmVm".to_string());
        assert!(validate_conditioning(&with_control_net, &reference).is_err());

        let mut identities = image_request(GenerationTask::TextToImage);
        identities.ref_images_base64 = vec!["b25l".to_string()];
        assert!(validate_conditioning(&with_control_net, &identities).is_err());
        let mut with_photo_maker = plain.clone();
        with_photo_maker.components.push(local_component(
            ComponentSlot::PhotoMaker,
            "photomaker.safetensors",
        ));
        assert!(validate_conditioning(&with_photo_maker, &identities).is_ok());

        // Nothing sent, nothing to refuse, whatever is loaded.
        assert!(validate_conditioning(&plain, &image_request(GenerationTask::TextToImage)).is_ok());
    }

    /// An ADetailer prompt without a detector is the same silent failure as a
    /// conditioning image without its weights: `ad_model_path` is the one
    /// ADetailer field the server does not read from the request body, so an
    /// engine launched without `--ad-model` accepts the prompt and re-details
    /// nothing.
    #[test]
    fn an_adetailer_prompt_is_refused_without_a_detector() {
        let plain = image_model();
        let mut request = image_request(GenerationTask::TextToImage);
        request.ad_prompt = Some("a sharp face".to_string());
        assert!(validate_conditioning(&plain, &request).is_err());

        let mut with_detector = plain.clone();
        with_detector
            .components
            .push(local_component(ComponentSlot::AdModel, "face_yolov8n.gguf"));
        assert!(validate_conditioning(&with_detector, &request).is_ok());

        // The negative prompt reaches the detector through the same flag, so it
        // is refused on its own too rather than only alongside a positive one.
        let mut negative_only = image_request(GenerationTask::TextToImage);
        negative_only.ad_negative_prompt = Some("blurry".to_string());
        assert!(validate_conditioning(&plain, &negative_only).is_err());
        assert!(validate_conditioning(&with_detector, &negative_only).is_ok());
    }

    /// `--ip-adapter` reads its reference through a CLIP vision tower and
    /// `sd-server` fails at load without one, so the model is rejected while it
    /// is being added rather than the first time it is run.
    #[test]
    fn an_ip_adapter_model_must_also_carry_a_clip_vision_encoder() {
        let mut spec = image_model();
        spec.components.push(local_component(
            ComponentSlot::IpAdapter,
            "ip-adapter.safetensors",
        ));
        assert!(validate_model_spec(&spec).is_err());

        spec.components.push(local_component(
            ComponentSlot::ClipVision,
            "clip-vision.safetensors",
        ));
        assert!(validate_model_spec(&spec).is_ok());
    }

    /// A remote backend is sent a workflow or an OpenAI-compatible body, and
    /// neither encoder emits any of these fields. Dropping them here is what
    /// keeps a run from looking conditioned by an image the backend never saw.
    #[test]
    fn conditioning_never_survives_the_hop_to_a_remote_backend() {
        let backend = RemoteBackend {
            id: "comfy".to_string(),
            label: "My ComfyUI".to_string(),
            kind: RemoteBackendKind::ComfyUi,
            base_url: "http://127.0.0.1:8188".to_string(),
            provider_id: None,
            workflow_template: None,
            supports_editing: false,
            models: vec!["sd_xl_base_1.0.safetensors".to_string()],
        };
        let mut request = image_request(GenerationTask::TextToImage);
        request.mask_image_base64 = Some("bWFzaw==".to_string());
        request.control_image_base64 = Some("Y29udHJvbA==".to_string());
        request.control_strength = Some(0.5);
        request.ip_adapter_image_base64 = Some("cmVm".to_string());
        request.ip_adapter_strength = Some(0.5);
        request.ref_images_base64 = vec!["b25l".to_string()];
        request.increase_ref_index = true;

        let normalized = validate_remote_request(&backend, &request).unwrap();
        assert!(normalized.mask_image_base64.is_none());
        assert!(normalized.control_image_base64.is_none());
        assert!(normalized.control_strength.is_none());
        assert!(normalized.ip_adapter_image_base64.is_none());
        assert!(normalized.ip_adapter_strength.is_none());
        assert!(normalized.ref_images_base64.is_empty());
        assert!(!normalized.increase_ref_index);
    }

    /// Every new slot is a flag `sd-server` actually accepts; a typo here is an
    /// engine that refuses to launch, which is why the mapping is asserted
    /// rather than trusted.
    #[test]
    fn the_conditioning_and_exotic_slots_map_to_real_engine_flags() {
        for (slot, flag) in [
            (ComponentSlot::ControlNet, "--control-net"),
            (ComponentSlot::IpAdapter, "--ip-adapter"),
            (ComponentSlot::PhotoMaker, "--photo-maker"),
            (ComponentSlot::PulidWeights, "--pulid-weights"),
            (ComponentSlot::LlmVision, "--llm_vision"),
            (
                ComponentSlot::UncondDiffusionModel,
                "--uncond-diffusion-model",
            ),
            (
                ComponentSlot::EmbeddingsConnectors,
                "--embeddings-connectors",
            ),
            (ComponentSlot::MotionModule, "--motion-module"),
        ] {
            assert_eq!(slot.flag(), flag);
            // None of them belongs to `llama-tts`, so all of them reach the
            // `sd-server` command line rather than being skipped.
            assert!(
                !slot.is_speech_only(),
                "{slot:?} was skipped as speech-only"
            );
        }

        // Only the three that unlock a per-run image say so.
        assert_eq!(
            ComponentSlot::ControlNet.conditioning_image(),
            Some(ConditioningImage::Control)
        );
        assert_eq!(
            ComponentSlot::IpAdapter.conditioning_image(),
            Some(ConditioningImage::IpAdapter)
        );
        assert_eq!(
            ComponentSlot::PhotoMaker.conditioning_image(),
            Some(ConditioningImage::Reference)
        );
        assert_eq!(
            ComponentSlot::PulidWeights.conditioning_image(),
            Some(ConditioningImage::Reference)
        );
        assert_eq!(ComponentSlot::Vae.conditioning_image(), None);
        assert_eq!(ComponentSlot::MotionModule.conditioning_image(), None);
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
                path: absolute(&["loras", "style.safetensors"]),
                multiplier: 0.8,
                is_high_noise: false,
            },
            LoraSelection {
                path: absolute(&["loras", "motion.safetensors"]),
                multiplier: -0.4,
                is_high_noise: true,
            },
        ];
        let normalized = validate_request(&wan, &request).unwrap();
        let body = request_body(&wan, &normalized);
        let loras = body["lora"].as_array().expect("lora array");
        assert_eq!(loras.len(), 2);
        assert_eq!(
            loras[0]["path"],
            json!(absolute(&["loras", "style.safetensors"]))
        );
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
                path: absolute(&["vae.safetensors"]),
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
        assert_eq!(media.len(), 1, "a clip is one artifact");
        assert_eq!(media[0].bytes, b"webm-bytes");
        assert_eq!(media[0].media_type, "video/webm");
        assert_eq!(media[0].frame_count, 33);

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
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].bytes, b"png-bytes");
        assert_eq!(media[0].media_type, "image/png");
        assert_eq!(media[0].frame_count, 1);

        // A batch comes back as several entries in that same list. Reading
        // only the first is the bug this asserts against: the run has already
        // been paid for by the time the response arrives, so a dropped entry
        // is sampling time thrown away.
        let completed = decode_job_status(&json!({
            "status": "completed",
            "result": {
                "output_format": "png",
                "images": [
                    {"index": 0, "b64_json": STANDARD.encode(b"first")},
                    {"index": 1, "b64_json": STANDARD.encode(b"second")},
                    {"index": 2, "b64_json": STANDARD.encode(b"third")},
                ],
            }
        }))
        .unwrap();
        let JobProgress::Completed(media) = completed else {
            panic!("expected a completed job");
        };
        assert_eq!(media.len(), 3, "every image in the batch is kept");
        assert_eq!(media[0].bytes, b"first");
        assert_eq!(media[2].bytes, b"third");
        // The format is stated once for the batch, so every image carries it.
        assert!(media.iter().all(|item| item.media_type == "image/png"));

        // A completed job with no payload is a protocol error, not empty media.
        assert!(decode_job_status(&json!({"status": "completed", "result": {}})).is_err());
        assert!(decode_job_status(&json!({
            "status": "completed",
            "result": {"output_format": "png", "images": []}
        }))
        .is_err());
        assert!(decode_job_status(&json!({"status": "invented"})).is_err());
    }

    /// A batch is an image-only idea, and only travels when it is asked for.
    ///
    /// The engine samples a batch serially, so `batch_count` on a video job
    /// would multiply the app's longest run by eight; and an engine build that
    /// predates the field should still see exactly the body it saw before, so
    /// the ordinary single-image run must not carry it at all.
    #[test]
    fn a_batch_is_images_only_and_only_sent_when_asked_for() {
        let mut spec = model(
            "sdxl-mine",
            vec![GenerationTask::TextToImage],
            FrameGrid::default(),
        );
        spec.defaults.width = 512;
        spec.defaults.height = 512;

        let mut request = video_request(GenerationTask::TextToImage);
        request.model_id = spec.id.clone();
        let single = validate_request(&spec, &request).unwrap();
        assert!(request_body(&spec, &single).get("batch_count").is_none());

        request.batch_count = 4;
        let batched = validate_request(&spec, &request).unwrap();
        assert_eq!(request_body(&spec, &batched)["batch_count"], json!(4));

        // Out of range is refused rather than clamped: silently returning one
        // image for a request that asked for fifty is worse than saying no.
        request.batch_count = MAX_BATCH_COUNT + 1;
        assert!(validate_request(&spec, &request).is_err());
        request.batch_count = 0;
        assert!(validate_request(&spec, &request).is_err());

        // Video and speech normalize back to one whatever the caller sent.
        let wan = video_model();
        let mut clip = video_request(GenerationTask::TextToVideo);
        clip.batch_count = 4;
        let clip = validate_request(&wan, &clip).unwrap();
        assert_eq!(clip.batch_count, 1);
        assert!(request_body(&wan, &clip).get("batch_count").is_none());

        let mut voice = model(
            "voice",
            vec![GenerationTask::TextToSpeech],
            FrameGrid::default(),
        );
        voice.components = vec![ModelComponent::huggingface(
            ComponentSlot::Checkpoint,
            "r",
            "backbone.gguf",
            1,
        )];
        let mut utterance = video_request(GenerationTask::TextToSpeech);
        utterance.batch_count = 4;
        assert_eq!(validate_request(&voice, &utterance).unwrap().batch_count, 1);
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
        assert!(
            request_body(&wan, &validate_request(&wan, &text_only).unwrap())
                .get("strength")
                .is_none()
        );

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
        assert_eq!(
            parse_sampling_progress("[INFO ] main.cpp:148 - listening"),
            None
        );
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
        let mut spec = model(
            "voice",
            vec![GenerationTask::TextToSpeech],
            FrameGrid::default(),
        );
        spec.components = vec![
            ModelComponent::huggingface(ComponentSlot::Checkpoint, "r", "backbone.gguf", 1),
            ModelComponent::huggingface(ComponentSlot::Mmproj, "r", "mmproj.gguf", 1),
        ];
        spec.extra_launch_args = vec!["--tts-use-guide-tokens".to_string()];
        assert!(validate_model_spec(&spec).is_ok());

        let mut request = video_request(GenerationTask::TextToSpeech);
        request.model_id = spec.id.clone();
        request.speaker_file = Some(absolute(&["reference.wav"]));
        request.language = Some("EN".to_string());
        let normalized = validate_request(&spec, &request).unwrap();
        assert_eq!(normalized.width, 0);

        let args = speech_args(&spec, Path::new("/m"), &normalized, Path::new("/out.wav")).unwrap();
        for (flag, value) in [
            ("--model", under("/m", &["voice", "backbone.gguf"])),
            ("--mmproj", under("/m", &["voice", "mmproj.gguf"])),
            ("--prompt", "a lovely cat".to_string()),
            (
                "--output",
                Path::new("/out.wav").to_string_lossy().to_string(),
            ),
            ("--tts-speaker-file", absolute(&["reference.wav"])),
            ("--tts-lang", "en".to_string()),
        ] {
            let at = args.iter().position(|arg| arg == flag).expect(flag);
            assert_eq!(args[at + 1], value, "{flag}");
        }
        assert!(args.contains(&"--tts-use-guide-tokens".to_string()));

        // Speech asks for the GPU by default. Without this the pinned macOS
        // arm64 build — which ships libggml-metal.dylib — synthesizes on the
        // CPU of a machine whose GPU is sitting idle.
        let ngl = args.iter().position(|arg| arg == "-ngl").expect("-ngl");
        assert_eq!(args[ngl + 1], SPEECH_GPU_LAYERS.to_string());
        // ...and the model's own arguments come after it, so llama.cpp's
        // last-one-wins parsing leaves the escape hatch open.
        let mut cpu_only = spec.clone();
        cpu_only.extra_launch_args = vec!["-ngl".to_string(), "0".to_string()];
        let args =
            speech_args(&cpu_only, Path::new("/m"), &normalized, Path::new("/o.wav")).unwrap();
        assert_eq!(args.last().unwrap(), "0");

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
            request.speaker_file = reference.map(|path| path.to_string_lossy().to_string());
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
        let mut spec = model(
            "voice",
            vec![GenerationTask::TextToSpeech],
            FrameGrid::default(),
        );
        spec.components = vec![
            ModelComponent {
                slot: ComponentSlot::Checkpoint,
                source: ComponentSource::LocalFile {
                    path: backbone.to_string(),
                },
                size_bytes: 0,
            },
            ModelComponent {
                slot: ComponentSlot::Mmproj,
                source: ComponentSource::LocalFile {
                    path: mmproj.to_string(),
                },
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
        let (Ok(binary), Ok(checkpoint)) =
            (std::env::var("SD_SERVER"), std::env::var("SD_CHECKPOINT"))
        else {
            panic!("set SD_SERVER and SD_CHECKPOINT");
        };
        let mut spec = model(
            "live",
            vec![GenerationTask::TextToImage],
            FrameGrid::default(),
        );
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
                    JobProgress::Completed(mut media) => {
                        assert_eq!(media.len(), 1, "one image was asked for");
                        break media.remove(0);
                    }
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
            let dimension =
                |at: usize| u32::from_be_bytes(media.bytes[at..at + 4].try_into().unwrap());
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

    /// The shape `/sdcpp/v1/capabilities` really answers with, trimmed to the
    /// parts the app reads: samplers and schedulers are bare strings, upscalers
    /// are objects, and the two are not interchangeable.
    #[test]
    fn capabilities_are_read_off_the_engines_own_lists() {
        let body = json!({
            "current_mode": "img_gen",
            "samplers": ["euler", "euler_a", "brand_new_sampler"],
            "schedulers": ["discrete", "normal", "karras"],
            "upscalers": [{"name": "None"}, {"name": "Lanczos"}, {"name": "RealESRGAN_x4plus"}],
            "loras": [{"name": "detail", "path": "/loras/detail.safetensors"}],
            "features": {"mask_image": true, "control_image": false, "cancel_generating": false},
        });
        let capabilities = decode_capabilities(&body);
        assert_eq!(
            capabilities.samplers,
            ["euler", "euler_a", "brand_new_sampler"]
        );
        assert_eq!(capabilities.schedulers, ["discrete", "normal", "karras"]);
        // A model dropped in --hires-upscalers-dir arrives beside the built-ins.
        assert_eq!(
            capabilities.upscalers,
            ["None", "Lanczos", "RealESRGAN_x4plus"]
        );
        assert_eq!(capabilities.features.get("mask_image"), Some(&true));
        assert_eq!(capabilities.features.get("control_image"), Some(&false));
        assert_eq!(capabilities.features.get("ip_adapter_image"), None);
    }

    /// An engine that is up but still loading has an address and must not be
    /// asked anything at it: the read would sit on one of its worker threads
    /// for the whole load, which is the drain `ensure_ready` goes out of its way
    /// to avoid. Only the run path, which waits, gets the address before then.
    #[test]
    fn an_engine_that_has_not_answered_yet_is_not_offered_to_readers() {
        let engine = GenerationEngineState {
            inner: Mutex::new(EngineProcess {
                port: Some(51_234),
                ..EngineProcess::default()
            }),
        };
        assert_eq!(engine.base_url().as_deref(), Some("http://127.0.0.1:51234"));
        assert!(engine.ready_base_url().is_none());

        engine.inner.lock().unwrap().ready = true;
        assert_eq!(
            engine.ready_base_url().as_deref(),
            Some("http://127.0.0.1:51234")
        );

        // Stopping takes the address back from both.
        engine.stop().unwrap();
        assert!(engine.base_url().is_none());
        assert!(engine.ready_base_url().is_none());
    }

    /// An engine too old to report any of this must read as "said nothing",
    /// not as "supports nothing" — the frontend falls back to its own lists on
    /// an empty answer, and cannot tell the two apart itself.
    #[test]
    fn missing_capability_fields_decode_to_empty_rather_than_failing() {
        let capabilities = decode_capabilities(&json!({"model": {"path": "/m.safetensors"}}));
        assert_eq!(capabilities, EngineCapabilities::default());
    }
}
