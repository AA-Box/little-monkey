//! Verified, user-initiated Wan 2.2 TI2V-5B native model preset.
//! No ComfyUI installation or FreeVideo node is involved; these are
//! individual Hugging Face weights passed to the managed sd-server.
//!
//! The 5B diffusion and video VAE come from the Comfy-Org repackaged
//! Apache-2.0 release; the UMT5 text encoder is the widely used city96 GGUF.
//! Model bytes are NOT distributed in the Little Monkey binary.
use crate::generation::{
    ComponentSlot, FrameGrid, GenerationDefaults, GenerationEngineKind,
    GenerationModelSpec, GenerationTask, LicenseGate, ModelComponent,
    ModelSource, VideoMemoryProfile,
};

pub const WAN22_NATIVE_ID: &str = "wan22-ti2v-5b-native";
const WAN_REPO: &str = "Comfy-Org/Wan_2.2_ComfyUI_Repackaged";
const UMT5_REPO: &str = "city96/umt5-xxl-encoder-gguf";

/// Hashes from the public Hugging Face Xet blob metadata. Checking these
/// against actual downloaded bytes detects truncation and supply-chain
/// replacement before publishing a weight to the resident model directory.
pub fn expected_sha256(repo: &str, file: &str) -> Option<&'static str> {
    match (repo, file) {
        (WAN_REPO, "split_files/diffusion_models/wan2.2_ti2v_5B_fp16.safetensors") =>
            Some("456f901338bd9eadbded3828b819109a9b68e8a525ca5cf8d0049a69fcfeca1e"),
        (WAN_REPO, "split_files/vae/wan2.2_vae.safetensors") =>
            Some("e40321bd36b9709991dae2530eb4ac303dd168276980d3e9bc4b6e2b75fed156"),
        (UMT5_REPO, "umt5-xxl-encoder-Q8_0.gguf") =>
            Some("2521d4de0bf9e1cc6549866463ceae85e4ec3239bc6063f7488810be39033bbc"),
        _ => None,
    }
}

pub fn native_wan22_ti2v_5b() -> GenerationModelSpec {
    GenerationModelSpec {
        id: WAN22_NATIVE_ID.into(),
        name: "Wan 2.2 TI2V-5B (native, local)".into(),
        family: "Wan 2.2".into(),
        tasks: vec![GenerationTask::TextToVideo, GenerationTask::ImageToVideo],
        components: vec![
            ModelComponent::huggingface(
                ComponentSlot::DiffusionModel, WAN_REPO,
                "split_files/diffusion_models/wan2.2_ti2v_5B_fp16.safetensors",
                10_000_000_000,
            ),
            ModelComponent::huggingface(
                ComponentSlot::Vae, WAN_REPO,
                "split_files/vae/wan2.2_vae.safetensors",
                1_410_000_000,
            ),
            ModelComponent::huggingface(
                ComponentSlot::T5xxl, UMT5_REPO,
                "umt5-xxl-encoder-Q8_0.gguf",
                6_040_000_000,
            ),
        ],
        source: ModelSource::Components,
        // Conservative short-clip starting point: memory consumption rises
        // steeply with both spatial resolution and temporal frame count.
        defaults: GenerationDefaults {
            width: 480,
            height: 832,
            steps: 20,
            cfg_scale: 6.0,
            sample_method: "euler".into(),
            flow_shift: Some(3.0),
            fps: 16,
            video_frames: 33,
            frame_grid: FrameGrid::DownTo4nPlus1,
        },
        // A guidance estimate, not a guaranteed RAM requirement; offload can
        // trade compute speed and disk IO to make individual systems viable.
        min_ram_bytes: 24 * 1024 * 1024 * 1024,
        license: LicenseGate {
            id: "wan2.2-apache-2.0".into(),
            name: "Wan 2.2 Apache-2.0".into(),
            url: "https://github.com/Wan-Video/Wan2.2/blob/main/LICENSE".into(),
            excluded_territories: vec![],
            acceptance_required: false,
        },
        // Do not repeat automatic memory flags here; the backend profile
        // owns them and changes take effect on warm engine restart.
        extra_launch_args: vec![],
        engine: GenerationEngineKind::StableDiffusionCpp,
        quantization_bits: None,
        reads_ref_images: false,
        video_memory_profile: VideoMemoryProfile::AdaptiveVram,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_has_valid_and_complete_native_input_slots() {
        let spec = native_wan22_ti2v_5b();
        crate::generation::validate_model_spec(&spec).unwrap();
        assert_eq!(spec.id, WAN22_NATIVE_ID);
        assert_eq!(spec.components.len(), 3);
        assert!(spec.supports(GenerationTask::TextToVideo));
        assert!(spec.supports(GenerationTask::ImageToVideo));
        assert!(!spec.license.acceptance_required);
        assert!(spec.license.excluded_territories.is_empty());
        assert!(spec.extra_launch_args.is_empty());
        assert_eq!(spec.video_memory_profile, VideoMemoryProfile::AdaptiveVram);
        let args = crate::generation::launch_args(
            &spec, std::path::Path::new("/models"), 5001
        );
        for flag in ["--diffusion-model", "--vae", "--t5xxl", "--max-vram",
            "--offload-to-cpu", "--vae-tiling", "--temporal-tiling"] {
            assert!(args.iter().any(|arg| arg == flag), "Missing {flag}");
        }
        assert!(!args.iter().any(|arg| arg.contains("comfyui")));
    }
}
