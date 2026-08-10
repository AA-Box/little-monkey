/**
 * Reads a user-picked image file into a `data:` URL, entirely client-side.
 *
 * No new Tauri/Rust command is needed for this: Tauri v2 automatically
 * extends `@tauri-apps/plugin-fs`'s read scope to any path the user picked
 * through the `@tauri-apps/plugin-dialog` file picker (the same `open()`
 * call `ChatWindow.tsx` already uses for every attachment), so `readFile`
 * can read these bytes directly without going through a sandboxed
 * workspace-relative Rust command like `tool_read_file`.
 */
import { open } from '@tauri-apps/plugin-dialog';
import { readFile } from '@tauri-apps/plugin-fs';

/** Extension -> MIME type for the raster formats this app treats as "an image" for vision purposes. */
const IMAGE_MIME_BY_EXTENSION: Record<string, string> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
};

function extensionOf(path: string): string {
  const base = path.split(/[\\/]/).pop() ?? path;
  const dot = base.lastIndexOf('.');
  return dot === -1 ? '' : base.slice(dot + 1).toLowerCase();
}

/** Whether `path` has a raster image extension this app knows how to attach as a vision content part. */
export function isImagePath(path: string): boolean {
  return extensionOf(path) in IMAGE_MIME_BY_EXTENSION;
}

/** Opens the native picker and returns the chosen image as bare base64 (no
 * `data:` prefix), or null when the dialog is dismissed.
 *
 * The native dialog rather than a hidden `<input type="file">`: the input keeps
 * the previous selection in `value`, so re-picking the same file after clearing
 * it fires no change event and the image silently refuses to load. */
export async function pickImageBase64(): Promise<string | null> {
  const path = await open({
    multiple: false,
    filters: [{ name: 'Image', extensions: ['png', 'jpg', 'jpeg', 'webp'] }],
  });
  if (typeof path !== 'string') return null;
  return (await readImageAsDataUrl(path)).split(',')[1] ?? null;
}

/** Reads `path`'s bytes and returns a `data:<mime>;base64,...` URL, suitable
 * for an OpenAI-style `image_url` content part. Throws if the file can't be
 * read (permission denied, doesn't exist, etc.) — callers should treat that
 * the same as any other attachment-resolution failure. */
export async function readImageAsDataUrl(path: string): Promise<string> {
  const bytes = await readFile(path);
  const mime = IMAGE_MIME_BY_EXTENSION[extensionOf(path)] ?? 'application/octet-stream';
  const blob = new Blob([bytes], { type: mime });

  return await new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () => reject(reader.error ?? new Error(`Failed to read '${path}' as a data URL`));
    reader.readAsDataURL(blob);
  });
}
