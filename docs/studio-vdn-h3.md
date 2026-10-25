# Standalone OpenVDN H3: second native Studio video engine

Little Monkey's Studio Video has two independent model engines:

- **Wan 2.2** runs through the app-managed native `sd-server` (PR #627).
- **VDN-H3** invokes a separately installed CUDA Python environment and a
  Little Monkey-owned Python worker that directly calls OpenVDN's genuine
  `ModularPipeline`/hybrid-attention checkpoint and audio/video decoders.
  It does **not** install or invoke FreeVideo or ComfyUI.

The VDN model checkpoint is derived from MiniMax H3. Model weights are
governed by the **MiniMax H3 Community License**, not Apache-2.0. The entire
launch/download path checks the jurisdiction-aware policy from PR #626.
Absent proper licensing, this engine is **unavailable** in the EU, US, UK,
South Korea. A compile-time trusted enterprise issuer may authorize an
explicitly licensed territory using independently signed receipts only after
actual written MiniMax rights have been obtained.

## Set up a real CUDA environment

The actual inference weights and Python+CUDA dependencies are too large
to bundle. Install the official model's supported environment from its
own repository, not FreeVideo:

1. Review the MiniMax model license and check your eligibility. Under EU/US/UK/KR
   use, obtain written permission **before** downloading or running weights.
2. Follow the OpenVDN upstream [Diffusers setup](https://github.com/OpenVDN/vdn-minimax-h3/blob/main/docs/diffusers.md),
   including its compatible Python 3.12, CUDA PyTorch, patched Diffusers,
   Accelerate, PyAV and (optional) torchao.
3. In Studio → Video → **Video DeltaNet / VDN-H3**, choose the Python executable
   **from that environment**, not your system Python.
4. Choose first and/or last PNG/JPEG keyframes if needed, prompt, supported
   clip length (5–15 seconds, 17n+5 frame grid), seed, 8 steps, TorchAO FP8
   and transformer streaming options.
5. Acknowledge that upstream modular components execute Python model code.
   Little Monkey will not silently trust unknown model code.
6. Press Generate; the worker downloads/loads authorized model weights from
   the fixed OpenVDN Hub repo, executes the hybrid attention decoder and audio
   muxer, produces an actual MP4 and adds it to the regular Studio Gallery.

The VDN modular pipeline uses Diffusers' real CUDA backend; it is **not**
the separate OpenVDN custom fastest Triton/FA4/8×B200 runner. The hybrid
attention architecture is real because the released trained VDN
checkpoint supplies its linear branch/LoRA and modular component. The
published highest-throughput timings cannot be claimed for this portable
path. Use Wan on macOS. In a typical 24-GB GPU, turn on
`offloadDit` to page transformer blocks between CPU and CUDA memory.
It may still be slow, and RAM/disk requirements can be substantial.

## Native learned latent upscale and refinement (8+3)

Select **Native learned 3D latent two-pass** in Studio with the Diffusers
backend. The entire implementation lives in
`scripts/studio_vdn_learned_upscaler.py` and
`scripts/studio-vdn-h3-worker.py`; Rust stages those sources together into
the real worker. This is **not** an adapter to FreeVideo.

1. The licensed OpenVDN H3 model generates eight steps on a 384×672
   preview canvas and checkpoints its real 24-channel video and audio
   latents, with the request fingerprint.
2. Little Monkey downloads the original LBH-123-AI 3D learned H3 resizer
   on first use to a profile-scoped cache. Its weight revision is
   `3f941d5d182014dd5c0a5e16330420ee2d4aa0c6`, size 690592672
   bytes, SHA-256
   `043e5a48e161610ef6c3ea974645220354d06fa618abca15f76d084812eb55c2`.
   The loader rejects an invalid or partial file. Code attribution and MIT
   text: `licenses/h3-latent-upscaler-MIT.txt`.
3. The native PyTorch 3D network applies the extra 24-channel H3
   normalization required by the published weights, infers the 2× latent
   lift (24×42 → 48×84 spatial cells), then reverses normalization.
   It chunks the temporal dimension on long clips with overlapping seams.
4. A calibrated 3-step H3 video sigma tail (0.9035, 0.6316, 0.3158, 0)
   and corresponding audio shift initialize and denoise the result at
   768×1344. The previously used plain trilinear resize + 10% noise is
   **not** a fallback: invalid weights stop the job. Existing completed
   preview checkpoints may be reused only when their fingerprint matches.

This trained checkpoint is calibrated for **exactly eight preview and three
refinement evaluations**. Single-pass inference still supports the ordinary
step options. CUDA and substantial system RAM/VRAM are required; the
native checkpoint is not included in the application binary. EU/UK/US/KR
restrictions on **VDN-H3 inference model weights** remain in force even if
the upscaler's own MIT license is permissive.

### Validation limits

The Python tests exercise pure CPU model forward passes with a tiny
weight-compatible architecture, strict file checks, latent time geometry,
chunking and schedule restoration. They do **not** establish real H3 GPU
video quality, audio continuity, exact trained checkpoint loading on CUDA,
or visual parity with FreeVideo. Those require an eligible machine,
licensed model weights and rendered video comparisons.

## Security and lifecycle contracts

- Backend authorizes territory and signed entitlements **before** any model
  code loads and repeats the check throughout inference, including before
  media publication.
- The native learned two-pass path additionally downloads the 3D upscaler
  weights only from `LBH-123-AI/Minimax_h3_latent_Upscaler` at a pinned
  immutable revision. It verifies the exact 690592672-byte file with SHA-256.
  The standalone inference code is part of Little Monkey; it does **not**
  import or start FreeVideo/ComfyUI.
- Upstream VDN model
  dependencies and remote code are separate user-installed third-party
  components with their own supply-chain risks.
- Python arguments are passed through `tokio::process::Command` directly,
  not through shell interpolation. The full prompt is bounded at 8 KiB.
- Output path is generated inside the selected Little Monkey profile,
  not supplied by the caller. One job runs at a time; a UUID selects
  cancellation, files are cleaned after completion or failure, and a four-hour
  wall prevents perpetual waits. Logs are not kept by the model worker.
- Only nonempty MP4 files with an MP4 `ftyp` box of <=256 MB are published
  into the existing Gallery content store. Fake successes are rejected.
- First/last frame files must have actual PNG/JPEG magic, exist and be <=32 MB.
- CPU-only CI runs the **real worker** with `--self-test` and invalid
  requests; Rust unit tests cover MP4 guards, cancellation contracts and
  rights policy. This is not equivalent to a real GPU model test.

## Physical E2E acceptance

After an approved machine has enough CUDA GPU resources and actual licensed
OpenVDN components, run at least:

- A 124-frame T2VA prompt, verifying a playable MP4 with audio in Gallery.
- I2VA first-frame generation, then FL2VA first+last-frame generation,
  inspecting that outputs reflect anchors.
- Same inputs with / without offload and optional fp8, recording wall time,
  peak RSS/VRAM and validated output duration.
- Cancel during sampling; ensure subprocess exits without a Gallery entry.
- Change territory / revoke entitlement while a job is running; ensure it
  stops and no artifact is published.

These tests require authentic model weights, a valid license and physical
CUDA resources; they **have not** been run by GitHub Actions. CPU/CI success
is not a claim that a particular GPU has produced the correct output.
