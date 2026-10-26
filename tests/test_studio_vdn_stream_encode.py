"""Incremental H3 video regression: preserve temporal overlap and frame order.

Real torch tensor math: original batch decode reference vs native frame-chunk
iterator, including token tail padding and emergency interrupted MP4 cleanup.
"""
import importlib.util
from pathlib import Path
import tempfile
import types
import unittest
import sys
import torch

SRC = Path(__file__).resolve().parents[1] / "scripts/studio_vdn_stream_encode.py"
spec = importlib.util.spec_from_file_location("native_mp4_stream", SRC)
stream = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stream)


class FakeVAE:
    def __init__(self):
        self.tokens_chunk_size = 5
        self.temporal_compression_ratio = 4
        self.frame_pre_padding = 3
        self.token_overlap = 2
        self.frame_overlap = 5
        self.config = types.SimpleNamespace(token_drop=3, clip_length=17,
                                            latents_mean=(0.0,)*3,
                                            latents_std=(1.0,)*3)
        self.post_quant_conv = types.SimpleNamespace(
            weight=torch.ones(1, dtype=torch.float32))
        self.max_forward_tokens = 0

    def _decode_clip(self, z):
        self.max_forward_tokens = max(self.max_forward_tokens, z.shape[2])
        return z.repeat_interleave(self.temporal_compression_ratio, dim=2)

    @staticmethod
    def _blend(a, b, extent, dim):
        n = min(a.shape[dim], b.shape[dim], extent)
        positions = torch.arange(n, device=b.device, dtype=b.dtype)
        weight_b = (positions / n).view(
            [n if i == (dim % a.ndim) else 1 for i in range(a.ndim)])
        weight_a = 1 - weight_b
        front = a.narrow(dim, a.shape[dim]-n, n)*weight_a + b.narrow(dim, 0, n)*weight_b
        return torch.cat([front, b.narrow(dim, n, b.shape[dim]-n)], dim=dim)

    def baseline(self, z):
        # Reproduce the upstream MiniMaxH3._decode reference, intentionally
        # accumulating its output only inside this bounded test fixture.
        t, drop = self.tokens_chunk_size, self.config.token_drop
        tokens = z.shape[2]+drop
        pad = (-tokens)%t
        n = (tokens+pad)//t - int(drop>0)
        if pad:
            z = torch.cat([z,z[:,:,-1:].repeat(1,1,pad,1,1)],dim=2)
        rows=[]
        overlap=None
        for i in range(n):
            clip=self._decode_clip(z[:,:,i*t:i*t+t+self.token_overlap])
            for j in range(2):
                chunk=clip[:,:,j*t*4:(j+1)*t*4]
                chunk=chunk[:,:,self.frame_pre_padding:]
                if j==0:
                    if overlap is not None:
                        chunk=self._blend(overlap,chunk,self.frame_overlap,dim=-3)
                    rows.append(chunk)
                else:
                    overlap=chunk
        if overlap is not None:
            rows.append(overlap)
        dec=torch.cat(rows,dim=2)
        if pad:
            tail=self.config.clip_length%4
            before=z.shape[2]-pad
            pad_frames=sum(tail if tail and (before+k)%t==0 else 4 for k in range(pad))
            dec=dec[:,:,:-pad_frames]
        return dec


class StreamingVideoTests(unittest.TestCase):
    def test_exact_reference_blend_and_pad_for_three_lengths(self):
        torch.manual_seed(9)
        for length in (7, 8, 42):
            with self.subTest(tokens=length):
                vae=FakeVAE()
                video=torch.rand(1,3,length,2,2)
                expected=vae.baseline(video)
                n=expected.shape[2]
                raw=list(stream.iter_h3_video_rgb_chunks(
                    vae,video,[0,0,0],[1,1,1],expected_frames=n))
                self.assertLessEqual(vae.max_forward_tokens,7)
                self.assertLessEqual(max(x.shape[0] for x in raw),20)
                actual=torch.cat(raw,dim=0)
                reference=(expected[0].permute(1,2,3,0)*255).round().to(torch.uint8)
                torch.testing.assert_close(actual,reference,rtol=0,atol=0)
                self.assertEqual(actual.shape[0],n)

    def test_incorrect_length_fails_loudly(self):
        vae=FakeVAE()
        z=torch.rand(1,3,42,2,2)
        with self.assertRaisesRegex(RuntimeError,"decoded"):
            list(stream.iter_h3_video_rgb_chunks(
                vae,z,[0,0,0],[1,1,1],expected_frames=200))

    def test_atomic_stream_and_audio_without_collecting_entire_movie(self):
        class Audio:
            config=types.SimpleNamespace(latents_mean=(0.0,)*4,
                                         latents_std=(1.0,)*4)
            def decode(self,z,return_dict=False):
                self.last_shape=tuple(z.shape)
                return (z[:,:1].repeat_interleave(800,dim=2),)
        video=FakeVAE()
        audio=Audio()
        pipe=types.SimpleNamespace(
            vae=video,audio_vae=audio,pixel_mean=[0.,0.,0.],
            pixel_std=[1.,1.,1.],audio_sampling_rate=32000)
        # Replace only the public Diffusers MP4 API with a fixture that
        # exhausts the real iterator, observes bounded chunks, then writes.
        root=types.ModuleType("diffusers")
        utils=types.ModuleType("diffusers.utils")
        exporter=types.ModuleType("diffusers.utils.export_utils")
        captures=[]
        def encode_video(chunks,fps,output_path,audio,audio_sample_rate):
            for chunk in chunks:
                captures.append(tuple(chunk.shape))
            self.assertEqual(audio.shape[0],2)
            self.assertEqual(audio_sample_rate,32000)
            Path(output_path).write_bytes(b"native-streamed-mp4-fixture")
        exporter.encode_video=encode_video
        original={n:sys.modules.get(n) for n in (
            "diffusers","diffusers.utils","diffusers.utils.export_utils")}
        sys.modules.update({"diffusers":root,"diffusers.utils":utils,
                            "diffusers.utils.export_utils":exporter})
        try:
            with tempfile.TemporaryDirectory() as d:
                output=Path(d)/"video.mp4"
                count=stream.encode_h3_latent_video(
                    pipe,torch.rand(1,3,42,2,2),torch.rand(2,4,11),
                    output_path=output,frames=141,fps=24)
                self.assertEqual(count,141)
                self.assertTrue(output.is_file())
                self.assertFalse(output.with_suffix(".partial.mp4").exists())
                self.assertEqual(sum(shape[0] for shape in captures),141)
                self.assertGreater(len(captures),1)
        finally:
            for name,prior in original.items():
                if prior is None:sys.modules.pop(name,None)
                else:sys.modules[name]=prior


if __name__=="__main__":
    unittest.main()
