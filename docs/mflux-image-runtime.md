# MFLUX image generation

MFLUX is an Apple Silicon-only Studio engine. It is installed as its own signed Runtime Hub component, separate from the existing MLX runtime. The package contains a pinned Python environment and a supervised HTTP service; Studio never launches a one-shot image command.

## Setup

1. Open Settings → Runtime Hub → Components and install **MFLUX Image Runtime**.
2. In Studio, choose **Add model** and select **MFLUX image**.
3. Keep the default repository `black-forest-labs/FLUX.1-dev`, choose **8-bit**, and enable text-to-image and/or image-to-image.
4. If the repository requires it, enter an access token. It is stored in the OS credential store and is not written to the model JSON.
5. Save the model, read and accept the displayed model terms, then choose Download.
6. Select the installed model in Image or Edit and run a prompt. The service keeps the loaded model warm for later requests.

The repository download is staged file-by-file and receives a completion marker only after every file succeeds. A cancelled or failed transfer therefore remains unavailable to Studio.

## Supported surface

The first engine path supports text-to-image and image-to-image, with sampling progress and queued/active cancellation. Negative prompts, masks, control images, reference images, IP-Adapter, LoRA, and hires passes are rejected or hidden because they are not wired through this service.

## Manual verification checklist

On an Apple Silicon macOS machine with valid access to the model source:

1. Install the runtime from Runtime Hub and confirm its version and installed state.
2. Add `black-forest-labs/FLUX.1-dev` with 8-bit quantization.
3. Enter a token if required, accept the displayed terms, and download the model.
4. Generate a text-to-image result; verify step progress, cancellation, and a gallery entry.
5. Generate a second image with the same model; verify the service process remains running and the model is not constructed again.
6. Queue a second request while one is active; cancel it and verify it reaches `cancelled` without producing an artifact.
7. Generate an image-to-image result and verify the input image reaches the service.

Automated coverage is provided by `pnpm test:mflux-service`, the Rust generation tests, and the TypeScript compiler. A full gated-model download and real-device generation require credentials and model weights, so they are release-acceptance checks rather than CI tests.
