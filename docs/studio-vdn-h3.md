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

## Security and lifecycle contracts

- Backend authorizes territory and signed entitlements **before** any model
  code loads and repeats the check throughout inference, including before
  media publication.
- The only remotely loaded model is `OpenVDN/vdn-minimax-h3`. Python model
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


## Native long-video temporal continuity (Diffusers backend)

Set **Native continuous segments** to 2–8 in Studio. Each bounded H3
generation uses an ordinary 17n+5-frame window, and each later window
reuses the preceding **seven video-latent frames (22 decoded frames)** and
the corresponding 40-Hz stereo audio-latent tail. A transformer pre-hook
marks these packed AV rows as *clean* (timestep 1), and a sampler callback
restores their original values after every dual-scheduler update. The same
protected context is restored on recovered denoising steps.

The worker calculates absolute 24-fps / 40-Hz offsets rather than guessing
audio trims from video frame count, and writes decoded frames to a bounded
disk-backed array. It assembles sample-aligned stereo audio with sub-tick
seam correction, then encodes a **single MP4 with audio**, avoiding the
audio loss and timestamp gaps caused by naively concatenating MP4 files.
The resulting number of frames is `frames + (segments - 1) * (frames - 22)`.

Each segment's completed denoising step is checkpointed under an isolated
stage name; the checkpoint fingerprint includes the *actual preceding
audio/video latent bytes*. Changing the predecessor invalidates a saved
step. An interrupted overall run recomputes earlier completed segments,
but can restore the last interrupted segment's completed steps if context
matches. Temporary frames are removed on success or failure.

Boundaries: this is an independent Little Monkey native implementation,
**not a FreeVideo runtime dependency**. A physical licensed CUDA/H3 model
test is still needed to certify visual and voice quality, sample
compatibility across released Diffusers versions, and performance. This native
implementation requires the installed H3 transformer interfaces;
model territory restrictions and upstream-code consent remain unchanged.

### Two-pass rolling windows

For a 2–8-segment generation, choose **Native learned 3D two-pass (8+3)**. Each segment
now executes *two model sampling passes*, not a preview job followed by
independent outputs: the half-resolution pass receives the exact prior
half-resolution video/audio latent tail; the full-resolution refinement pass
receives the matching full-resolution tail. The preview's video latents
are passed through the independently implemented, SHA-256-pinned,
trained 3D H3 latent upscaler from LBH-123-AI, then initialized on the
native three-step calibrated H3 video/audio sigma schedule. No interpolation
fallback is accepted by native two-pass. Each pass has a distinct
completed-denoising-step checkpoint and a content-bound Safetensors
preview checkpoint. A restart can reuse a matching preview; an outdated or
corrupt local preview is regenerated. The trained checkpoint requires exactly
**eight preview and three refinement evaluations**. It is downloaded only
after licensed/territorial runtime gating and verified against the publisher's
SHA-256, original revision and byte count. Independent code and license:
`scripts/studio_vdn_learned_upscaler.py` and
`licenses/h3-latent-upscaler-MIT.txt`.

### Continue from a prior native video

After the MP4 is encoded, the worker writes a small, atomic
`temporal-contexts/<job-id>.safetensors` snapshot containing the *actual
final seven visual latents* and the matching stereo audio latent tail.
For two-pass videos it persists both preview and final-resolution tails;
for regular single-pass videos the low-resolution tail is derived from
the final-resolution latents. The snapshot includes the original
job ID, model identity, total-frame timeline and SHA-256 of the exact MP4.

In Studio choose an existing native Diffusers Gallery clip or a queued
predecessor and select **Continue using saved native video + audio latents**.
The durable job resolver accepts only a completed native Gallery job,
verifies the artifact ID, bounds the saved snapshot and provides the worker
the exact source job and pinned MP4 digest. The worker verifies the
Safetensors tensor digest, geometry, model, job identity, video SHA-256 and
global 24-fps/40-Hz AV time origins before GPU sampling. Invalid or legacy
sources fail explicitly instead of silently degrading to frame-only
conditioning.

The first continued window consumes 22 old frames as protected context,
so it samples only `frames - 22` *new* frames. Later windows reuse the
preceding window's actual AV latent tails. The worker now decodes the
**entire original Gallery chain** plus newly sampled material and emits
**one full MP4** with no duplicated overlap; the parent remains independently
available in Gallery. A native streaming PyAV mux assembles the continuous
24-fps video and 32-kHz stereo PCM with 10-ms seam conditioning before
encoding AAC once. The joined artifact's exact bytes are pinned in the
next AV latent snapshot; the durable Gallery frame count is read from
the actual worker's verified completion record.

To avoid losing progress after a crash, each completed segment atomically
commits its full RGB8 frame bytes, float32 stereo PCM and matching *high/low*
AV-latent context. A separate Safetensors commit marker records source
fingerprints, absolute timeline and SHA-256 of the finished bytes. On retry,
unchanged windows are read as disk-backed arrays and are never re-sampled
on CUDA; mismatched/tampered/incomplete segments are discarded. The job
owns a bounded cache directory and the disk preflight runs before writing.
Preview checkpoint recovery and sampler-step recovery still work for
unfinished windows. The durable resolver also follows the authorized job
ancestry to reuse unchanged original Ref2VA visual/audio references
along the chain; missing or changed reference files are rejected.

### Original identity, streaming chain joining, and bounded recovery

On the first successful native Gallery film, Little Monkey extracts an
immutable, profile-owned PNG from the **original opening frame**. The pixel
data is SHA256-pinned into each job's final AV-latent Safetensors record,
including descendant jobs. A continuation verifies that the predecessor's
identity PNG matches its stored fingerprint, and passes the same picture to
the FL2VA workflow's Qwen3-VL visual text conditioning **on each window**.
Unlike a replacement keyframe, this is compatible with the real 22-frame
protected predecessor latent prefix: the original is the fixed visual
reference; the existing AV tail remains the temporal start. Native Ref2VA
continuations also retain their original immutable and verified image/audio
references from the durable job ancestry. A legacy parent without this
identity record fails explicitly rather than pretending character identity
has been preserved. Semantic/perceptual identity quality still needs actual
model evaluation; this mechanism does not guarantee unchanging faces.

Native chained MP4 output now uses **bounded streaming** decoding and
32-kHz stereo AAC encoding. The edge repair holds at most 320 samples of
the previous soundtrack (plus one decoder chunk). It does not allocate
the entire original chain soundtrack in RAM. H.264 clips with identical
codec headers, canvas and frame rates take a packet-copy path with rebased
PTS/DTS and a post-mux decoded-frame count check, avoiding reencoding
earlier film segments. Any unsupported packet timing, mismatch or invalid
copy is discarded, and a native x264 rebuild is used instead. The
original parent Gallery artifact is never overwritten.

Crash recovery retains only verified completed window RGB8 bytes,
independently decoded PCM, and high/low audiovisual latent tails.
The cache is limited to **12 GiB per job** and leaves at least **1 GiB**
free for final assembly. Optional caching is bypassed (with a progress
event) when the budget or disk reserve is insufficient, so an otherwise
valid inference does not fail solely because of its extra checkpoint.
A native job lifecycle cleanup removes completed/cancelled/exhausted
retry caches and prunes stale or orphaned scratch directories on startup.
Resumable jobs retain committed windows and sampler/preview recovery data.
No claim of live, mid-GPU-kernel state migration is made.

### Acceptance boundary

The CPU contracts validate deterministic frame/audio planning, source
ownership/size rules, preview fingerprints, staging and Python syntax.
Optional real-tensor tests exercise Safetensors roundtrips and a mocked
two-pass parent-to-child path when CPU PyTorch and Safetensors are installed.
These do not substitute for the licensed CUDA H3 models: physical
multi-segment visual continuity, voice identity, audiovisual synchrony,
exact Diffusers version compatibility, checkpoint resume and resource
usage still require real-hardware acceptance before parity is claimed.
