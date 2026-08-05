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
//! Nothing in this file is specific to one model. Adding Flux, LTX or
//! HunyuanVideo is a [`curated_models`] entry — a list of component slots and a
//! table of defaults — not new code. The two video entries prove the shape:
//! Wan 2.2 wants `--t5xxl` and a plain VAE, MiniMax H3 wants `--llm` plus a
//! second `--audio-vae`, and both reach the same argv builder.
//!
//! This module is Tauri-free so the desktop commands and the CLI can share it.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Generation's own `sd-server` instance, next to `llama.rs`'s chat (8090) and
/// embeddings (8091) instances.
pub const GENERATION_PORT: u16 = 8092;

const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_STEPS: u32 = 200;
/// 15 s at 24 fps, the longest clip any currently supported model produces.
const MAX_VIDEO_FRAMES: u32 = 361;
const MAX_FPS: u32 = 60;
const MAX_INIT_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// A 15 s 2K clip with audio stays far under this; it exists so a runaway
/// server response can never be buffered without bound.
const MAX_MEDIA_BYTES: usize = 256 * 1024 * 1024;
/// Weights are tens of gigabytes and are read lazily from disk on first use,
/// so first-token latency after launch is dominated by IO, not compute.
const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// One `sd-server` command-line weight slot. The mapping to a flag is the
/// entire model-specific surface: a new architecture picks different slots
/// rather than needing different code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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
        }
    }
}

/// One downloadable weight file filling one slot.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelComponent {
    pub slot: ComponentSlot,
    /// Hugging Face repo id, resolved through the existing model downloader.
    pub repo: String,
    /// Path within the repo. The basename is what lands on disk.
    pub file: String,
    pub size_bytes: u64,
}

impl ModelComponent {
    /// The flat on-disk name. Components from different repos can collide only
    /// if they share a basename, which [`curated_models_have_unique_component_files`]
    /// rejects at test time.
    pub fn file_name(&self) -> &str {
        self.file.rsplit('/').next().unwrap_or(&self.file)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationTask {
    TextToImage,
    ImageToImage,
    TextToVideo,
    ImageToVideo,
}

impl GenerationTask {
    pub fn is_video(self) -> bool {
        matches!(self, Self::TextToVideo | Self::ImageToVideo)
    }

    pub fn needs_init_image(self) -> bool {
        matches!(self, Self::ImageToImage | Self::ImageToVideo)
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
#[derive(Clone, Debug, Serialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameGrid {
    /// Largest `4n + 1` at or below the request — the core video path's
    /// general normalization, used by Wan.
    DownTo4nPlus1,
    /// Smallest `17k + 5` at or above the request, minimum 5 — MiniMax H3.
    UpTo17kPlus5,
}

/// Per-model starting point for the request fields a user does not set.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationDefaults {
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f64,
    pub sample_method: &'static str,
    /// `None` leaves the backend's own choice in place.
    pub flow_shift: Option<f64>,
    pub fps: u32,
    pub video_frames: u32,
    pub frame_grid: FrameGrid,
}

#[derive(Clone, Debug, Serialize)]
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
            .map(|component| model_root.join(&self.id).join(component.file_name()))
            .collect()
    }

    /// Components still missing from disk, so the UI can show what a download
    /// would actually fetch rather than re-fetching a partially present set.
    pub fn missing_components(&self, model_root: &Path) -> Vec<&ModelComponent> {
        self.components
            .iter()
            .filter(|component| {
                !model_root
                    .join(&self.id)
                    .join(component.file_name())
                    .is_file()
            })
            .collect()
    }
}

fn permissive_license(id: &str, name: &str, url: &str) -> LicenseGate {
    LicenseGate {
        id: id.to_string(),
        name: name.to_string(),
        url: url.to_string(),
        excluded_territories: Vec::new(),
        acceptance_required: false,
    }
}

/// The shipped model set. Every entry's repo, file path and byte size was read
/// off the Hugging Face API, and every flag combination comes from
/// stable-diffusion.cpp's own documented invocations.
pub fn curated_models() -> Vec<GenerationModelSpec> {
    vec![
        GenerationModelSpec {
            id: "sdxl-base-1.0".to_string(),
            name: "Stable Diffusion XL 1.0".to_string(),
            family: "SDXL".to_string(),
            tasks: vec![GenerationTask::TextToImage, GenerationTask::ImageToImage],
            components: vec![ModelComponent {
                slot: ComponentSlot::Checkpoint,
                repo: "stabilityai/stable-diffusion-xl-base-1.0".to_string(),
                file: "sd_xl_base_1.0.safetensors".to_string(),
                size_bytes: 6_938_040_682,
            }],
            defaults: GenerationDefaults {
                width: 1024,
                height: 1024,
                steps: 20,
                cfg_scale: 7.0,
                sample_method: "euler_a",
                flow_shift: None,
                fps: 1,
                video_frames: 1,
                frame_grid: FrameGrid::DownTo4nPlus1,
            },
            min_ram_bytes: 16 * 1024 * 1024 * 1024,
            license: permissive_license(
                "openrail-plus-plus-m",
                "CreativeML Open RAIL++-M",
                "https://huggingface.co/stabilityai/stable-diffusion-xl-base-1.0/blob/main/LICENSE.md",
            ),
            extra_launch_args: Vec::new(),
        },
        GenerationModelSpec {
            id: "wan2.2-ti2v-5b".to_string(),
            name: "Wan 2.2 TI2V 5B".to_string(),
            family: "Wan".to_string(),
            tasks: vec![GenerationTask::TextToVideo, GenerationTask::ImageToVideo],
            components: vec![
                ModelComponent {
                    slot: ComponentSlot::DiffusionModel,
                    repo: "Comfy-Org/Wan_2.2_ComfyUI_Repackaged".to_string(),
                    file: "split_files/diffusion_models/wan2.2_ti2v_5B_fp16.safetensors"
                        .to_string(),
                    size_bytes: 10_003_000_000,
                },
                ModelComponent {
                    slot: ComponentSlot::Vae,
                    repo: "Comfy-Org/Wan_2.2_ComfyUI_Repackaged".to_string(),
                    file: "split_files/vae/wan2.2_vae.safetensors".to_string(),
                    size_bytes: 1_411_000_000,
                },
                ModelComponent {
                    slot: ComponentSlot::T5xxl,
                    repo: "city96/umt5-xxl-encoder-gguf".to_string(),
                    file: "umt5-xxl-encoder-Q8_0.gguf".to_string(),
                    size_bytes: 6_043_000_000,
                },
            ],
            defaults: GenerationDefaults {
                width: 704,
                height: 1280,
                steps: 20,
                cfg_scale: 6.0,
                sample_method: "euler",
                flow_shift: Some(3.0),
                fps: 24,
                video_frames: 33,
                frame_grid: FrameGrid::DownTo4nPlus1,
            },
            min_ram_bytes: 16 * 1024 * 1024 * 1024,
            license: permissive_license(
                "apache-2.0",
                "Apache 2.0",
                "https://huggingface.co/Wan-AI/Wan2.2-TI2V-5B/blob/main/LICENSE.txt",
            ),
            extra_launch_args: vec!["--diffusion-fa".to_string(), "--offload-to-cpu".to_string()],
        },
        GenerationModelSpec {
            id: "minimax-h3-ref2va".to_string(),
            name: "MiniMax H3 Ref2VA (video + audio)".to_string(),
            family: "MiniMax H3".to_string(),
            // The reference-conditioning this checkpoint is named for reaches
            // the engine only through its CLI and C API: `sd-server`'s vid_gen
            // route advertises `init_image` and `end_image` but no
            // `ref_images`, so over HTTP it serves text- and image-driven
            // video like any other H3 weight.
            tasks: vec![GenerationTask::TextToVideo, GenerationTask::ImageToVideo],
            components: vec![
                ModelComponent {
                    slot: ComponentSlot::DiffusionModel,
                    repo: "leejet/MiniMax-H3-GGUF".to_string(),
                    file: "minimax_h3_ref2va_pruned-Q4_K_M.gguf".to_string(),
                    size_bytes: 11_420_663_904,
                },
                ModelComponent {
                    slot: ComponentSlot::Llm,
                    repo: "leejet/MiniMax-H3-GGUF".to_string(),
                    file: "qwen3vl_32b_minimax_h3-Q2_K_M.gguf".to_string(),
                    size_bytes: 13_102_161_024,
                },
                ModelComponent {
                    slot: ComponentSlot::Vae,
                    repo: "Comfy-Org/MiniMax-H3".to_string(),
                    file: "vae/minimax_h3_video_vae_fp16.safetensors".to_string(),
                    size_bytes: 5_207_808_496,
                },
                ModelComponent {
                    slot: ComponentSlot::AudioVae,
                    repo: "Comfy-Org/MiniMax-H3".to_string(),
                    file: "vae/minimax_h3_audio_vae_fp32.safetensors".to_string(),
                    size_bytes: 605_254_808,
                },
            ],
            defaults: GenerationDefaults {
                width: 864,
                height: 480,
                steps: 8,
                cfg_scale: 1.0,
                sample_method: "euler",
                flow_shift: None,
                fps: 24,
                video_frames: 39,
                frame_grid: FrameGrid::UpTo17kPlus5,
            },
            // Measured: 30.5 GB resident with --offload-to-cpu on a 52 GB M4 Pro.
            min_ram_bytes: 36 * 1024 * 1024 * 1024,
            license: LicenseGate {
                id: "minimax-h3-community".to_string(),
                name: "MiniMax H3 Community License".to_string(),
                url: "https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/LICENSE".to_string(),
                excluded_territories: vec![
                    "European Union".to_string(),
                    "United Kingdom".to_string(),
                    "Republic of Korea".to_string(),
                    "United States of America".to_string(),
                ],
                acceptance_required: true,
            },
            extra_launch_args: vec![
                "--diffusion-fa".to_string(),
                "--offload-to-cpu".to_string(),
                "--rng".to_string(),
                "cpu".to_string(),
            ],
        },
        GenerationModelSpec {
            id: "minimax-h3-fl2va".to_string(),
            name: "MiniMax H3 (video + audio)".to_string(),
            family: "MiniMax H3".to_string(),
            tasks: vec![GenerationTask::TextToVideo, GenerationTask::ImageToVideo],
            components: vec![
                ModelComponent {
                    slot: ComponentSlot::DiffusionModel,
                    repo: "leejet/MiniMax-H3-GGUF".to_string(),
                    file: "minimax_h3_fl2va_pruned-Q4_K_M.gguf".to_string(),
                    size_bytes: 11_420_000_000,
                },
                ModelComponent {
                    slot: ComponentSlot::Llm,
                    repo: "leejet/MiniMax-H3-GGUF".to_string(),
                    file: "qwen3vl_32b_minimax_h3-Q2_K_M.gguf".to_string(),
                    size_bytes: 13_100_000_000,
                },
                ModelComponent {
                    slot: ComponentSlot::Vae,
                    repo: "Comfy-Org/MiniMax-H3".to_string(),
                    file: "vae/minimax_h3_video_vae_fp16.safetensors".to_string(),
                    size_bytes: 5_210_000_000,
                },
                ModelComponent {
                    slot: ComponentSlot::AudioVae,
                    repo: "Comfy-Org/MiniMax-H3".to_string(),
                    file: "vae/minimax_h3_audio_vae_fp32.safetensors".to_string(),
                    size_bytes: 605_000_000,
                },
            ],
            defaults: GenerationDefaults {
                width: 1344,
                height: 768,
                steps: 25,
                cfg_scale: 1.0,
                sample_method: "euler",
                flow_shift: None,
                // The core video path forces 24 fps for this family, and
                // aligns length upward onto a 17k+5 grid: 56 frames is the
                // third rung, about 2.3 s.
                fps: 24,
                video_frames: 56,
                frame_grid: FrameGrid::UpTo17kPlus5,
            },
            min_ram_bytes: 48 * 1024 * 1024 * 1024,
            license: LicenseGate {
                id: "minimax-h3-community".to_string(),
                name: "MiniMax H3 Community License".to_string(),
                url: "https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/LICENSE".to_string(),
                excluded_territories: vec![
                    "European Union".to_string(),
                    "United Kingdom".to_string(),
                    "Republic of Korea".to_string(),
                    "United States of America".to_string(),
                ],
                acceptance_required: true,
            },
            extra_launch_args: vec!["--offload-to-cpu".to_string(), "--rng".to_string(), "cpu".to_string()],
        },
    ]
}

pub fn find_model(id: &str) -> Option<GenerationModelSpec> {
    curated_models().into_iter().find(|model| model.id == id)
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
        args.push(component.slot.flag().to_string());
        args.push(
            model_root
                .join(&spec.id)
                .join(component.file_name())
                .to_string_lossy()
                .to_string(),
        );
    }
    args.extend(spec.extra_launch_args.iter().cloned());
    args
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
    /// Negative asks the backend for a random seed.
    pub seed: i64,
    #[serde(default)]
    pub video_frames: u32,
    #[serde(default)]
    pub fps: u32,
    /// Base64 PNG/JPEG starting frame, required by the image-driven tasks.
    #[serde(default)]
    pub init_image_base64: Option<String>,
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
    let mut sample_params = json!({
        "sample_method": spec.defaults.sample_method,
        "sample_steps": request.steps,
        "guidance": { "txt_cfg": request.cfg_scale },
    });
    if let Some(flow_shift) = spec.defaults.flow_shift {
        sample_params["flow_shift"] = json!(flow_shift);
    }

    let mut body = json!({
        "prompt": request.prompt,
        "negative_prompt": request.negative_prompt,
        "width": request.width,
        "height": request.height,
        "seed": request.seed,
        "sample_params": sample_params,
    });
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
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
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

/// The one running `sd-server`, plus which model it was launched against.
#[derive(Default)]
pub struct GenerationEngineState {
    inner: Mutex<EngineProcess>,
}

#[derive(Default)]
struct EngineProcess {
    child: Option<Child>,
    model_id: Option<String>,
    /// Tail of the engine's stderr, drained by a reader thread so the pipe can
    /// never fill and block the child.
    stderr_tail: Option<Arc<Mutex<String>>>,
}

impl GenerationEngineState {
    pub fn loaded_model(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|state| state.model_id.clone())
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
            .and_then(|tail| tail.lock().ok().map(|value| value.trim().to_string()))
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
        port: u16,
    ) -> Result<String, String> {
        let base_url = format!("http://127.0.0.1:{port}");
        if self.loaded_model().as_deref() == Some(spec.id.as_str())
            && self.child_exited()?.is_none()
        {
            return Ok(base_url);
        }
        self.stop()?;

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
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Failed to spawn the generation engine: {error}"))?;
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        if let Some(stream) = child.stderr.take() {
            let sink = Arc::clone(&stderr_tail);
            // A piped stream nobody reads fills its buffer and blocks the
            // child, so this thread drains it for the process's whole life and
            // keeps only the tail — a failing loader prints its diagnosis last.
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let Ok(mut buffer) = sink.lock() else { return };
                    buffer.push_str(&line);
                    buffer.push('\n');
                    if buffer.len() > MAX_STDERR_TAIL {
                        let (capped, _) =
                            crate::output_cap::cap_tail(std::mem::take(&mut buffer), MAX_STDERR_TAIL);
                        *buffer = capped;
                    }
                }
            });
        }
        {
            let mut state = self.inner.lock().map_err(|error| error.to_string())?;
            state.child = Some(child);
            state.model_id = Some(spec.id.clone());
            state.stderr_tail = Some(stderr_tail);
        }

        let client = reqwest::Client::new();
        let capabilities = format!("{base_url}/sdcpp/v1/capabilities");
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
                // response, exactly as `llama.rs` does.
                if response.status().is_success() {
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
            body.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("no detail")
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

    #[test]
    fn every_curated_model_declares_a_usable_component_set() {
        for model in curated_models() {
            assert!(!model.tasks.is_empty(), "{}", model.id);
            assert!(!model.components.is_empty(), "{}", model.id);
            // Exactly one weight carries the denoiser, under whichever slot the
            // family uses. A spec with neither cannot launch.
            let denoisers = model
                .components
                .iter()
                .filter(|component| {
                    matches!(
                        component.slot,
                        ComponentSlot::Checkpoint | ComponentSlot::DiffusionModel
                    )
                })
                .count();
            assert_eq!(denoisers, 1, "{}", model.id);
            assert!(model.total_bytes() > 0, "{}", model.id);
            assert!(model.defaults.steps > 0, "{}", model.id);
            if model.tasks.iter().any(|task| task.is_video()) {
                assert_eq!(
                    normalize_video_frames(
                        model.defaults.frame_grid,
                        model.defaults.video_frames
                    ),
                    model.defaults.video_frames,
                    "{} default frame count is off its own grid",
                    model.id
                );
            }
        }
    }

    /// Components land in one flat per-model directory, so two files in the
    /// same spec sharing a basename would silently overwrite each other.
    #[test]
    fn curated_models_have_unique_component_files() {
        for model in curated_models() {
            let mut names: Vec<_> = model
                .components
                .iter()
                .map(|component| component.file_name().to_string())
                .collect();
            names.sort();
            let count = names.len();
            names.dedup();
            assert_eq!(names.len(), count, "{}", model.id);
        }
    }

    /// A territory-restricted model must gate on acceptance; a permissive one
    /// must not nag. Getting this backwards is a licensing problem, not a UI one.
    #[test]
    fn restricted_models_require_license_acceptance() {
        for model in curated_models() {
            assert_eq!(
                model.license.acceptance_required,
                !model.license.excluded_territories.is_empty(),
                "{}",
                model.id
            );
        }
        let h3 = find_model("minimax-h3-fl2va").expect("h3");
        assert!(h3.license.acceptance_required);
        assert!(h3
            .license
            .excluded_territories
            .contains(&"United States of America".to_string()));
    }

    #[test]
    fn launch_args_map_every_slot_to_its_flag() {
        let h3 = find_model("minimax-h3-fl2va").expect("h3");
        let args = launch_args(&h3, Path::new("/models"), 8092);
        for (flag, file) in [
            ("--diffusion-model", "minimax_h3_fl2va_pruned-Q4_K_M.gguf"),
            ("--llm", "qwen3vl_32b_minimax_h3-Q2_K_M.gguf"),
            ("--vae", "minimax_h3_video_vae_fp16.safetensors"),
            ("--audio-vae", "minimax_h3_audio_vae_fp32.safetensors"),
        ] {
            let at = args.iter().position(|arg| arg == flag).expect(flag);
            assert_eq!(
                args[at + 1],
                format!("/models/minimax-h3-fl2va/{file}"),
                "{flag}"
            );
        }
        assert!(args.windows(2).any(|pair| pair == ["--listen-port", "8092"]));
        assert!(args.contains(&"--offload-to-cpu".to_string()));

        // A single-checkpoint family reaches the same builder with one slot.
        let sdxl = find_model("sdxl-base-1.0").expect("sdxl");
        let sdxl_args = launch_args(&sdxl, Path::new("/models"), 8092);
        assert!(sdxl_args.contains(&"--model".to_string()));
        assert!(!sdxl_args.contains(&"--diffusion-model".to_string()));
    }

    /// Wan rounds down onto 4n+1; MiniMax H3 rounds *up* onto 17k+5. Using one
    /// rule for both would misreport the duration of every H3 clip.
    #[test]
    fn frame_counts_snap_onto_each_familys_own_grid() {
        use FrameGrid::{DownTo4nPlus1, UpTo17kPlus5};
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 33), 33);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 34), 33);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 32), 29);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 5), 5);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 4), 1);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, 0), 1);
        assert_eq!(normalize_video_frames(DownTo4nPlus1, u32::MAX), MAX_VIDEO_FRAMES);

        assert_eq!(normalize_video_frames(UpTo17kPlus5, 56), 56);
        assert_eq!(normalize_video_frames(UpTo17kPlus5, 45), 56);
        assert_eq!(normalize_video_frames(UpTo17kPlus5, 39), 39);
        assert_eq!(normalize_video_frames(UpTo17kPlus5, 1), 5);
        assert_eq!(normalize_video_frames(UpTo17kPlus5, 0), 5);
        // Rounding up must not push past the cap.
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

    fn video_request(task: GenerationTask) -> GenerationRequest {
        GenerationRequest {
            model_id: "wan2.2-ti2v-5b".to_string(),
            task,
            prompt: "a lovely cat".to_string(),
            negative_prompt: String::new(),
            width: 704,
            height: 1280,
            steps: 20,
            cfg_scale: 6.0,
            seed: -1,
            video_frames: 34,
            fps: 24,
            init_image_base64: None,
        }
    }

    #[test]
    fn validation_normalizes_and_rejects_out_of_bounds_requests() {
        let wan = find_model("wan2.2-ti2v-5b").expect("wan");
        let normalized =
            validate_request(&wan, &video_request(GenerationTask::TextToVideo)).unwrap();
        assert_eq!(normalized.video_frames, 33);
        // The same 34 becomes 39 on H3's upward 17k+5 grid, not 33.
        let h3 = find_model("minimax-h3-fl2va").expect("h3");
        let mut on_h3 = video_request(GenerationTask::TextToVideo);
        on_h3.model_id = h3.id.clone();
        assert_eq!(validate_request(&h3, &on_h3).unwrap().video_frames, 39);
        assert_eq!(normalized.fps, 24);

        // Image-driven tasks cannot silently fall back to text-only.
        assert!(validate_request(&wan, &video_request(GenerationTask::ImageToVideo)).is_err());

        // A model that does not do video must refuse a video task outright.
        let sdxl = find_model("sdxl-base-1.0").expect("sdxl");
        assert!(validate_request(&sdxl, &video_request(GenerationTask::TextToVideo)).is_err());

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
        let wan = find_model("wan2.2-ti2v-5b").expect("wan");
        let body = request_body(&wan, &video_request(GenerationTask::TextToVideo));
        assert_eq!(body["video_frames"], json!(34));
        assert_eq!(body["output_format"], json!("webm"));
        assert_eq!(body["sample_params"]["flow_shift"], json!(3.0));
        assert_eq!(body["sample_params"]["guidance"]["txt_cfg"], json!(6.0));
        assert!(body.get("init_image").is_none());

        let sdxl = find_model("sdxl-base-1.0").expect("sdxl");
        let mut image = video_request(GenerationTask::TextToImage);
        image.model_id = sdxl.id.clone();
        let body = request_body(&sdxl, &image);
        assert_eq!(body["output_format"], json!("png"));
        assert!(body.get("video_frames").is_none());
        // SDXL declares no flow shift, so the field must be absent rather than
        // pinned to a value the backend would otherwise choose itself.
        assert!(body["sample_params"].get("flow_shift").is_none());
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

    #[test]
    fn video_and_image_tasks_reach_different_endpoints() {
        assert_eq!(
            GenerationTask::TextToVideo.endpoint(),
            "/sdcpp/v1/vid_gen"
        );
        assert_eq!(
            GenerationTask::ImageToVideo.endpoint(),
            "/sdcpp/v1/vid_gen"
        );
        assert_eq!(GenerationTask::TextToImage.endpoint(), "/sdcpp/v1/img_gen");
        assert!(GenerationTask::ImageToVideo.needs_init_image());
        assert!(!GenerationTask::TextToVideo.needs_init_image());
    }
}
