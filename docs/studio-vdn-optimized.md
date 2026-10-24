# Accelerated VDN-H3 in Little Monkey Studio

This is a stacked follow-on to [#629](https://github.com/AA-Box/little-monkey/pull/629):
the original portable Diffusers backend remains, while Studio now provides
**three independent actual inference paths**. The model's MiniMax H3 regional
license restrictions apply identically to every path. Neither changing the
backend nor accessing an external server constitutes permission to use weights.

| Backend | Entry point | Hardware | Speed characteristics |
| --- | --- | --- | --- |
| Portable | Official ModularPipeline | CUDA 1 GPU; block offload | Reliable baseline; no tuned OpenVDN kernel claim |
| Optimized single | `src/inference/infer.py` | Linux, a high-memory CUDA GPU | Real OpenVDN kernels and optional tuned FP8 config |
| Ulysses | `src/inference/infer_ulysses.py` using `torch.distributed.run` | Linux, exactly 8 H200 or 8 B200 | Actual OpenVDN 8-way sequence/branch-parallel runtime |
| SGLang | Locally running `sglang serve` HTTP `/v1/videos` | 1, 2, 4 or 8 supported server-side GPUs | Native SGLang acceleration, asynchronous video job API |

## Optimized single GPU

Under an independently authorized account/territory, install the original
[OpenVDN inference project](https://github.com/OpenVDN/vdn-minimax-h3),
its optimized dependencies, and the **real 8-step** checkpoint in the
`ckpts/` folder. The published full download is approximately 82 GB
and includes MiniMax-H3 base, VDN linear branch and adapter weights.
Do not download, distribute or use weights in an excluded territory without
separate MiniMax authorization.

Open **Studio → Video → VDN** and select **OpenVDN tuned kernels**. Choose
the Python in that repository's configured CUDA environment, the upstream
source checkout and the existing `ckpts` folder. Press **Check installed VDN
runtime**, which imports real PyTorch, checks CUDA and the upstream
scripts/checkpoint layout. The encoder is chosen automatically:
`encode_prompt.py` for T2VA, `encode_keyframes.py` for first/last images.
Then `infer.py` receives the precomputed prompt-cache path and the real
`8nfe_tuned_fp8.yaml` or `8nfe_tuned.yaml` with a checkpoint override
pointing to `stage-dmd-step-250`. The trained model runs the official
attention implementation; it is **not** the Diffusers fallback.

## Eight GPU Ulysses

Choose **OpenVDN Ulysses**. The preflight requires eight *visible matching*
H200 or B200 CUDA GPUs, upstream OpenVDN source and weights. It selects
`8nfe_tuned_fp8_ulysses_h200.yaml` or `8nfe_tuned_fp8_ulysses_b200.yaml`
and starts the genuine `src/inference/infer_ulysses.py` through
`python -m torch.distributed.run --standalone --nproc_per_node=8`.
This is the tuned multi-rank attention model, not eight copies of the
single-GPU job. Torchrun children run in an independent process group;
cancel/timeout/revocation requests are passed to the worker and its group.

The large fp8 model is designed for high-memory datacenter cards. No
single-GPU 8 GiB claim applies to these kernels; that is a different
streaming implementation.

## SGLang Diffusion accelerated local server

Install the released `sglang[diffusion]` package on a properly configured
GPU machine (see the [authoritative MiniMax-H3 cookbook](https://github.com/sgl-project/sglang/blob/main/docs/cookbook/diffusion/MiniMax/MiniMax-H3.mdx)).
Example for eight B200s **only after permission to use VDN-H3 weights**:

```bash
sglang serve --model-path OpenVDN/vdn-minimax-h3 \
  --num-gpus 8 --quantization fp8 \
  --attention-backend hybrid_window_attn_h3 \
  --performance-mode speed --port 30010
```

For this backend, choose Python (to launch the worker), select SGLang, set
the local port, and check readiness. The worker requests only
`127.0.0.1:port`, verifies `GET /v1/models` explicitly lists
`OpenVDN/vdn-minimax-h3`, posts `POST /v1/videos` with T2VA/FL2VA
conditions and 8-step sampling, polls `GET /v1/videos/{id}`, then streams
`GET /v1/videos/{id}/content` to an app-owned file (bounded 256 MB). It
rejects HTTP redirects and arbitrary remote origins and lets the Rust
backend validate/store the MP4 in Studio Gallery.

SGLang's `DELETE /v1/videos/{id}` is invoked best-effort when canceling,
but some SGLang versions **do not actually abort a running GPU computation**.
In that case, Little Monkey cancels its local wait and never publishes the
video; the remote GPU job may continue. Avoid claiming fully deterministic
GPU-work cancellation on SGLang until the server itself guarantees it.

## Security, performance and CI

The Rust backend enforces the same MiniMax territorial policy before
spawning any worker, every three seconds while it runs, and before Gallery
publication. Source and checkpoint folders are canonicalized and verified,
the upstream commands are argument vectors (no shell injection), and the
fast path supports only a fixed set of reviewed upstream scripts and configs.
Python inference code and Hugging Face checkpoints are still third-party
software; the user explicitly consents before executing them.

CI runs subprocess-based real command-planning checks for both native
runtimes, including first/last-frame encoding and torchrun arguments. It
also performs a real HTTP submit/poll/MP4-download against a local test
SGLang server and verifies bad configurations are rejected. This validates
routing, argument construction, SGLang protocol and output handling
**without** claiming a live-model acceptance test. An authorized GPU
operator must still benchmark the same actual prompt, seed and output
settings on the appropriate hardware, collect GPU peak memory and
render-time results, validate audio/video playback and compare generated
quality before claiming speed or quality parity with OpenVDN reports.
