# Durable Studio video jobs (PR 4 / 5)

A video render no longer depends on the React view remaining mounted. The
backend writes a profile-scoped job record **before** it loads a native model,
then keeps the video worker running until the model returns or the app exits.
The frontend polls that record and can find it again on reopening Studio.

## Contract

- \`generation_video_submit(request, idempotencyKey, dependsOn)\`: exact
  idempotency, with conflict rejection if the key is reused for different
  inputs or prerequisites. Video tasks only. At most 128 retained jobs,
  16 predecessors, 48 MiB per private journal record.
- \`generation_video_jobs\`: read persisted stage, status, retries and gallery
  entry IDs; reconcile gallery publication when a crash split result publishing
  from marking the job complete; mark orphaned \`running\` jobs \`interrupted\`.
  The original prompts are shortened and inline input images are not exposed
  to this listing.
- \`generation_video_resume(jobId)\`: explicit, bounded replay of the saved
  request from its beginning after a failed/interrupted job. Blank/random
  seeds are pinned once when the journal is created. It does not replay automatically after a process crash.
- \`generation_video_cancel(jobId)\`: cancel queued work or stop the process
  that owns active work. No need to release other jobs' active generations.
- Dependencies are explicit completed-job IDs, never inferred from prompt
  text. The dispatcher starts waiting children only after **all**
  predecessors completed, even after reopening Studio. New dependencies must
  already exist so cycles are not possible.

## Supported checkpoints and limitations

The gallery publication is the authoritative durable success checkpoint;
each entry carries the originating \`videoJobId\`. A restart after gallery
publication but before journal completion reconciles to \`completed\`
without re-rendering. The journal uses write-temp, sync, atomic rename and
app-private permissions on Unix.

The bundled sd-server and MLX video service do **not** export restartable
sampler/frame checkpoints. A crash during sampling cannot resume from an
individual denoising step: the UI marks the job interrupted and offers a
**new render from the persisted request**. The running engine cannot be
adopted across desktop process crashes. Fixed seeds make retry reproducible
subject to backend deterministic behavior; randomness, model/runtime changes,
and nondeterministic kernels can still change output.

Dependencies are *ordering* edges between video jobs, not automatically
wired media inputs. Image-to-video uses its own saved starting image.

## Acceptance

- Submit a video, navigate away from the Studio panel, and return: the worker
  remains running and the job appears in the panel.
- Start a video, close the whole app, relaunch, and open Studio: persisted
  \`running\` becomes \`interrupted\`; explicit Resume retries once. It must
  not claim a mid-frame resume.
- Kill the desktop process after a successful gallery write but before the
  final journal transition: listing must reconcile the published entry and
  Resume must not cause a duplicate render.
- Reuse an idempotency key with identical content: return the same job; alter
  the request: reject. A dependency waits until its parent finishes.
- Cancel while queued and while rendering; cancellation must not
  accidentally stop another queued job's engine.
- Verify on a real video model and actual desktop restart. Unit tests prove
  serialization, dependency gating, and path safety; physical-engine
  acceptance requires a model-installed host and is not asserted by CI.


## Boot recovery and video-to-video chaining

The Tauri setup hook launches the journal reconciler and dispatcher without a Studio UI request. Queued children of completed jobs resume scheduling automatically. An interrupted native sampling operation remains interrupted until the user explicitly requests a new render; native engines do not support restoring mid-frame sampler state.

A single dependent video job automatically selects its parent as the source. With multiple predecessors, the caller must choose sourceJobId. The worker verifies a published, content-addressed parent video blob in the gallery, decodes its actual final PNG frame using FFmpeg, and supplies the PNG to the child's image_to_video input. SourceArtifactId is recorded for provenance. The Studio Continue button creates the next clip using the selected parent, with a new-scene prompt. If FFmpeg is unavailable, the source format is unsupported, or the receiving model lacks image_to_video support, the job fails visibly rather than silently generating an unrelated clip.

These are last-frame handoffs, not cross-model latent or audio-context continuity, and the separate timeline editor concatenates published clips into one MP4. Installed FFmpeg is an explicit runtime dependency for chaining. Real installed-model and actual desktop crash/restart acceptance remain necessary.


## Timeline editor and real MP4 assembly

The video tab now has an ordered timeline editor. Pick **2–16** Studio video
jobs in the desired order (unfinished jobs can be selected); adjust start/end
trims in milliseconds, add an optional local soundtrack, and submit an MP4
export. The journal stores the exact clip order and trims, and the
startup-independent dispatcher begins the export when every selected clip has
published valid video bytes. A failed/cancelled source fails the export
visibly. Export runs out-of-process via FFmpeg with a 30-minute limit,
app-private scratch data, a bounded output, an explicit running/failed state,
cancellation, and manual retry.

FFmpeg normalizes mixed MP4/WebM sources to a common 24fps, aspect-preserving
canvas, pads rather than crops, then concatenates them in order. Optional
audio is a **user-picked** source track that loops to the end of the video.
Without a soundtrack, the compositor preserves embedded clip audio and inserts aligned silence for clips without audio; a picked soundtrack overrides all clip audio. This does not retain native
audio from source clips; it also does not carry any diffusion latent states
between clips. The resulting \`studio:timeline:<id>\` MP4 is published into
the normal Studio gallery. A crash between publication and marking the export
complete is reconciled from the gallery, so retry cannot duplicate it.

The timeline journal lives in the profile folder and is not sent to a hosted
rendering service. Each input is resolved from a published
\`videoJobId\` gallery entry rather than arbitrary file paths. The optional
soundtrack is validated as a local regular audio file.

## FreeVideo (user-owned ComfyUI integration)

In Studio's media workflow area, select **FreeVideo (ComfyUI)** and import an
API-format workflow JSON exported from *your own installed FreeVideo nodes*.
Pick first frame, last frame, video reference and/or audio reference, provide
a prompt and seed, then run the workflow through the existing ComfyUI executor.
Each uploaded file is passed by the typed names
\`input_image\`, \`last_frame_image\`, \`input_video\`, \`input_audio\`;
your imported graph must place these placeholder values on its matching
loader nodes. ComfyUI accepts video/audio files through the \`upload/image\`
route as generic files in its input directory. The returned outputs go into
the Studio gallery.

FreeVideo's engine-dependent latent/motion conditioning runs **inside the
actual FreeVideo/ComfyUI graph**, not by pretending FFmpeg last-frame chaining
equals native latent transfer. Little Monkey does not automatically install
or redistribute FreeVideo, and this adapter does not claim to resume a native
FreeVideo sampler across a process crash. Running it requires the user's
ComfyUI service, FreeVideo nodes, models and a compatible imported API graph.

### Validation boundary

Tests cover journal validation, ordered FFmpeg graph construction, actual
MP4 creation with FFmpeg and decode, source ownership, and workflow reference
binding. CI cannot prove visually coherent latent continuity between
model-generated segments or exercise a user's installed FreeVideo graph.
Real hardware / model checks and desktop crash/relaunch checks are still
required to certify those behaviors.
