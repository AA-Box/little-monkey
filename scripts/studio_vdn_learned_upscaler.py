"""Little Monkey native H3 learned latent resizer (no FreeVideo runtime).

Pure PyTorch implementation of the 3-D resizer published by LBH-123-AI,
adapted under the MIT license from
https://github.com/LBH-123-AI/Comfyui_Minimax_h3_latent_Upscaler
Copyright (c) 2026 LBH-123-AI; see licenses/h3-latent-upscaler-MIT.txt.

Weights are NOT included: opt-in native two-pass downloads an exact pinned
upscaler checkpoint from its original publisher after the H3 license gate.
No torch, HF Hub, or GPU initialization occurs until inference is requested.
"""
import hashlib
from pathlib import Path

UPSCALER_REPO = "LBH-123-AI/Minimax_h3_latent_Upscaler"
UPSCALER_REVISION = "3f941d5d182014dd5c0a5e16330420ee2d4aa0c6"
UPSCALER_FILE = ("minimax_h3_latent_upscaler_3d_conv_v1/"
                 "minimax_h3_latent_upscaler_3d_conv_v1_fp16.safetensors")
UPSCALER_BYTES = 690592672
UPSCALER_SHA256 = "043e5a48e161610ef6c3ea974645220354d06fa618abca15f76d084812eb55c2"

# The original H3 24-channel training normalization is applied AGAIN to the
# latents returned by Diffusers; omitting this causes coloured grid artifacts.
LATENTS_MEAN = (0.858090341091156, -0.9606591463088989, 1.0661640167236328,
 -0.5090325474739075, -0.2727581858634949, -1.3675414323806763,
 -0.2553254961967468, -0.26907554268836975, -0.5376840829849243,
 -0.0464097298681736, 0.6657370328903198, 0.19690127670764923,
 -0.5460608005523682, -0.4035342037677765, -0.23683024942874908,
 0.25928452610969543, -0.30133944749832153, 0.211341992020607,
 -1.1206848621368408, 0.3581933379173279, -0.04225143790245056,
 0.2604829967021942, 0.22864092886447906, 0.7056031823158264)
LATENTS_STD = (1.2223774194717407, 1.2767263650894165, 1.6831774711608887,
 1.7549455165863037, 1.5636216402053833, 2.194143533706665,
 0.9653137922286987, 1.0569885969161987, 0.841948926448822,
 0.7729952931404114, 1.8955937623977661, 0.946841835975647,
 0.7996809482574463, 0.44988900423049927, 0.7197399735450745,
 0.6936293244361877, 2.961095094680786, 2.7694199085235596,
 3.0496184825897217, 2.1088054180145264, 3.276226282119751,
 3.1627357006073, 2.2816812992095947, 2.6127843856811523)


def validate_learned_latents(shape, target=(48, 84)):
    if (len(shape) != 5 or shape[0:2] != (1, 24) or
            not 1 <= shape[2] <= 102 or len(target) != 2 or
            not all(type(n) is int and n > 0 for n in target)):
        raise ValueError("Expected bounded 1 x 24 x T x H x W H3 video latents")
    h, w = shape[-2:]
    if h < 1 or w < 1 or target[0] < h or target[1] < w:
        raise ValueError("Learned H3 upscaler cannot downscale")
    if target[0] * w != target[1] * h:
        raise ValueError("Learned H3 upscaler requires the source aspect ratio")
    factor = target[0] / h
    if not 1 < factor <= 4 or factor != target[1] / w:
        raise ValueError("Learned H3 upscaler supports 1x < scale <= 4x")
    return factor


def verify_upscaler_file(path):
    file = Path(path)
    if not file.is_file() or file.stat().st_size != UPSCALER_BYTES:
        raise ValueError("H3 learned upscaler checkpoint missing or truncated")
    with file.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if digest != UPSCALER_SHA256:
        raise ValueError("H3 learned upscaler checkpoint SHA256 mismatch")
    return file


def resolve_upscaler_checkpoint(cache_dir):
    """Pin weights and verify every byte, including already cached files."""
    from huggingface_hub import hf_hub_download
    cache = Path(cache_dir).resolve(strict=True)
    if not cache.is_dir():
        raise ValueError("App-owned upscaler cache is missing")
    path = Path(hf_hub_download(repo_id=UPSCALER_REPO, revision=UPSCALER_REVISION,
                                filename=UPSCALER_FILE, cache_dir=str(cache)))
    if not path.resolve().is_relative_to(cache):
        raise ValueError("Upscaler checkpoint escaped the app-owned model cache")
    return verify_upscaler_file(path)


def make_h3_resizer(torch, *, channels=512, in_blocks=12, out_blocks=12,
                    temporal_every=2, temporal_kernel=5):
    """Independent torch architecture with the original strict weight keys.

    Small shapes are permitted for CPU-only numerical tests; inference uses
    the released fixed 512-channel / 12+12-block architecture.
    """
    nn = torch.nn
    F = torch.nn.functional

    def norm():
        return nn.GroupNorm(32, channels)

    class ResBlockEmb3D(nn.Module):
        def __init__(self):
            super().__init__()
            self.in_layers = nn.Sequential(norm(), nn.SiLU(),
                                           nn.Conv3d(channels, channels, 3, padding=1))
            self.emb_layers = nn.Sequential(nn.SiLU(), nn.Linear(64, 2 * channels))
            self.out_norm = norm()
            self.out_layers = nn.Sequential(nn.SiLU(), nn.Dropout(p=0.1),
                                             nn.Conv3d(channels, channels, 3, padding=1))
            self.skip = nn.Identity()

        def forward(self, x, emb):
            h = self.in_layers(x)
            scale, shift = self.emb_layers(emb).to(dtype=h.dtype).chunk(2, dim=1)
            h = self.out_norm(h) * (1 + scale[..., None, None, None]) + shift[..., None, None, None]
            return self.skip(x) + self.out_layers(h)

    class TemporalConv(nn.Module):
        def __init__(self):
            super().__init__()
            self.norm = norm()
            self.dwconv = nn.Conv3d(channels, channels, (temporal_kernel, 1, 1),
                                    padding=(temporal_kernel // 2, 0, 0), groups=channels)
            self.pwconv = nn.Conv3d(channels, channels, 1)

        def forward(self, x):
            return x + self.pwconv(self.dwconv(F.silu(self.norm(x))))

    class LatentResizer3D(nn.Module):
        def __init__(self):
            super().__init__()
            self.conv_in = nn.Conv3d(24, channels, 3, padding=1)
            self.embed = nn.Sequential(nn.Linear(1, 64), nn.SiLU(), nn.Linear(64, 64))
            def stage(count):
                blocks = nn.ModuleList()
                for i in range(count):
                    blocks.append(ResBlockEmb3D())
                    if temporal_every and i % temporal_every == 0:
                        blocks.append(TemporalConv())
                return blocks
            self.in_blocks = stage(in_blocks)
            self.out_blocks = stage(out_blocks)
            self.norm_out = norm()
            self.conv_out = nn.Conv3d(channels, 24, 3, padding=1)

        def _forward_segment(self, x, scale, size):
            emb = self.embed(x.new_tensor([[scale - 1]])).expand(x.shape[0], -1)
            h = self.conv_in(x)
            for block in self.in_blocks:
                h = block(h, emb) if isinstance(block, ResBlockEmb3D) else block(h)
            h = F.interpolate(h, size=size, mode="trilinear", align_corners=False)
            for block in self.out_blocks:
                h = block(h, emb) if isinstance(block, ResBlockEmb3D) else block(h)
            return self.conv_out(F.silu(self.norm_out(h)))

        def forward(self, x, *, scale, target_size, temporal_chunk=8):
            if x.shape[-3:] == target_size:
                return x
            frames = x.shape[2]
            if frames <= temporal_chunk:
                return self._forward_segment(x, scale, target_size)
            # Overlap is required because the learned temporal convolutions
            # have nonzero temporal receptive fields. Blend every seam.
            overlap = temporal_kernel
            padded = F.pad(x, (0, 0, 0, 0, overlap, overlap), mode="replicate")
            output = x.new_zeros((x.shape[0], x.shape[1], frames,
                                  target_size[-2], target_size[-1]))
            weights = x.new_zeros((1, 1, frames, 1, 1))
            for start in range(0, frames, temporal_chunk):
                end = min(frames, start + temporal_chunk)
                outer_start = max(0, start - overlap)
                outer_end = min(frames, end + overlap)
                lo = max(0, outer_start - overlap)
                hi = min(frames + 2 * overlap, outer_end + overlap)
                segment = padded[:, :, lo:hi].contiguous()
                refined = self._forward_segment(segment, scale,
                    (hi - lo, target_size[-2], target_size[-1]))
                view = refined[:, :, outer_start + overlap - lo:
                                outer_start + overlap - lo + outer_end - outer_start]
                weight = x.new_ones((outer_end - outer_start,))
                if outer_start < start:
                    amount = start - outer_start
                    weight[:amount] = torch.arange(1, amount + 1, device=x.device,
                        dtype=x.dtype) / (amount + 1)
                if outer_end > end:
                    amount = outer_end - end
                    weight[-amount:] = torch.arange(amount, 0, -1, device=x.device,
                        dtype=x.dtype) / (amount + 1)
                output[:, :, outer_start:outer_end] += view * weight.view(1, 1, -1, 1, 1)
                weights[:, :, outer_start:outer_end] += weight.view(1, 1, -1, 1, 1)
                del segment, refined, view
            return output / weights.clamp_min(1e-8)

    return LatentResizer3D()


def native_learned_upscale(video, cache_dir, target=(48, 84)):
    """Real trained 3-D upscale of normalized H3 video latents.

    Strict checkpoint, no interpolation-only fallback and no FreeVideo process.
    Load to CUDA from CPU safetensors, release before denoising the second pass.
    """
    import gc
    import torch
    from safetensors import safe_open

    factor = validate_learned_latents(tuple(video.shape), target)
    if not video.is_floating_point() or not bool(torch.isfinite(video).all()):
        raise ValueError("H3 learned upscaler needs finite floating-point latents")
    checkpoint = resolve_upscaler_checkpoint(cache_dir)
    dtype = torch.float16 if video.device.type == "cuda" else torch.float32
    model = None
    try:
        with torch.device("meta"):
            model = make_h3_resizer(torch)
        with safe_open(str(checkpoint), framework="pt", device="cpu") as handle:
            params = {key.removeprefix("upscaler."): handle.get_tensor(key).to(
                device=video.device, dtype=dtype) for key in handle.keys()}
        model.load_state_dict(params, strict=True, assign=True)
        del params
        model.eval().requires_grad_(False)
        mean = video.new_tensor(LATENTS_MEAN, dtype=dtype).view(1, 24, 1, 1, 1)
        std = video.new_tensor(LATENTS_STD, dtype=dtype).view(1, 24, 1, 1, 1)
        with torch.inference_mode():
            lifted = model((video.to(dtype=dtype) - mean) / std,
                scale=factor, target_size=(video.shape[2], *target)).to(torch.float32)
            lifted = lifted * std.float() + mean.float()
        if tuple(lifted.shape) != (1, 24, video.shape[2], *target):
            raise RuntimeError("H3 learned upscaler returned an incorrect tensor shape")
        if not bool(torch.isfinite(lifted).all()):
            raise RuntimeError("H3 learned upscaler returned nonfinite latents")
        return lifted
    finally:
        del model
        gc.collect()
        if video.device.type == "cuda":
            torch.cuda.empty_cache()
