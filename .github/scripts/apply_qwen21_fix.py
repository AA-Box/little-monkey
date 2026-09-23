from pathlib import Path

path = Path("src-tauri/src/generation.rs")
text = path.read_text()


def replace_once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one match, found {count}: {old[:100]!r}")
    text = text.replace(old, new, 1)


replace_once(
    "const MAX_STDERR_TAIL: usize = 4_000;",
    "const MAX_STDERR_TAIL: usize = 32 * 1024;",
)

replace_once(
    '        .filter(|line| line.contains("[ERROR]") || line.starts_with("error"))\n',
    '''        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            line.contains("[ERROR]")
                || lower.starts_with("error")
                || lower.contains("libc++abi")
                || lower.contains("uncaught exception")
                || lower.contains("terminate called")
                || lower.contains("what():")
                || lower.contains("std::runtime_error")
                || lower.contains("std::bad_alloc")
                || lower.contains("ggml_assert")
                || lower.contains("assertion failed")
                || lower.starts_with("fatal")
        })
''',
)

replace_once(
    '''    (
        "wrong shape in model metadata",
        "That file's tensors are laid out for a different loader. A ComfyUI-GGUF quantization will not load here even though the extension matches — use one built for stable-diffusion.cpp.",
    ),
];
''',
    '''    (
        "wrong shape in model metadata",
        "That file's tensors are laid out for a different loader. A ComfyUI-GGUF quantization will not load here even though the extension matches — use one built for stable-diffusion.cpp.",
    ),
    (
        "INT8 tensorwise/convrot is not supported by this ggml build",
        "This text encoder uses ComfyUI INT8 ConvRot weights, but this stable-diffusion.cpp runtime was built without the patched GGML extensions required to execute them. Use the BF16 Qwen3-VL encoder, a GGUF Qwen3-VL encoder, or a managed runtime built with stable-diffusion.cpp's patched GGML.",
    ),
];
''',
)

replace_once(
    '''/// Builds the `sd-server` command line for a model. Every weight path is
/// absolute and app-owned; nothing is read from a user shell or PATH.
pub fn launch_args(spec: &GenerationModelSpec, model_root: &Path, port: u16) -> Vec<String> {
''',
    '''/// Qwen Image 2.1's Qwen3-VL INT8 ConvRot encoder is designed to live in RAM.
///
/// stable-diffusion.cpp's own Qwen Image 2.1 examples use `--offload-to-cpu`,
/// and its INT8 ConvRot documentation notes that Metal and other non-CUDA GPU
/// backends rely on CPU fallback for this path. Keeping the encoder parameters
/// on the GPU can therefore fail during runner construction before the server
/// ever becomes ready. The diffusion model remains on its normal runtime
/// backend; only parameter residency is changed.
fn qwen_image_2_1_int8_convrot_needs_cpu_offload(spec: &GenerationModelSpec) -> bool {
    let has_qwen_21_diffusion = spec.components.iter().any(|component| {
        if component.slot != ComponentSlot::DiffusionModel {
            return false;
        }
        component
            .file_name()
            .to_ascii_lowercase()
            .replace('_', "-")
            .contains("qwen-image-2.1")
    });
    let has_int8_convrot_llm = spec.components.iter().any(|component| {
        if component.slot != ComponentSlot::Llm {
            return false;
        }
        let name = component.file_name().to_ascii_lowercase().replace('_', "-");
        name.contains("qwen3vl") && name.contains("int8") && name.contains("convrot")
    });
    let residency_is_explicit = spec.extra_launch_args.iter().any(|argument| {
        let argument = argument.trim();
        argument == "--offload-to-cpu"
            || argument == "--params-backend"
            || argument.starts_with("--params-backend=")
    });
    has_qwen_21_diffusion && has_int8_convrot_llm && !residency_is_explicit
}

/// Builds the `sd-server` command line for a model. Every weight path is
/// absolute and app-owned; nothing is read from a user shell or PATH.
pub fn launch_args(spec: &GenerationModelSpec, model_root: &Path, port: u16) -> Vec<String> {
''',
)

replace_once(
    '''    args.extend(spec.extra_launch_args.iter().cloned());
    args
}
''',
    '''    if qwen_image_2_1_int8_convrot_needs_cpu_offload(spec) {
        args.push("--offload-to-cpu".to_string());
    }
    args.extend(spec.extra_launch_args.iter().cloned());
    args
}
''',
)

test_anchor = '''    /// A warm engine is reused on what it was launched with, not on the model
'''
tests = r'''    #[test]
    fn qwen_image_2_1_int8_convrot_defaults_to_cpu_offload() {
        let mut spec = model(
            "qwen21",
            vec![GenerationTask::TextToImage],
            FrameGrid::DownTo4nPlus1,
        );
        spec.family = "Qwen Image 2.1".to_string();
        spec.extra_launch_args.clear();
        spec.components = vec![
            ModelComponent::huggingface(
                ComponentSlot::DiffusionModel,
                "KasugaiSakura/Qwen-Image-2.1-Uncensored-Abenzerps-GGUF",
                "qwen-image-2.1-Q6_K.gguf",
                1,
            ),
            ModelComponent::huggingface(
                ComponentSlot::Llm,
                "Comfy-Org/Qwen-Image-2.1",
                "text_encoders/qwen3vl_8b_int8_convrot.safetensors",
                1,
            ),
            ModelComponent::huggingface(
                ComponentSlot::Vae,
                "Comfy-Org/Qwen-Image-2.1",
                "vae/qwen_image_2.1_vae_bf16.safetensors",
                1,
            ),
        ];

        let args = launch_args(&spec, Path::new("/models"), 8092);
        assert!(args.contains(&"--offload-to-cpu".to_string()));

        let mut bf16 = spec.clone();
        bf16.components[1] = ModelComponent::huggingface(
            ComponentSlot::Llm,
            "Comfy-Org/Qwen-Image-2.1",
            "text_encoders/qwen3vl_8b_bf16.safetensors",
            1,
        );
        assert!(!launch_args(&bf16, Path::new("/models"), 8092)
            .contains(&"--offload-to-cpu".to_string()));

        let mut explicit = spec.clone();
        explicit.extra_launch_args = vec![
            "--params-backend".to_string(),
            "*=metal".to_string(),
        ];
        assert!(!launch_args(&explicit, Path::new("/models"), 8092)
            .contains(&"--offload-to-cpu".to_string()));
    }

    #[test]
    fn native_abort_detail_keeps_the_exception_reason() {
        let tail = "libc++abi: terminating due to uncaught exception of type std::runtime_error: INT8 tensorwise/convrot is not supported by this ggml build\n0 libsystem_kernel.dylib ...\n19 libstable-diffusion.dylib StableDiffusionGGML::build_runners + 572\n20 libstable-diffusion.dylib StableDiffusionGGML::apply_model_update + 908";
        let detail = engine_failure_detail(tail);
        assert!(detail.contains("libc++abi"));
        assert!(detail.contains("INT8 tensorwise/convrot"));
        assert!(detail.contains("patched GGML"));
        assert!(!detail.contains("19 libstable-diffusion"));
    }

'''
if text.count(test_anchor) != 1:
    raise SystemExit("test insertion anchor not unique")
text = text.replace(test_anchor, tests + test_anchor, 1)

path.write_text(text)
