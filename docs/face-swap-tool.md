# GHOST face-swap Studio tool

`examples/studio-tool-face-swap` is a local-only Studio sidecar. Its default
pipeline is:

```text
InsightFace detection/embedding → GHOST 3_256 → optional CodeFormer
```

No weights are stored in this repository and the sidecar never downloads them.
The Studio checkbox records the user's acknowledgement; it does not grant a
model license. The user/build publisher must verify the exact checkpoint terms,
consent, privacy, and applicable biometric requirements.

## Automatic runtime setup

Open the tool through Studio and the launcher automatically creates a private
environment under the app's per-user Studio-tool data directory and installs
the bundled base requirements on first launch. It refreshes the environment
only when the requirements file changes; the repository and installed tool
directory stay read-only. The only prerequisite is Python 3.10–3.12 being
available on the machine; set `FACE_SWAP_SYSTEM_PYTHON` when it is not on the
default `PATH`.

The launcher prints setup progress to the Studio tool log. No terminal command
or manual `pip install` step is required. `FACE_SWAP_VENV` remains available
for a deliberate test or packaging override; it is not the production default.

Provide the model root yourself:

```sh
export FACE_SWAP_MODEL_ROOT=/absolute/path/to/licensed-model-root
```

Required files:

```text
$FACE_SWAP_MODEL_ROOT/models/buffalo_l/det_10g.onnx
$FACE_SWAP_MODEL_ROOT/models/buffalo_l/w600k_r50.onnx
$FACE_SWAP_MODEL_ROOT/models/buffalo_l/genderage.onnx
$FACE_SWAP_MODEL_ROOT/models/ghost_3_256.onnx
$FACE_SWAP_MODEL_ROOT/models/crossface_ghost.onnx
```

The public InsightFace model packs are not automatically commercially cleared;
the SDK license and model license are separate. `FACE_SWAP_FACE_PACK` defaults
to `buffalo_l`. `FACE_SWAP_PROVIDER=cpu` is the safest default.

This tool is explicitly **non-commercial use only** when using the public
InsightFace model pack. Studio displays that warning on the available-tool card
and again on the selected tool page before a run. Do not use it for paid,
business, client, advertising, or other commercial work without separate
written permission for every model involved.

To require exact hashes before inference, obtain trusted release hashes and set:

```sh
export FACE_SWAP_REQUIRE_MODEL_HASH=1
export FACE_SWAP_DETECTOR_SHA256=<det-10g-sha256>
export FACE_SWAP_RECOGNITION_SHA256=<w600k-r50-sha256>
export FACE_SWAP_GENDERAGE_SHA256=<genderage-sha256>
export FACE_SWAP_MODEL_SHA256=<ghost-model-sha256>
export FACE_SWAP_EMBEDDING_CONVERTER_SHA256=<converter-sha256>
```

The values above are placeholders and must not be copied literally. The tool
also accepts `FACE_SWAP_MODEL` for a local GHOST 3_256 model path.

Open **Studio → Tools → Add your own binary** and select:

```text
examples/studio-tool-face-swap
```

Every run requires the user to confirm they may use all supplied models. This
is an acknowledgement gate, not a substitute for a license.

## Optional CodeFormer restoration

CodeFormer is opt-in because its project license is not a blanket commercial
license. When the user selects CodeFormer in Studio, its optional PyTorch
dependencies are downloaded and installed automatically into the same private
environment. The official CodeFormer checkout and checkpoint must still be
supplied locally:

```sh
export FACE_SWAP_CODEFORMER_HOME=/absolute/path/to/pinned/CodeFormer
export FACE_SWAP_CODEFORMER_MODEL=/absolute/path/to/model-root/models/codeformer.pth
export FACE_SWAP_CODEFORMER_SHA256=<codeformer-sha256>
```

The Studio restorer choice is **CodeFormer (user-supplied)**. The official
CodeFormer checkout and `codeformer.pth` must be supplied locally; only the
Python dependencies are downloaded automatically.

## Build a managed one-file tool

This is a publisher/build step, not a model downloader. It embeds only the
assets explicitly supplied to the builder:

```sh
export FACE_SWAP_MODEL_ROOT=/absolute/path/to/licensed-model-root
export FACE_SWAP_MODEL_LICENSE_FILE=/absolute/path/to/model-license.txt
export FACE_SWAP_PYTHON=/absolute/path/to/python-with-face-swap-dependencies
export FACE_SWAP_DOWNLOAD_URL=https://downloads.example.com/face-swap-ghost-3-256
pnpm face-swap:package
```

For an optional CodeFormer-enabled build, also set:

```sh
export FACE_SWAP_CODEFORMER_HOME=/absolute/path/to/pinned/CodeFormer
export FACE_SWAP_CODEFORMER_MODEL=$FACE_SWAP_MODEL_ROOT/models/codeformer.pth
export FACE_SWAP_CODEFORMER_LICENSE_FILE=/absolute/path/to/codeformer-license.txt
```

The builder requires model-license evidence, records SHA-256 metadata for the
embedded models, and writes the executable to
`packaging/face-swap/dist/`. The catalog is written only for an HTTPS URL.

References: [GHOST](https://github.com/ai-forever/ghost), [FaceFusion GHOST
definitions](https://github.com/facefusion/facefusion/blob/master/facefusion/processors/modules/face_swapper/core.py),
[CodeFormer license](https://github.com/sczhou/CodeFormer/blob/master/LICENSE),
[InsightFace licensing](https://github.com/deepinsight/insightface/blob/master/server/LICENSING.md).
