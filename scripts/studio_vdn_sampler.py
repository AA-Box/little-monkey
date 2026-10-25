"""Little Monkey-owned exact completed-step recovery for MiniMax-H3.

No FreeVideo, no pickle, no model downloads. Installed inline into the staged
Python worker by the Rust host. The public `run_completed_steps` and
`verify_step_metadata` helpers are CPU-only testable.
"""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import uuid

CHECKPOINT_SCHEMA = "little-monkey-h3-denoising-step-v1"
STEP_CHECKPOINT_MAX_BYTES = 64 * 1024 * 1024


def run_completed_steps(total, start, do_step, after_step):
    """Commit only fully completed steps. Exceptions leave the old cursor."""
    if not isinstance(total, int) or total < 1 or not 0 <= start <= total:
        raise ValueError("Invalid H3 step cursor")
    for index in range(start, total):
        do_step(index)
        after_step(index + 1)


def verify_step_metadata(meta, identity, stage, total, schedule_digest,
                         video_shape, audio_shape):
    if not isinstance(meta, dict):
        return None
    desired = {"schema": CHECKPOINT_SCHEMA, "identity": identity,
               "stage": stage, "total": str(total), "schedule": schedule_digest,
               "video_shape": ",".join(map(str, video_shape)),
               "audio_shape": ",".join(map(str, audio_shape))}
    if any(meta.get(key) != val for key, val in desired.items()):
        return None
    try:
        step = int(meta.get("completed", "-1"))
    except (ValueError, TypeError):
        return None
    return step if 1 <= step <= total else None


class NativeStepStore:
    def __init__(self, root, stage, identity, total, schedule_digest,
                 video_shape, audio_shape):
        self.root = Path(root)
        if not self.root.is_absolute() or stage not in ("single", "preview", "refine"):
            raise ValueError("Unsafe native H3 step checkpoint root or stage")
        self.stage = stage
        self.identity = identity
        self.total = total
        self.schedule_digest = schedule_digest
        self.video_shape = tuple(video_shape)
        self.audio_shape = tuple(audio_shape)
        if total < 1 or any(d <= 0 for d in self.video_shape + self.audio_shape):
            raise ValueError("Invalid step checkpoint geometry")
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        if self.root.is_symlink() or not self.root.is_dir():
            raise ValueError("Unsafe native H3 checkpoint directory")

    def filename(self, slot):
        if slot not in (0, 1):
            raise ValueError("Invalid step checkpoint slot")
        return self.root / ("%s.%s.safetensors" % (self.stage, slot))

    def metadata(self, completed):
        return {"schema": CHECKPOINT_SCHEMA, "identity": self.identity,
                "stage": self.stage, "total": str(self.total),
                "schedule": self.schedule_digest,
                "completed": str(completed),
                "video_shape": ",".join(map(str, self.video_shape)),
                "audio_shape": ",".join(map(str, self.audio_shape))}

    def latest(self):
        """Pick highest fully committed slot, rejecting mismatches and corruption."""
        from safetensors import safe_open
        best = None
        for slot in (0, 1):
            path = self.filename(slot)
            if not path.exists() and not path.is_symlink():
                continue
            stat = path.lstat()
            if not path.is_file() or path.is_symlink() or not 128 <= stat.st_size <= STEP_CHECKPOINT_MAX_BYTES:
                continue
            try:
                with safe_open(str(path), framework="pt", device="cpu") as checkpoint:
                    step = verify_step_metadata(
                        checkpoint.metadata(), self.identity, self.stage,
                        self.total, self.schedule_digest, self.video_shape,
                        self.audio_shape)
                    if step is None or (best is not None and step <= best[0]):
                        continue
                    video = checkpoint.get_tensor("video")
                    audio = checkpoint.get_tensor("audio")
                    if (tuple(video.shape) != self.video_shape
                            or tuple(audio.shape) != self.audio_shape
                            or not video.is_floating_point()
                            or not audio.is_floating_point()):
                        continue
                    best = step, video, audio
            except (OSError, ValueError, RuntimeError, KeyError):
                # A crash during write cannot destroy the previous slot.
                continue
        return best

    def save(self, completed, video, audio):
        from safetensors.torch import save_file
        if not 1 <= completed <= self.total:
            raise ValueError("Out-of-range native H3 step commit")
        if tuple(video.shape) != self.video_shape or tuple(audio.shape) != self.audio_shape:
            raise ValueError("Native H3 step tensor shape changed")
        # The high-water mark is committed only after both schedulers' complete
        # updates; tensors are plain data, NEVER torch.load/pickle objects.
        dest = self.filename(completed % 2)
        temporary = self.root / (".%s.%s.tmp" % (self.stage, uuid.uuid4()))
        try:
            save_file({"video": video.detach().float().cpu().contiguous(),
                       "audio": audio.detach().float().cpu().contiguous()},
                      str(temporary), metadata=self.metadata(completed))
            if temporary.stat().st_size > STEP_CHECKPOINT_MAX_BYTES:
                raise ValueError("Native sampler step checkpoint exceeds 64 MiB")
            with open(temporary, "rb") as handle:
                os.fsync(handle.fileno())
            os.replace(temporary, dest)
            if os.name == "posix":
                fd = os.open(self.root, os.O_RDONLY)
                try:
                    os.fsync(fd)
                finally:
                    os.close(fd)
        finally:
            temporary.unlink(missing_ok=True)

    def clear(self):
        for slot in (0, 1):
            self.filename(slot).unlink(missing_ok=True)


@contextlib.contextmanager
def native_sampler_recovery(args, stage, pipeline, *, model_identity):
    """Own the real Diffusers H3 loop for this worker call only.

    On restart, reconstruct the *original* deterministic text/keyframe/reference
    conditioning, restore generated video/audio rows, and point BOTH official
    Euler schedulers to the next uncompleted index. An in-flight kernel is
    never claimed as completed.
    """
    if not args.step_checkpoint_root:
        yield
        return
    import torch
    import diffusers
    from diffusers.modular_pipelines.minimax_h3.denoise import MiniMaxH3DenoiseLoopWrapper

    base = MiniMaxH3DenoiseLoopWrapper.__call__
    import hashlib as _hashlib
    digest = _hashlib.sha256()
    digest.update(checkpoint_identity(args).encode("ascii"))
    digest.update(stage.encode("ascii"))
    digest.update(model_identity.encode("utf-8"))
    digest.update(str(args.internal_attempt).encode("ascii"))
    digest.update(str(diffusers.__version__).encode("ascii"))
    digest.update(str(torch.__version__).encode("ascii"))
    if stage == "refine":
        source = args.refine_from or args.checkpoint
        with open(source, "rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    request_identity = digest.hexdigest()

    def run_with_checkpoints(loop, components, state):
        block = loop.get_block_state(state)
        total = len(block.timesteps)
        if total < 1:
            raise ValueError("MiniMax H3 returned an empty denoising schedule")
        cv = int(block.num_condition_video_rows)
        ca = int(block.num_condition_audio_rows)
        if cv < 0 or ca < 0:
            raise ValueError("Negative H3 conditioning row offset")
        initial_video = block.latents[cv:]
        initial_audio = block.audio_latents[ca:]
        schedule = _hashlib.sha256()
        for tensor in (block.timesteps, block.audio_timesteps,
                       components.scheduler.sigmas, components.audio_scheduler.sigmas):
            schedule.update(tensor.detach().to("cpu").contiguous().numpy().tobytes())
        store = NativeStepStore(
            args.step_checkpoint_root, stage, request_identity, total,
            schedule.hexdigest(), initial_video.shape, initial_audio.shape)
        snapshot = store.latest()
        start = 0
        if snapshot is not None:
            start, video, audio = snapshot
            block.latents[cv:] = video.to(
                device=block.latents.device, dtype=block.latents.dtype)
            block.audio_latents[ca:] = audio.to(
                device=block.audio_latents.device, dtype=block.audio_latents.dtype)
            if start < total:
                # Official scheduler API; set_timesteps called anew by the H3
                # before-denoise block, so both begin indices must be restored.
                components.scheduler.set_begin_index(start)
                components.audio_scheduler.set_begin_index(start)
            print(json.dumps({"phase": "sampler_recovered", "stage": stage,
                              "completedSteps": start, "totalSteps": total}),
                  flush=True)

        with loop.progress_bar(total=total) as progress:
            if start:
                progress.update(start)
            def do_step(index):
                nonlocal components, block
                components, block = loop.loop_step(
                    components, block, i=index, t=block.timesteps[index])
                if (components.scheduler.step_index != index + 1
                        or components.audio_scheduler.step_index != index + 1):
                    raise RuntimeError("Both MiniMax H3 schedulers must commit each step")

            def commit(completed):
                store.save(completed, block.latents[cv:], block.audio_latents[ca:])
                print(json.dumps({"phase": "sampler_step_committed",
                                  "stage": stage, "completedSteps": completed,
                                  "totalSteps": total}), flush=True)
                progress.update()

            run_completed_steps(total, start, do_step, commit)
        loop.set_block_state(state, block)
        return components, state

    MiniMaxH3DenoiseLoopWrapper.__call__ = run_with_checkpoints
    try:
        yield
    finally:
        MiniMaxH3DenoiseLoopWrapper.__call__ = base


def clear_native_sampler_stage(args, stage):
    if not args.step_checkpoint_root:
        return
    root = Path(args.step_checkpoint_root)
    for slot in (0, 1):
        (root / ("%s.%s.safetensors" % (stage, slot))).unlink(missing_ok=True)
