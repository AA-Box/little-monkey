# GHOST face-swap Studio tool

`studio-tools/face-swap/studio-tool-face-swap` is a local-only Studio sidecar. Its default
pipeline is:

```text
InsightFace detection/embedding → GHOST 3_256 → optional CodeFormer
```

No weights are stored in this repository. After the user acknowledges the
non-commercial notice, the sidecar downloads pinned public model assets to the
app's private Studio-tool data directory and verifies their SHA-256 hashes.
The acknowledgement does not grant a model license; users remain responsible
for consent, privacy, and applicable biometric requirements.

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

The default first run downloads `buffalo_l`, GHOST 3_256, and the GHOST
embedding converter automatically. Published Studio builds are discovered from
the signed face-swap catalog automatically; the local launcher remains the
fallback for development and offline installs. To use a separately licensed/custom pack,
provide the model root yourself:

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

The public InsightFace model packs are not commercially cleared; the SDK license
and model license are separate. `FACE_SWAP_FACE_PACK` defaults to `buffalo_l`.
`FACE_SWAP_PROVIDER=cpu` is the safest default. Set
`FACE_SWAP_DISABLE_AUTO_DOWNLOAD=1` to require all model files to already exist.

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

After the published catalog refreshes, open **Studio → Tools** and click **Install**
on Face Swap. You do not need to add a binary manually. **Add your own binary**
and **Import catalog** remain available for offline/self-hosted installs:

```text
studio-tools/face-swap/studio-tool-face-swap
```

Every run requires the user to confirm they may use all supplied models. This
is an acknowledgement gate, not a substitute for a license.

## Optional CodeFormer restoration

CodeFormer is opt-in because its project license is not a blanket commercial
license. Here, “source” means the CodeFormer program checkout — not the input
image. When the user selects CodeFormer in Studio, its optional PyTorch
dependencies, official source checkout, and trained checkpoint (`codeformer.pth`)
are downloaded and installed automatically into the same private environment.
The source and checkpoint are pinned and SHA-256 verified. To use separately
licensed/custom files instead, provide:

```sh
export FACE_SWAP_CODEFORMER_HOME=/absolute/path/to/pinned/CodeFormer
export FACE_SWAP_CODEFORMER_MODEL=/absolute/path/to/model-root/models/codeformer.pth
export FACE_SWAP_CODEFORMER_SHA256=<codeformer-sha256>
```

The Studio restorer choice is **CodeFormer**. It remains non-commercial under
its upstream S-Lab license, and automatic download happens only after the
non-commercial acknowledgement.

## Build a managed one-file tool

This is a publisher/build step, not a model downloader. It embeds only the
assets explicitly supplied to the builder:

```sh
export FACE_SWAP_MODEL_ROOT=/absolute/path/to/licensed-model-root
export FACE_SWAP_MODEL_LICENSE_FILE=/absolute/path/to/model-license.txt
export FACE_SWAP_PYTHON=/absolute/path/to/python-with-face-swap-dependencies
export FACE_SWAP_DOWNLOAD_URL=https://downloads.example.com/face-swap-ghost-3-256
export FACE_SWAP_SIGNING_KEY="$(cat /secure/path/to/ed25519-private-key.pem)"
pnpm face-swap:package
```

For an optional CodeFormer-enabled build, also set:

```sh
export FACE_SWAP_CODEFORMER_HOME=/absolute/path/to/pinned/CodeFormer
export FACE_SWAP_CODEFORMER_MODEL=$FACE_SWAP_MODEL_ROOT/models/codeformer.pth
export FACE_SWAP_CODEFORMER_LICENSE_FILE=/absolute/path/to/codeformer-license.txt
```

The builder requires model-license evidence, records SHA-256 metadata for the
embedded models, signs the catalog with the pinned release key, and writes the
executable to `packaging/face-swap/dist/`. The production workflow publishes
platform binaries plus a rolling signed catalog; Studio refreshes that catalog
automatically after the app release contains the signed-key verifier.

References: [GHOST](https://github.com/ai-forever/ghost), [FaceFusion GHOST
definitions](https://github.com/facefusion/facefusion/blob/master/facefusion/processors/modules/face_swapper/core.py),
[CodeFormer license](https://github.com/sczhou/CodeFormer/blob/master/LICENSE),
[InsightFace licensing](https://github.com/deepinsight/insightface/blob/master/server/LICENSING.md).
