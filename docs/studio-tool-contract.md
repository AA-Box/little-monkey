# The Studio tool contract

Studio tools are the app's extension tier: face swap, detectors, segmenters,
background removal — operations that are not diffusion and will never arrive by
pinning a newer engine, because they are different programs.

A tool is **a separate executable, not a plugin**. The app spawns it, talks to
it over loopback, and can kill it. It is the same boundary VS Code draws around
its extension host and browsers draw around extensions, and it is drawn here for
three reasons that are not negotiable: this app is signed and notarized, it
holds keychain items, and it enforces an egress policy. Third-party code inside
that process defeats all three at once, and on macOS unsigned native code will
not load into a hardened runtime at all.

The consequence people ask about: you cannot install an Automatic1111-style
extension here, because those are Python files a Python app imports and executes
in-process. There is no interpreter in this binary to run them, and the
generation pipeline they would hook is a compiled C++ process rather than Python
objects. What you get instead is everything on this page — one-click install of
a verified tool, whose UI appears without an app release.

## Launch

```
<binary> --host 127.0.0.1 --port <port>
```

The port is reserved by the app and handed over on the command line. Bind
loopback only. Both stdout and stderr are drained by the app; the tail of them
is what a user sees when a launch fails, so log the reason a start failed.

The app runs one tool at a time and leaves it running between runs so a model is
not reloaded per operation. It is killed on tool switch, on "Release memory",
and on app exit.

## `GET /tool/v1/manifest`

Answer once you are ready to serve runs — the app polls this until it succeeds
and treats the first success as "loaded". You have 120 seconds.

```json
{
  "schemaVersion": 1,
  "id": "face-swap",
  "name": "Face Swap",
  "description": "Replace a face in the target with the one in the source.",
  "inputs": [
    { "key": "source", "label": "Source face", "kind": "image", "required": true },
    { "key": "target", "label": "Target image", "kind": "image", "required": true },
    { "key": "strength", "label": "Blend", "kind": "number",
      "min": 0, "max": 1, "step": 0.05, "default": 0.8 },
    { "key": "restorer", "label": "Face restorer", "kind": "choice",
      "options": [{ "value": "none", "label": "None" },
                  { "value": "gfpgan", "label": "GFPGAN" }] },
    { "key": "upscale", "label": "Upscale result", "kind": "toggle", "default": false }
  ]
}
```

**The manifest is your UI.** Studio draws a form from it and knows nothing else
about your tool — that indirection is the whole design, and it is why a new tool
is a download rather than a release of this app.

Input kinds: `image` (base64, no data-URL prefix), `text`, `number`, `toggle`,
`choice`. `min`/`max`/`step` apply to `number`; `options` is required on
`choice` and rejected on everything else. A `default` is type-checked against
its own kind and range.

Limits, all enforced before anything is rendered: 32 inputs, 4 of them images,
64 options per choice, 256 KiB of manifest.

## `POST /tool/v1/run`

```json
{ "inputs": { "source": "<base64>", "target": "<base64>", "strength": 0.8 } }
```

Only keys you declared are ever sent, required ones are always present, numbers
are inside your declared range and choices are one of your options — the app
validates against the manifest *you served this session*, so a tool that changed
underneath the UI gets the stale field rejected rather than handed to it.

Answer with:

```json
{ "media": [{ "mediaType": "image/png", "dataBase64": "<base64>" }] }
```

`mediaType` must be one of `image/png`, `image/jpeg`, `image/webp`, `video/mp4`,
`audio/wav`. At most 8 items, 64 MiB total, 300 seconds. Results are filed in
the Studio gallery.

On failure, answer a non-2xx with `{"error": "no face found in the source
image"}`. That sentence is shown to the user; without it every refusal reads as
a bare status code.

## Reference tool

`examples/studio-tool-echo.mjs` is a complete, dependency-free tool. It has no
useful effect — it returns the image you gave it — but it exercises the whole
path, and it is the shortest way to see the tier work:

```bash
node examples/studio-tool-echo.mjs --host 127.0.0.1 --port 9099
```

In Studio → Tools → **Add your own binary**, pick the script (make it executable
first, or point at a wrapper that runs `node`). Its card appears, built entirely
from the manifest above.

## Getting a tool installed for real

Tools published by this project install through the Runtime Hub's component
path, unchanged: a registry entry of kind `studio_tool` is downloaded, checked
against its declared SHA-256, versioned and made rollback-able before it is ever
run. That is the **Verified** badge in the tools list.

A binary you point at yourself is labelled **Your own** and is not checked by
anything — it is allowed for the same reason a weight file from your own disk is
allowed, and it is labelled so the two are never confused.
