# Native Wan 2.2 TI2V-5B — one-click install and memory profiles

This is the global-license local video path for Studio. It uses the
**app-managed pinned stable-diffusion.cpp `sd-server`** and ordinary Studio
`text_to_video` / `image_to_video` generation; it does not spawn a
FreeVideo or ComfyUI engine.

## In Studio

1. Open Settings → Runtime Hub → Studio Models.
2. Select **Install Wan 2.2** and confirm the ~17.5 GB transfer.
   The button registers the complete model and downloads its missing weights
   through the existing cancellable Hugging Face downloader. Per-file byte
   progress and cancellation are visible; a failed download leaves an
   incomplete model, never a completed-model claim.
3. All three downloaded components are verified against their published
   SHA-256 checksums before an atomic rename into the app-owned model root.
   Already-present curated components are also checked before reuse.
4. Open Studio → Video, select the installed model, enter a prompt, and
   generate. Image-to-video accepts a source image; final clips are published
   through the regular Studio gallery.

The model's Apache-2.0 license imposes no MiniMax H3 territorial restrictions.
Other user-supplied components must still satisfy their own licenses.

## Actual model sources

- `Comfy-Org/Wan_2.2_ComfyUI_Repackaged`:
  `split_files/diffusion_models/wan2.2_ti2v_5B_fp16.safetensors`,
  `split_files/vae/wan2.2_vae.safetensors`
- `city96/umt5-xxl-encoder-gguf`:
  `umt5-xxl-encoder-Q8_0.gguf`

The backend owns this manifest; callers do not pass a file path or
unverified remote weight URL to the installation command. The standard
`generation_register_wan22` command is idempotent and does not overwrite an
already-edited entry with the same ID.

## Memory profiles

Model editor → **Native video memory profile** controls actual sd-server
startup argv for video tasks:

- `default`: unchanged legacy launch flags (backwards compatibility)
- `maximum_speed`: no extra conservative memory flags
- `adaptive_vram`: `--offload-to-cpu --max-vram -1 --vae-tiling --temporal-tiling`
- `minimum_ram`: `--params-backend diffusion=disk,te=cpu,vae=cpu --max-vram -1 --vae-tiling --temporal-tiling`

Wan starts in adaptive mode. This app's pinned `sd-server`
`master-987-a77d362` already implements adaptive graph-cut segmentation,
weight eviction and asynchronous segment prefetch. Its released documentation
states that **no `--stream-layers` opt-in exists or is required** on this
release. Adding an unsupported flag would make the engine fail to start.

Changes to the memory profile alter `launch_signature` and thus cause a
clean engine re-launch rather than silently reusing an incompatible model.
The model chooses placement within the configured maximum based on actual
GPU memory, but a low-memory machine may still fail or render very slowly.
The code does not reproduce FreeVideo's Video DeltaNet-specific kernels or
guarantee FreeVideo throughput.

## Acceptance

Automated unit tests verify a fully valid native Wan model spec, all required
model slot launch flags, no ComfyUI dispatch, the real memory-profile argv
of the pinned engine, completeness of the SHA-256 manifest, and tampering
detection on file bytes. The existing native Studio generator handles
progress, cancellation, media publication and I2V/T2V execution.

A **physical** acceptance test still requires a machine with enough RAM/disk,
a working supported sd-server GPU/backend and the full ~17.5 GB downloaded
weights. CI does not download these weights and cannot claim that it completed
a GPU video render. A complete acceptance run should generate and inspect a
nonempty WebM in both T2V and I2V mode and should exercise cancellation,
restart/retry and the disk-backed memory setting.
