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

  "Studio.title": "Studio",
  "Studio.subtitle": "Generate images and video on this machine. No account, no upload.",
  "Studio.models": "Models",
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
  "Studio.duration": "Duration",
  "Studio.frames": "{{frames}} frames · {{fps}} fps",
  "Studio.seed": "Seed",
  "Studio.seedHint": "-1 picks a new one each run",
  "Studio.generate": "Generate",
  "Studio.unload": "Free memory",
  "Studio.phase.submitted": "Submitted",
  "Studio.phase.running": "Generating",
  "Studio.phase.completed": "Done",
  "Studio.phase.queued": "Queued, {{position}} ahead",
  "Studio.gallery": "Generations",
  "Studio.galleryEmpty": "Nothing generated yet.",
  "Studio.loadPreview": "Load preview",
  "Studio.unsupported.title": "Studio is not available on this machine",
  "Studio.unsupported.body":
    "The generation engine ships prebuilt for Apple silicon Macs and for x86-64 Windows and Linux. Chat and Code are unaffected.",
};
