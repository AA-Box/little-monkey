/**
 * Studio — image and video generation over the managed stable-diffusion.cpp
 * runtime. English source of truth for the `Studio.*` / `App.section.*` key
 * namespace, spread into every other locale file and overridden there with a
 * real translation, like every other feature slice in this directory.
 */
export const studioLocale: Record<string, string> = {
  "App.section.switcher": "Section",
  "App.section.chat": "Chat",
  "App.section.studio": "Studio",

  "Studio.tab.models": "Models",
  "Studio.tab.image": "Image",
  "Studio.tab.video": "Video",
  "Studio.tab.audio": "Audio",
  "Studio.audio.title": "Audio",
  "Studio.audio.subtitle":
    "Speak text in a chosen voice, or clone one from a recording. Nothing leaves this machine.",
  "Studio.mlx.preparing": "Preparing the MLX video service…",
  "Studio.mlx.notReady": "The MLX video service is not ready yet.",
  "Studio.mlx.retry": "Retry MLX setup",
  "Studio.add.speechHint":
    "A speech model is its backbone on --model plus its projector on --mmproj.",
  "Studio.task.text_to_speech": "Text to speech",
  "Studio.models.title": "Models",
  "Studio.models.subtitle":
    "Your library. Nothing is preinstalled — add files you already have, or point at a repo to download from.",
  "Studio.image.title": "Image",
  "Studio.image.subtitle": "Generate images on this machine. No account, no upload.",
  "Studio.video.title": "Video",
  "Studio.video.subtitle": "Generate video on this machine. No account, no upload.",
  "Studio.models": "Models",

  // Control-image preprocessors. ControlNet wants a hint map, not a
  // photograph; before this the user had to make one elsewhere.
  // The prompt fields had no label at all — a placeholder is not one, because
  // it vanishes on the first keystroke.
  "Studio.prompt": "Prompt",
  "Studio.negativePrompt": "Negative prompt",
  "Studio.speechText": "Text to speak",
  // Weighting is parsed by the engine itself, not by this app.
  "Studio.promptWeighting":
    "Weight a word with (word:1.3) to strengthen it, or (word:0.6) to weaken it. 1.0 is neutral.",

  // Outpainting. Presented as extending rather than as a canvas, because that
  // is what the buttons do: each press grows one side and asks the model to
  // fill it.
  "Studio.mask.zoomIn": "Zoom in",
  "Studio.mask.zoomOut": "Zoom out",

  "Studio.outpaint.title": "Extend the picture",
  "Studio.outpaint.hint":
    "Grows the image by the chosen number of pixels on one side and fills the new space to match. Press again to keep going. The seam is blended by letting the model repaint a thin strip of the existing picture.",
  "Studio.outpaint.left": "Extend to the left",
  "Studio.outpaint.right": "Extend to the right",
  "Studio.outpaint.up": "Extend upwards",
  "Studio.outpaint.down": "Extend downwards",
  "Studio.outpaint.undo": "Undo the last extension",
  "Studio.outpaint.redo": "Redo the undone extension",

  "Studio.preprocess.label": "Turn this photo into a hint map",
  "Studio.preprocess.placeholder": "Leave as it is",
  "Studio.preprocess.canny": "Edges (Canny)",
  "Studio.preprocess.grayscale": "Greyscale",
  "Studio.preprocess.invert": "Invert",

  // Tools — the sidecar tier. A tool is a separate program, not a model and
  // not part of the engine, and the copy says so: what the user installs here
  // runs beside the app rather than inside it.
  "Studio.tab.tools": "Tools",
  "Studio.tools.title": "Tools",
  "Studio.tools.subtitle":
    "Extra operations that are not generation — face swap, detectors, segmenters. Each runs as its own program beside the app, never as code inside it.",
  "Studio.tools.library": "Installed",
  "Studio.tools.available": "Available",
  "Studio.tools.empty":
    "No tools installed. Install a published one below, or point at a binary you already have.",
  "Studio.tools.noneAvailable":
    "No tools are published for this app version yet.",
  "Studio.tools.install": "Install",
  "Studio.tools.installing": "Installing…",
  "Studio.tools.installed": "Installed",
  "Studio.tools.remove": "Remove this tool",
  "Studio.tools.addLocal": "Add your own binary",
  "Studio.tools.addLocalHint":
    "A program that speaks the tool contract on 127.0.0.1. Nothing checks these bytes for you — only add one you built or trust.",
  "Studio.tools.managed": "Verified",
  "Studio.tools.managedHint":
    "Downloaded through the Runtime Hub and checked against a published SHA-256 before it was activated.",
  "Studio.tools.unmanaged": "Your own",
  "Studio.tools.unmanagedHint":
    "A binary you pointed at yourself. It was not downloaded or digest-checked by the app.",
  "Studio.tools.select": "Choose a tool to see what it does.",
  "Studio.tools.starting": "Starting the tool…",
  "Studio.tools.settings": "Settings",
  "Studio.tools.run": "Run",
  "Studio.tools.running": "Running…",
  "Studio.tools.stop": "Release memory",
  "Studio.tools.stopAll": "Release all {count}",
  "Studio.tools.importCatalog": "Import a catalog",
  "Studio.tools.missing": "Fill in {fields} first",
  "Studio.tools.pickImage": "Choose an image",
  "Studio.tools.clearImage": "Remove this image",
  "Studio.tools.fromGallery": "Use the newest result",
  "Studio.tools.results": "Results",
  "Studio.tools.version": "Version {version}",

  "Studio.noneForTab": "No model in your library can do this yet",
  "Studio.notDownloaded": "not downloaded",
  "Studio.browserOnly.title": "Studio runs in the desktop app",
  "Studio.browserOnly.body":
    "This page is open in a browser, where Studio cannot reach the generation engine. Open Little Monkey itself to add models and generate.",
  "Studio.emptyLibrary": "Your library is empty. Add a model to get started — nothing is preinstalled.",
  "Studio.forget": "Forget this model",
  "Studio.add.open": "Add model",
  "Studio.add.cancel": "Cancel",
  "Studio.add.title": "Add a model",
  "Studio.add.slotHint":
    "Each part is named from its own file name where that says enough — check them. A part named wrong fails inside the engine as a tensor-shape error, not here.",
  "Studio.add.slot": "What this part is",
  "Studio.add.source": "Where it comes from",
  // Example inputs. They are here so every string the panel renders has one
  // home, but they stay identical in every locale: a repo id, a path inside a
  // repo and an engine flag are not English, they are the literal thing to
  // type, and translating one would produce an example that does not work.
  "Studio.add.repoPlaceholder": "Comfy-Org/Wan_2.2_ComfyUI_Repackaged",
  "Studio.add.repoFilePlaceholder": "split_files/vae/wan2.2_vae.safetensors",
  "Studio.add.namePlaceholder": "Wan 2.2 TI2V 5B",
  "Studio.add.familyPlaceholder": "Wan",
  "Studio.add.engineArgsPlaceholder": "--diffusion-fa --threads 8 --offload-to-cpu",
  "Studio.speakerPlaceholder": "/Users/you/voices/narrator.wav",
  "Studio.languagePlaceholder": "en",
  "Studio.add.name": "Name",
  "Studio.add.family": "Family",
  "Studio.add.tasks": "What it can do",
  "Studio.add.files": "Model parts",
  "Studio.add.addFile": "Add a part",
  "Studio.add.removeFile": "Remove this part",
  "Studio.add.onDisk": "On this machine",
  "Studio.add.browse": "Choose file",
  "Studio.add.choose": "Choose a file…",
  "Studio.add.download": "Download",
  "Studio.add.fps": "Frames per second",
  "Studio.add.frameGrid": "Frame grid",
  "Studio.add.grid4n1": "Round down to 4n+1 (Wan, most video)",
  "Studio.add.grid17k5": "Round up to 17k+5 (MiniMax H3)",
  "Studio.adetailer.title": "Re-detail",
  "Studio.adetailer.hint":
    "The detector finds each face or hand and the model repaints that region on its own, at full resolution. Leave the prompts empty to reuse the ones above.",
  "Studio.adetailer.prompt": "Re-detail prompt",
  "Studio.adetailer.promptPlaceholder": "Same as the main prompt",
  "Studio.adetailer.negative": "Re-detail negative prompt",
  "Studio.adetailer.negativePlaceholder": "Same as the main negative prompt",
  "Studio.add.upscalersDir": "Upscalers folder",
  "Studio.add.upscalersDirHint":
    "ESRGAN and other upscale models found here join the upscaler list, alongside the built-in ones.",
  "Studio.add.embeddingsDir": "Embeddings folder",
  "Studio.add.embeddingsDirHint":
    "Textual-inversion embeddings found here can be named in a prompt.",
  "Studio.add.chooseFolder": "Choose folder…",
  "Studio.add.clearFolder": "Clear folder",
  "Studio.add.noFolder": "None",
  // Which program renders the model. Named by what the user has to know — the
  // file format each one reads — because "MLX" and "stable-diffusion.cpp" mean
  // nothing next to a downloaded file, and picking wrong fails deep inside a
  // loader with a message about tensors.
  "Studio.add.engine": "Engine",
  "Studio.add.engineBundled": "Built in (stable-diffusion.cpp)",
  "Studio.add.engineMlxVideo": "MLX video (Apple silicon)",
  "Studio.add.engineHint":
    "Reads safetensors and GGUF weights. The right choice for everything except a checkpoint converted for MLX.",
  "Studio.add.engineMlxVideoHint":
    "For MLX conversions — a folder holding config.json beside model, t5_encoder and vae safetensors. Needs the MLX package from Settings → Runtime Hub, and makes video only.",

  // Launch-time engine switches. Each hint says what it costs as well as what
  // it buys — every one of these is a trade, and a checkbox that only promises
  // the upside gets turned on by everybody and blamed for the downside.
  "Studio.add.engineOptions": "Engine options",
  "Studio.add.vaeTiling": "Decode the image in tiles",
  "Studio.add.vaeTilingHint":
    "Uses far less memory at the final decode, which is what usually fails on a large image. Slightly slower, and very occasionally leaves a faint seam.",
  "Studio.add.offloadToCpu": "Keep weights in system memory",
  "Studio.add.offloadToCpuHint":
    "Holds the model in RAM and moves each part to the GPU as it is needed, so a model larger than the card can still run. Noticeably slower.",
  "Studio.add.flashAttention": "Flash attention",
  "Studio.add.flashAttentionHint":
    "Faster and lighter on memory in the diffusion model. Not supported by every backend — turn it off if generation fails at startup.",
  "Studio.add.seamless": "Tileable output",
  "Studio.add.seamlessHint":
    "Makes the result repeat without a visible join, for textures and patterns. Set per model rather than per image, because the engine takes it at startup.",

  "Studio.add.engineArgs": "Extra engine arguments",
  "Studio.add.engineArgsHint":
    "Passed to the engine as typed — no shell, so quote any path with spaces. This is how flags without their own field are reached: --vae-format, --model-args.",
  "Studio.add.save": "Add to library",
  "Studio.lora.title": "LoRAs",
  "Studio.lora.add": "Add LoRA",
  "Studio.lora.remove": "Remove this LoRA",
  "Studio.lora.strength": "Strength",
  "Studio.lora.highNoise": "high noise",
  "Studio.lora.library": "LoRA library",
  "Studio.lora.addToLibrary": "Add a LoRA",
  "Studio.lora.libraryEmpty":
    "No LoRAs yet. Add the files you have and they become pickable in every generation.",
  "Studio.lora.empty": "No LoRAs in your library. Add one in the Models tab.",
  "Studio.lora.pick": "Which LoRA",
  "Studio.lora.forget": "Remove from library",
  "Studio.parts": "Model parts",
  "Studio.partsHint":
    "Pick which CLIP, text encoder or VAE this run loads. Add them in the Models tab; this only chooses between them.",
  "Studio.parts.own": "the model's own",
  "Studio.parts.none": "None",
  "Studio.partsSave": "Save",
  "Studio.partsLibrary": "CLIPs, text encoders & VAEs",
  "Studio.partsAdd": "Add a part",
  "Studio.partsForget": "Remove from library",
  "Studio.partsLibraryEmpty":
    "None yet. A checkpoint that needs a separate VAE or text encoder does not name one — add the file here and pick it when you generate.",
  "Studio.backends": "Remote backends",
  "Studio.backendAdd": "Add a backend",
  "Studio.backendsEmpty":
    "None. The managed engine runs the weight files above. Add a backend to reach a ComfyUI you run yourself, or a hosted image API when this machine has no GPU.",
  "Studio.backendModelCount": "{count} models",
  "Studio.backend.title": "Add a remote backend",
  "Studio.backend.hint":
    "Nothing is installed here. Studio only stores the address, and for a hosted endpoint which saved provider key to use — no key is entered on this form.",
  "Studio.backend.kind": "Kind",
  "Studio.backend.kindComfy": "ComfyUI",
  "Studio.backend.kindOpenAi": "OpenAI-compatible",
  "Studio.backend.label": "Name",
  "Studio.backend.labelPlaceholder": "ComfyUI on the desktop",
  "Studio.backend.id": "Id",
  "Studio.backend.idPlaceholder": "comfy",
  "Studio.backend.baseUrl": "Base URL",
  "Studio.backend.provider": "Provider whose key this uses",
  "Studio.backend.editing": "This endpoint accepts a source image (/images/edits)",
  "Studio.backend.models": "Model names, one per line",
  "Studio.backend.workflow": "API-format workflow",
  "Studio.backend.badWorkflow": "Workflow JSON: {detail}",
  "Studio.backend.save": "Save backend",
  "Studio.slot.checkpoint": "Checkpoint",
  "Studio.slot.diffusion_model": "Diffusion model",
  "Studio.slot.high_noise_diffusion_model": "Diffusion model, high noise",
  "Studio.slot.clip_l": "CLIP-L text encoder",
  "Studio.slot.clip_g": "CLIP-G text encoder",
  "Studio.slot.clip_vision": "CLIP vision encoder",
  "Studio.slot.t5xxl": "T5 text encoder",
  "Studio.slot.llm": "LLM text encoder",
  "Studio.slot.llm_vision": "LLM vision tower",
  "Studio.slot.uncond_diffusion_model": "Diffusion model, unconditional",
  "Studio.slot.embeddings_connectors": "Embeddings connectors",
  "Studio.slot.motion_module": "Motion module",
  "Studio.slot.vae": "VAE",
  "Studio.slot.audio_vae": "Audio VAE",
  "Studio.slot.taesd": "TAESD preview decoder",
  "Studio.slot.control_net": "ControlNet",
  "Studio.slot.ip_adapter": "IP-Adapter",
  "Studio.slot.photo_maker": "PhotoMaker",
  "Studio.slot.pulid_weights": "PuLID",
  "Studio.slot.mmproj": "Speech projector",
  "Studio.slot.vocoder": "Vocoder",
  "Studio.installed": "Installed",
  "Studio.download": "Download {{size}}",
  "Studio.cancelDownload": "Cancel download",
  "Studio.tooLarge": "Needs about {{needed}} of memory — this machine has less, so generation would swap.",
  "Studio.license.restricted":
    "{{name}} does not grant rights in {{territories}}. Read the terms and accept them before downloading these weights.",
  "Studio.license.read": "Read the licence",
  "Studio.license.accept": "I accept",
  "Studio.task.text_to_image": "Text to image",
  "Studio.task.image_to_image": "Image to image",
  "Studio.task.text_to_video": "Text to video",
  "Studio.task.image_to_video": "Image to video",
  "Studio.promptPlaceholder": "Describe the shot, the camera move, and the sound.",
  "Studio.negativePlaceholder": "What to avoid (optional)",
  "Studio.chooseImage": "Choose image",
  "Studio.imageReady": "Image ready",
  "Studio.clearImage": "Remove image",

  "Studio.mask.title": "Repaint area",
  "Studio.mask.brush": "Brush",
  "Studio.mask.paint": "Paint",
  "Studio.mask.erase": "Erase",
  "Studio.mask.clear": "Clear",
  "Studio.mask.undo": "Undo the last stroke",
  "Studio.mask.redo": "Redo the undone stroke",
  "Studio.mask.loading": "Loading the source image…",
  "Studio.mask.hint":
    "Paint over what should be redrawn; everything unpainted is kept. Mask is {{width}}×{{height}}, matching the source.",

  "Studio.control.title": "Control image",
  "Studio.control.strength": "Control strength",
  "Studio.control.hint":
    "Already a depth map, pose skeleton or edge map — no detector runs here, so a plain photo is followed as though it were one.",

  "Studio.ipAdapter.title": "Style reference",
  "Studio.ipAdapter.strength": "Reference strength",
  "Studio.ipAdapter.hint": "The look to borrow, read through the IP-Adapter.",

  "Studio.reference.title": "Reference images",
  "Studio.reference.add": "Add reference",
  "Studio.reference.remove": "Remove reference",
  "Studio.reference.hint": "Photographs of the subject to keep consistent.",
  "Studio.reference.full": "At most {{max}} reference images per run.",
  "Studio.reference.numbered": "Number them so the prompt can tell them apart",
  "Studio.reference.numberedHint":
    "Each reference is numbered in the order shown, so a prompt can say \"the jacket from image 2\".",
  "Studio.reference.numberedAlt": "Reference image {{index}}",
  "Studio.duration": "Duration",
  "Studio.frames": "{{frames}} frames · {{fps}} fps",
  "Studio.settings": "Settings",
  "Studio.aspect": "Aspect ratio",
  "Studio.aspect.portrait": "Portrait",
  "Studio.aspect.landscape": "Landscape",
  "Studio.aspect.square": "Square",
  "Studio.aspect.original": "Original size of the source image",
  "Studio.width": "Width",
  "Studio.height": "Height",
  "Studio.steps": "Sampling steps",
  "Studio.batch": "Images per run",
  "Studio.guidance": "CFG scale",
  "Studio.sampler": "Sampling method",
  "Studio.denoise": "Denoising strength",
  "Studio.advanced": "Advanced settings",
  "Studio.scheduler": "Scheduler",
  "Studio.clipSkip": "Clip skip",
  "Studio.engineDefault": "Model's own",
  "Studio.upscale": "Upscale",
  "Studio.upscaleTo": "to {{target}}",
  "Studio.upscaler": "Upscaler",
  "Studio.upscalerHint":
    "Built-in upscalers are listed. For R-ESRGAN and friends, point --hires-upscalers-dir at their folder in the model's extra engine arguments, then type the model's name here.",
  "Studio.hiresSteps": "Hires steps",
  "Studio.seedPlaceholder": "Empty for random",
  "Studio.seedShuffle": "Pick a random seed",
  "Studio.speakerFile": "Reference voice",
  "Studio.speakerHint":
    "Optional path to an audio clip. The voice in it is the voice you get. Leave empty for the model's own.",
  "Studio.language": "Language",
  "Studio.languageHint": "ISO 639-1, e.g. en",
  "Studio.result.edit": "Edit this",
  "Studio.result.save": "Save",
  "Studio.result.delete": "Delete",
  "Studio.result.deleteConfirm": "Delete this generation? Its file is removed from disk and cannot be recovered.",
  "Studio.result.close": "Close",
  "Studio.result.expand": "Show full size",
  "Studio.result.noEditTask":
    "{{name}} cannot start from an existing image. Pick a model that does image to image or image to video.",
  "Studio.seed": "Seed",
  "Studio.seedHint": "-1 picks a new one each run",
  "Studio.generate": "Generate",
  "Studio.stop": "Stop",
  "Studio.queue.waiting": "Queued ({{count}})",
  "Studio.queue.remove": "Remove from queue",
  "Studio.queue.chip.one": "1 generation running",
  "Studio.queue.chip.many": "{{count}} generations running",
  "Studio.unload": "Free memory",
  "Studio.unloadIdle": "No model is loaded",
  "Studio.phase.submitted": "Submitted",
  "Studio.phase.loading": "Loading weights",
  "Studio.phase.running": "Generating",
  "Studio.phase.step": "Step {{step}} of {{total}}",
  "Studio.phase.stopping": "Stopping",
  "Studio.phase.completed": "Done",
  "Studio.phase.queued": "Queued, {{position}} ahead",
  "Studio.gallery": "Generations",
  "Studio.galleryEmpty": "Nothing generated yet.",
  "Studio.loadPreview": "Load preview",
  "Studio.unsupported.title": "Studio is not available on this machine",
  "Studio.unsupported.body":
    "The generation engine ships prebuilt for Apple silicon Macs and for x86-64 Windows and Linux. Chat and Code are unaffected.",
};
