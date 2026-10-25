"""Little Monkey native exact-context low-memory execution for MiniMax-H3.

First-party processor: tile queries, attention heads, Q/K/V projections, and
feed-forward tokens while keeping *all* key/value positions available to
each query. No FreeVideo dependency, no truncated temporal context.
"""
import json
import types


def tile_policy(tokens, heads, dim, free_bytes, *, query_override=0, heads_override=0):
    """Limit attention temporary buffers against the measured CUDA headroom."""
    if min(tokens, heads, dim) <= 0 or free_bytes <= 0:
        raise ValueError("Invalid native H3 attention geometry/resources")
    if query_override < 0 or heads_override < 0:
        raise ValueError("Negative H3 chunk size")
    scratch = min(256 * 1024**2, max(4 * 1024**2, int(free_bytes * .15)))
    h = min(heads, max(1, scratch // (2 * tokens * dim * 4)), 8)
    if heads_override:
        h = min(h, heads_override)
    q = max(1, min(tokens, 2048, scratch // max(1, 8 * h * dim)))
    if query_override:
        q = min(q, query_override)
    return q, h


def weight_stream_policy(free_cuda, free_ram, *, disk_spill=False):
    """Budget genuine Diffusers block streaming and async weight prefetch.

    Prefetch is beneficial only when the model can keep two transfer slots in
    VRAM plus host-pinned pages. Low-VRAM/low-RAM machines use sequential
    transfers instead of causing a secondary OOM inside the offload hooks.
    """
    if free_cuda < 0 or free_ram is not None and free_ram < 0:
        raise ValueError("Negative native streaming budget")
    gib = 1024**3
    cpu_ok = free_ram is not None and free_ram >= 64 * gib
    cuda_ok = free_cuda >= 14 * gib
    use_prefetch = bool(cuda_ok and cpu_ok and not disk_spill)
    return {
        # Diffusers recommends 1 block per group whenever CUDA streams
        # prefetch the NEXT group; larger groups are only safe for sequential
        # transfers, when enough VRAM is actually free.
        "denoiser_blocks_per_group": 1 if use_prefetch or free_cuda < 32 * gib else 2,
        "denoiser_async_prefetch": use_prefetch,
        "text_async_prefetch": use_prefetch,
        "disk_spill": bool(disk_spill),
    }


def _plain_linear_rows(layer, x, start, stop):
    import torch
    import torch.nn.functional as functional
    if not isinstance(layer, torch.nn.Linear) or layer.weight.layout != torch.strided:
        raise TypeError("Projection is not a standard strided torch.nn.Linear")
    return functional.linear(x, layer.weight[start:stop],
                             layer.bias[start:stop] if layer.bias is not None else None)


def _is_plain_linear(layer):
    import torch
    return isinstance(layer, torch.nn.Linear) and layer.weight.layout == torch.strided


def native_h3_attention(attn, hidden_states, rotary_emb=None, attention_mask=None,
                        *, query_chunk=0, head_chunk=0, budget_bytes=None,
                        fallback=None):
    """Full-context attention with bounded head-group keys/values and query tiles.

    Matches H3 q/k norm, full attention and projections. Nonstandard quantized
    layers or masks use original processor rather than a silent approximation.
    """
    import torch
    import torch.nn.functional as functional
    from diffusers.models.transformers.transformer_minimax_h3 import _apply_rotary_emb

    if hidden_states.ndim != 3 or hidden_states.shape[0] != 1 or attention_mask is not None:
        if fallback is None:
            raise ValueError("Unsupported H3 attention batch/mask")
        return fallback(attn, hidden_states, rotary_emb, attention_mask)
    batch, tokens, _ = hidden_states.shape
    heads, dim = int(attn.heads), int(attn.head_dim)
    fused = getattr(attn, "fused_projections", False)
    if fused:
        projection = attn.to_qkv
        supported = _is_plain_linear(projection)
    else:
        supported = all(_is_plain_linear(getattr(attn, name))
                        for name in ("to_q", "to_k", "to_v"))
    # TorchAO/quantized projections may not expose sliceable row weights.
    # Execute those modules on TOKEN tiles and collect only the active
    # attention heads. This is slower than sliced ordinary Linear but it
    # bounds the intermediate projection memory and works with INT8 layers.
    if budget_bytes is None:
        budget_bytes = (torch.cuda.mem_get_info(hidden_states.device)[0]
                        if hidden_states.is_cuda else 512 * 1024**2)
    q_chunk, h_chunk = tile_policy(tokens, heads, dim, budget_bytes,
                                   query_override=query_chunk,
                                   heads_override=head_chunk)
    merged = torch.empty((batch, tokens, heads, dim),
                         device=hidden_states.device, dtype=hidden_states.dtype)
    def project(kind, x, lo, hi):
        if supported:
            if fused:
                offset = {"q": 0, "k": heads * dim, "v": 2 * heads * dim}[kind]
                return _plain_linear_rows(projection, x, offset + lo, offset + hi)
            return _plain_linear_rows(getattr(attn, "to_" + kind), x, lo, hi)
        if fused:
            full = attn.to_qkv(x)
            offset = {"q": 0, "k": heads * dim, "v": 2 * heads * dim}[kind]
            return full[..., offset + lo:offset + hi]
        return getattr(attn, "to_" + kind)(x)[..., lo:hi]

    def kv_for_group(kind, lo, hi):
        if supported:
            return project(kind, hidden_states, lo, hi)
        # Quantized projection output is limited to a token tile, never a
        # whole 75k-token X 3*56*128 tensor. Each head group is streamed.
        pieces = []
        for offset in range(0, tokens, q_chunk):
            pieces.append(project(kind, hidden_states[:, offset:offset + q_chunk], lo, hi))
        return torch.cat(pieces, dim=1)

    for first_head in range(0, heads, h_chunk):
        last_head = min(heads, first_head + h_chunk)
        lo, hi = first_head * dim, last_head * dim
        key = kv_for_group("k", lo, hi)
        value = kv_for_group("v", lo, hi)
        key = attn.norm_k(key.unflatten(-1, (last_head-first_head, dim)))
        if rotary_emb is not None:
            key = _apply_rotary_emb(key, *rotary_emb)
        key = key.transpose(1, 2).contiguous()
        value = value.unflatten(-1, (last_head-first_head, dim)).transpose(1, 2).contiguous()
        for begin in range(0, tokens, q_chunk):
            end = min(tokens, begin + q_chunk)
            query = project("q", hidden_states[:, begin:end], lo, hi)
            query = attn.norm_q(query.unflatten(-1, (last_head-first_head, dim)))
            if rotary_emb is not None:
                query = _apply_rotary_emb(query,
                                          rotary_emb[0][begin:end],
                                          rotary_emb[1][begin:end])
            query = query.transpose(1, 2).contiguous()
            result = functional.scaled_dot_product_attention(
                query, key, value, dropout_p=0.0, is_causal=False)
            merged[:, begin:end, first_head:last_head] = result.transpose(1, 2)
        del key, value
    merged = merged.flatten(2, 3)
    # The final H3 output projection is also token-tiled.
    projected = torch.empty((batch, tokens, hidden_states.shape[-1]),
                            device=hidden_states.device, dtype=hidden_states.dtype)
    for begin in range(0, tokens, q_chunk):
        end = min(tokens, begin + q_chunk)
        projected[:, begin:end] = attn.to_out[1](
            attn.to_out[0](merged[:, begin:end]))
    return projected


class NativeH3ChunkedProcessor:
    def __init__(self, previous, query_chunk, head_chunk):
        self.previous = previous
        self.query_chunk = query_chunk
        self.head_chunk = head_chunk

    def __call__(self, attn, hidden_states, rotary_emb=None, attention_mask=None):
        return native_h3_attention(attn, hidden_states, rotary_emb, attention_mask,
                                   query_chunk=self.query_chunk, head_chunk=self.head_chunk,
                                   fallback=self.previous)


def install_native_h3_tiles(denoiser, *, query_chunk=0, head_chunk=0,
                            ff_chunk=512, verbose=True):
    """Install only on MiniMax H3 modules; untouched models retain their kernels."""
    if min(query_chunk, head_chunk, ff_chunk) < 0:
        raise ValueError("Negative native H3 tile size")
    from diffusers.models.transformers.transformer_minimax_h3 import MiniMaxH3Attention
    applied_attn = applied_ff = 0
    for module in denoiser.modules():
        if isinstance(module, MiniMaxH3Attention):
            if isinstance(module.processor, NativeH3ChunkedProcessor):
                continue
            module.set_processor(NativeH3ChunkedProcessor(
                module.processor, query_chunk, head_chunk))
            applied_attn += 1
    if ff_chunk:
        for module in denoiser.modules():
            if module.__class__.__name__ != "FeedForward":
                continue
            if getattr(module, "_little_monkey_tiled_ff", False):
                continue
            original = module.forward
            def tiled_forward(self, x, *args, _original=original, **kwargs):
                if x.ndim != 3 or x.shape[1] <= ff_chunk:
                    return _original(x, *args, **kwargs)
                import torch
                out = torch.empty_like(x)
                for offset in range(0, x.shape[1], ff_chunk):
                    end = min(offset + ff_chunk, x.shape[1])
                    out[:, offset:end] = _original(x[:, offset:end], *args, **kwargs)
                return out
            module.forward = types.MethodType(tiled_forward, module)
            module._little_monkey_tiled_ff = True
            applied_ff += 1
    if applied_attn == 0 and not any(
            isinstance(m.processor, NativeH3ChunkedProcessor)
            for m in denoiser.modules() if isinstance(m, MiniMaxH3Attention)):
        raise RuntimeError("H3 low-memory tiling found no supported attention modules")
    if verbose:
        print(json.dumps({"phase": "native_lowmem_installed",
                          "fullContextAttention": True,
                          "attentionLayers": applied_attn,
                          "feedForwardLayers": applied_ff,
                          "queryChunk": query_chunk or "adaptive",
                          "maxHeadsPerChunk": head_chunk or "adaptive",
                          "ffTokensPerChunk": ff_chunk}), flush=True)
    return applied_attn, applied_ff
