"""CPU validation for persistent, source-pinned H3 AV continuation and two-pass windows.

No CUDA, remote weights or FreeVideo dependencies. The mocked pipeline only
exercises the actual orchestration and tensor/codec contracts, not model quality.
"""
import contextlib
import hashlib
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
import uuid

SCRIPT = Path(__file__).resolve().parents[1] / "scripts"


def owned(name):
    path = SCRIPT / name
    spec = importlib.util.spec_from_file_location(name.replace(".", "_"), path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


store = owned("studio_vdn_temporal_store.py")
temporal = owned("studio_vdn_temporal.py")


class MathAndProvenance(unittest.TestCase):
    def test_preview_identity_and_bounds(self):
        p = store.preview_path("/tmp/previous.safetensors", 7)
        self.assertTrue(p.endswith(".segment7.safetensors"))
        self.assertNotEqual(store.preview_identity("request", 0, "old"),
                            store.preview_identity("request", 0, "new"))
        self.assertNotEqual(store.preview_identity("request", 0, None),
                            store.preview_identity("request", 1, None))
        for index in (-1, 8, 999):
            with self.assertRaises(ValueError):
                store.preview_path("/tmp/a", index)

    def test_audio_timeline_and_window_math(self):
        self.assertEqual(store.audio_origin(119), temporal.audio_origin(119))
        self.assertEqual(temporal.temporal_frame_plan(124, 2), 226)
        self.assertEqual(temporal.emitted_audio_samples(226), 301333)


try:
    import torch
    import numpy as np
    import safetensors
    READY = True
except ImportError:
    READY = False


@unittest.skipUnless(READY, "Requires optional CPU PyTorch/numpy/safetensors wheels")
class RealTensorAndWindowContracts(unittest.TestCase):
    def make_context(self, frames):
        high = {"video": torch.ones((1, 24, 7, 48, 84)),
                "audio": torch.ones((2, 32, 37)),
                "frame_origin": frames-22,
                "audio_origin": store.audio_origin(frames-22)}
        low = store.preview_fallback(high)
        return high, low

    def test_real_safetensors_roundtrip_and_tamper(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            job = str(uuid.uuid4())
            mp4 = root / "source.mp4"
            mp4.write_bytes(b"\x00\x00\x00\x18ftypisom" + b"\x00" * 1024)
            h, l = self.make_context(141)
            destination = root / "context.safetensors"
            store.save_context(destination, h, l, mp4, "vdn-h3", job, 141)
            expected = hashlib.sha256(mp4.read_bytes()).hexdigest()
            loaded = store.load_context(destination, expected, "vdn-h3", job)
            self.assertEqual(loaded["total_frames"], 141)
            self.assertTrue(torch.equal(loaded["high"]["video"], h["video"]))
            self.assertTrue(torch.equal(loaded["low"]["video"], l["video"]))
            for digest, model, job_id in [
                ("a"*64, "vdn-h3", job), (expected, "different", job),
                (expected, "vdn-h3", str(uuid.uuid4())),
            ]:
                with self.assertRaises(ValueError):
                    store.load_context(destination, digest, model, job_id)
            mp4.write_bytes(b"different MP4")
            with self.assertRaises(ValueError):
                store.load_context(destination, hashlib.sha256(mp4.read_bytes()).hexdigest(),
                                   "vdn-h3", job)

    def test_preview_checkpoint_refuses_changed_context(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "preview.safetensors"
            identity = store.preview_identity("input", 1, "prefix")
            video = torch.zeros((1, 24, 8, 24, 42))
            audio = torch.zeros((2, 32, 207))
            store.save_preview(path, video, audio, identity)
            a, b = store.load_preview(path, identity)
            self.assertTrue(torch.equal(a, video))
            self.assertTrue(torch.equal(b, audio))
            with self.assertRaises(ValueError):
                store.load_preview(path, store.preview_identity("input", 1, "altered"))

    def test_two_pass_parent_then_child_reuses_real_context(self):
        class FakePrefix:
            def __init__(self, _model, data, _device):
                self.digest = hashlib.sha256(
                    data["video"].numpy().tobytes() + data["audio"].numpy().tobytes()
                ).hexdigest()
            def finish(self, _video, _audio):
                pass

        class FakePipe:
            def __init__(self):
                self.calls = []
            def __call__(self, **kwargs):
                self.calls.append(kwargs)
                frames = kwargs["num_frames"]
                shape = (24, 42) if kwargs.get("height") == 384 else (48, 84)
                latent = torch.zeros((1, 24, 8, *shape))
                sound = torch.zeros((2, 32, 207))
                result = {"latents": latent, "audio_latents": sound}
                if "videos" in kwargs["output"]:
                    result["videos"] = [[np.zeros((32, 32, 3), dtype=np.uint8)
                                         for _ in range(frames)]]
                    result["audio"] = torch.full((1, 2, 207 * 800),
                        0.65 if len(self.calls) > 2 else -0.35)
                    result["sampling_rate"] = 32000
                return result

        original = temporal.NativeTemporalPrefix
        old_join = getattr(temporal, "join_native_chain", None)
        old_upscaler = getattr(temporal, "native_learned_upscale", None)
        old_schedule = getattr(temporal, "native_h3_refine_schedule", None)
        temporal.native_learned_upscale = lambda video, cache, target: torch.nn.functional.interpolate(
            video, size=(video.shape[2], *target), mode="trilinear", align_corners=False)
        temporal.native_h3_refine_schedule = lambda pipe, torch: contextlib.nullcontext()
        temporal.NativeTemporalPrefix = FakePrefix
        def fake_join(parent, child, target, *, parent_frames, child_frames, expected_parent_sha256):
            self.assertEqual(hashlib.sha256(Path(parent).read_bytes()).hexdigest(), expected_parent_sha256)
            self.assertEqual(parent_frames, 226)
            self.assertEqual(child_frames, 102)
            Path(target).write_bytes(Path(child).read_bytes())
            return {"frames": parent_frames + child_frames, "audioSamples": temporal.emitted_audio_samples(parent_frames + child_frames)}
        temporal.join_native_chain = fake_join
        try:
            with tempfile.TemporaryDirectory() as temp:
                base = Path(temp)
                encoded_pcm = []
                def run(job_id, source_path="", source_sha="", source_id="", segments=2,
                        refine_from="", expect_cached=False):
                    args = SimpleNamespace(
                        prompt="moving characters", output=str(base / (job_id + ".mp4")),
                        seed=42, frames=124, segments=segments, steps=8, refine_steps=3,
                        two_pass=True, checkpoint=str(base / (job_id + ".preview")),
                        refine_from=refine_from, step_checkpoint_root=str(base / "step"),
                        upscaler_cache_dir=str(base / "model-cache"),
                        temporal_context_in=source_path,
                        temporal_parent=str(base / (source_id + ".mp4")) if source_id else "",
                        temporal_segment_root=str(base / (job_id + ".parts")),
                        temporal_context_out=str(base / (job_id + ".safetensors")),
                        temporal_source_video_sha256=source_sha,
                        temporal_source_job_id=source_id, job_id=job_id,
                    )
                    pipe = FakePipe()
                    def encoder(_video, *, fps, output_path, audio, audio_sample_rate):
                        self.assertEqual(fps, 24)
                        self.assertEqual(audio_sample_rate, 32000)
                        self.assertEqual(audio.shape[1], temporal.emitted_audio_samples(_video.shape[0]))
                        encoded_pcm.append(hashlib.sha256(
                            audio.detach().cpu().contiguous().numpy().tobytes()).hexdigest())
                        Path(output_path).write_bytes(b"\x00\x00\x00\x18ftypisom" + b"x"*128)
                    report = temporal.render_temporal_video(
                        pipe, args, torch, "cpu", {}, [], "vdn-h3", object(),
                        encoder, lambda *a, **kw: contextlib.nullcontext(),
                        lambda *a: None, "request-identity",
                    )
                    self.assertEqual(len(pipe.calls), 0 if expect_cached else
                                     segments if refine_from else 2 * segments)
                    self.assertTrue(all("latents" in call["output"] for call in pipe.calls))
                    return args, report
                parent = str(uuid.uuid4())
                args, report = run(parent)
                self.assertEqual(report["videos"], 226)
                self.assertEqual(report["audioSamples"], temporal.emitted_audio_samples(226))
                self.assertTrue(Path(store.preview_path(args.checkpoint, 0)).is_file())
                self.assertTrue(Path(store.preview_path(args.checkpoint, 1)).is_file())
                _, cached_report = run(parent, expect_cached=True)
                self.assertEqual(cached_report["videos"], 226)
                self.assertEqual(encoded_pcm[0], encoded_pcm[1],
                                 "Crash resume must reconstruct exactly the same stereo PCM seams")
                refined_job = str(uuid.uuid4())
                copied, again = run(refined_job, segments=2, refine_from=args.checkpoint)
                self.assertEqual(again["videos"], 226)
                self.assertTrue(Path(copied.output).is_file())
                digest = hashlib.sha256(Path(args.output).read_bytes()).hexdigest()
                previous = store.load_context(args.temporal_context_out, digest, "vdn-h3", parent)
                self.assertEqual(previous["total_frames"], 226)
                child = str(uuid.uuid4())
                next_args, next_report = run(
                    child, args.temporal_context_out, digest, parent, segments=1)
                self.assertEqual(next_report["videos"], 226 + 124 - 22)
                self.assertEqual(next_report["audioSamples"],
                                 temporal.emitted_audio_samples(226 + 124 - 22))
                out_sha = hashlib.sha256(Path(next_args.output).read_bytes()).hexdigest()
                inherited = store.load_context(next_args.temporal_context_out, out_sha,
                                                "vdn-h3", child)
                self.assertEqual(inherited["total_frames"], 226 + 102)
        finally:
            temporal.NativeTemporalPrefix = original
            if old_join is None:
                del temporal.join_native_chain
            else:
                temporal.join_native_chain = old_join
            if old_upscaler is None:
                del temporal.native_learned_upscale
            else:
                temporal.native_learned_upscale = old_upscaler
            if old_schedule is None:
                del temporal.native_h3_refine_schedule
            else:
                temporal.native_h3_refine_schedule = old_schedule


if __name__ == "__main__":
    unittest.main()
