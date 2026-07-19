/**
 * PNG generation plus durable/generated and workspace-image loading for the
 * `generate_image` model tool and the chat's inline image previews.
 *
 * Split from `turnEngine.ts` (which intercepts the `generate_image` tool call
 * and calls `rasterizeSvgToPng` below) because rasterization is inherently
 * DOM-bound: a text model can't emit PNG bytes, so the model supplies SVG
 * markup — the one raster-free image format local models produce reliably,
 * per the artifact design doc's "DETECTION" reasoning — and the webview's
 * own `<canvas>` turns it into real PNG bytes. The Rust side
 * (`tools.rs::tool_generate_image`) then persists those bytes in private,
 * app-owned durable artifact storage. A workspace is not involved; exporting
 * to a user-selected path happens only when Download is pressed.
 *
 * The pure helpers (`svgDimensions`, `isWorkspaceImageSrc`) are kept
 * DOM-free so they run under vitest's `environment: "node"` — everything
 * touching `Image`/`canvas`/`URL.createObjectURL` lives in
 * `rasterizeSvgToPng` alone, which tests must stub rather than call.
 */
import { invoke, isTauri } from '@tauri-apps/api/core';
import { artifactDataUrl, readDurableArtifact } from './durableArtifacts';

/** Hard bound on either rasterized dimension, applied after `svgDimensions`
 * (an SVG declaring a 100000px width must not allocate a canvas that size —
 * WebKit refuses very large canvases silently, yielding an empty PNG). The
 * aspect ratio is preserved when scaling down. */
export const MAX_RASTER_DIMENSION = 4096;

/** Fallback raster size for an SVG that declares no usable width/height or
 * viewBox at all. */
export const DEFAULT_RASTER_SIZE = { width: 800, height: 600 };

/**
 * Extracts the intended pixel dimensions of an SVG document from its root
 * element: explicit `width`/`height` attributes win (ignoring
 * percentage/relative values, which have no absolute meaning without a
 * container), then the `viewBox` rect, then `DEFAULT_RASTER_SIZE`. Pure
 * string parsing (no DOM) so it is unit-testable under node — the regexes
 * only need to find the ROOT `<svg …>` tag's attributes, so they scan the
 * first `<svg` occurrence only.
 */
export function svgDimensions(svg: string): { width: number; height: number } {
  const rootTag = /<svg\b[^>]*>/i.exec(svg)?.[0] ?? '';

  const attr = (name: string): number | null => {
    const match = new RegExp(`\\b${name}\\s*=\\s*["']\\s*([0-9.]+)\\s*(px)?\\s*["']`, 'i').exec(rootTag);
    if (!match) return null;
    const value = Number.parseFloat(match[1]);
    return Number.isFinite(value) && value > 0 ? value : null;
  };

  const width = attr('width');
  const height = attr('height');
  if (width !== null && height !== null) return { width, height };

  const viewBox = /\bviewBox\s*=\s*["']\s*([-0-9.]+)[\s,]+([-0-9.]+)[\s,]+([0-9.]+)[\s,]+([0-9.]+)\s*["']/i.exec(rootTag);
  if (viewBox) {
    const vbWidth = Number.parseFloat(viewBox[3]);
    const vbHeight = Number.parseFloat(viewBox[4]);
    if (Number.isFinite(vbWidth) && vbWidth > 0 && Number.isFinite(vbHeight) && vbHeight > 0) {
      // One explicit absolute dimension + viewBox: honor the explicit one and
      // derive the other from the viewBox's aspect ratio.
      if (width !== null) return { width, height: (width * vbHeight) / vbWidth };
      if (height !== null) return { width: (height * vbWidth) / vbHeight, height };
      return { width: vbWidth, height: vbHeight };
    }
  }

  if (width !== null) return { width, height: width * (DEFAULT_RASTER_SIZE.height / DEFAULT_RASTER_SIZE.width) };
  if (height !== null) return { width: height * (DEFAULT_RASTER_SIZE.width / DEFAULT_RASTER_SIZE.height), height };
  return { ...DEFAULT_RASTER_SIZE };
}

/** Scales `dims` down (never up) so neither side exceeds
 * `MAX_RASTER_DIMENSION`, preserving aspect ratio and rounding to whole
 * pixels with a floor of 1. Exported separately from `svgDimensions` (rather
 * than folded in) so the clamp is testable against exact inputs. */
export function clampRasterDimensions(dims: { width: number; height: number }): { width: number; height: number } {
  const scale = Math.min(1, MAX_RASTER_DIMENSION / dims.width, MAX_RASTER_DIMENSION / dims.height);
  return {
    width: Math.max(1, Math.round(dims.width * scale)),
    height: Math.max(1, Math.round(dims.height * scale)),
  };
}

export interface RasterizedPng {
  /** Raw base64 (no `data:` prefix) of the PNG bytes. */
  contentBase64: string;
  width: number;
  height: number;
}

/** Compact durable reference returned by `tools.rs::tool_generate_image` and
 * persisted verbatim as the tool-result message in the transcript. */
export interface GeneratedImageReceipt {
  artifactId: string;
  mediaType: 'image/png';
  width: number;
  height: number;
  size: number;
  suggestedName: string;
}

/** Parses a generated-image tool result without trusting arbitrary JSON from
 * a model/provider transcript as an artifact id or media type. Legacy results
 * were plain strings and intentionally return `null` so callers can fall back
 * to the old workspace-image loader. */
export function parseGeneratedImageReceipt(result: string | undefined): GeneratedImageReceipt | null {
  if (!result) return null;
  try {
    const value: unknown = JSON.parse(result);
    if (!value || typeof value !== 'object') return null;
    const receipt = value as Partial<GeneratedImageReceipt>;
    if (!/^[a-f0-9]{64}$/.test(receipt.artifactId ?? '')) return null;
    if (receipt.mediaType !== 'image/png') return null;
    if (!Number.isInteger(receipt.width) || (receipt.width ?? 0) < 1) return null;
    if (!Number.isInteger(receipt.height) || (receipt.height ?? 0) < 1) return null;
    if (!Number.isInteger(receipt.size) || (receipt.size ?? -1) < 0) return null;
    if (typeof receipt.suggestedName !== 'string' || !receipt.suggestedName.toLowerCase().endsWith('.png')) return null;
    return receipt as GeneratedImageReceipt;
  } catch {
    return null;
  }
}

/**
 * Rasterizes SVG markup to PNG via an offscreen `<canvas>`. Loading the SVG
 * through an `<img>` (object URL over a Blob) is what makes this safe to run
 * in the MAIN webview: per the HTML spec an SVG document loaded as an image
 * never executes scripts and never fetches external subresources, so a
 * malicious `<script>` inside model-authored markup simply doesn't run —
 * unlike `mermaid.render`, which needs `securityLevel: 'strict'` for the
 * same reason (see `artifacts.ts::renderMermaidToSvg`).
 *
 * Rejects (never throws synchronously) on markup the engine can't parse as
 * an image, so the `generate_image` interception branch in `turnEngine.ts`
 * can hand the model a recoverable tool error.
 */
export function rasterizeSvgToPng(svg: string): Promise<RasterizedPng> {
  const { width, height } = clampRasterDimensions(svgDimensions(svg));

  // WebKit renders an SVG with no explicit root width/height at its own
  // default size, ignoring the viewBox-derived size computed above — pinning
  // the computed size onto the root tag makes every engine agree with it.
  //
  // Models routinely omit `xmlns` — it's not needed for SVG inlined in HTML,
  // which is the form they see most in training data. But loaded standalone
  // via `<img src="blob:...">` the markup is parsed as a freestanding XML
  // document, and without a namespace declaration on the root element every
  // engine rejects it outright (a bare `image.onerror`, no useful detail) —
  // so it's injected here alongside the size attributes whenever absent.
  const sized = svg.replace(/<svg\b([^>]*)>/i, (_tag, attrs: string) => {
    const stripped = attrs.replace(/\s(width|height)\s*=\s*("[^"]*"|'[^']*')/gi, '');
    const xmlns = /\bxmlns\s*=/i.test(attrs) ? '' : ' xmlns="http://www.w3.org/2000/svg"';
    return `<svg${stripped}${xmlns} width="${width}" height="${height}">`;
  });

  const blob = new Blob([sized], { type: 'image/svg+xml' });
  const url = URL.createObjectURL(blob);

  return new Promise<RasterizedPng>((resolve, reject) => {
    const image = new Image();
    image.onload = () => {
      URL.revokeObjectURL(url);
      try {
        const canvas = document.createElement('canvas');
        canvas.width = width;
        canvas.height = height;
        const context = canvas.getContext('2d');
        if (!context) {
          reject(new Error('Canvas 2D context is unavailable in this webview'));
          return;
        }
        // White backing: PNG keeps alpha, but charts drawn against an
        // implicit background otherwise read as black-on-transparent in
        // dark-mode viewers. Models wanting transparency can draw their own
        // background rect — a solid default is the less surprising failure.
        context.fillStyle = '#ffffff';
        context.fillRect(0, 0, width, height);
        context.drawImage(image, 0, 0, width, height);
        const dataUrl = canvas.toDataURL('image/png');
        const base64 = dataUrl.split(',', 2)[1];
        if (!base64) {
          reject(new Error('Canvas produced an empty PNG'));
          return;
        }
        resolve({ contentBase64: base64, width, height });
      } catch (err) {
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    };
    image.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error('The SVG markup could not be rendered as an image — check that it is well-formed'));
    };
    image.src = url;
  });
}

/** File extensions `workspace_read_image` (tools.rs) accepts — mirrored here
 * so `isWorkspaceImageSrc` can cheaply pre-filter Markdown image srcs
 * without a round trip that would just error. */
const IMAGE_EXTENSIONS = /\.(png|jpe?g|gif|webp|bmp|svg)$/i;

/**
 * Whether a Markdown image's `src` looks like a workspace-relative image
 * path worth resolving through `workspace_read_image`: a relative path (no
 * URL scheme, no leading `/` — absolute filesystem paths are not
 * workspace-relative and `resolve_path_and_root` would reject them anyway)
 * ending in a previewable image extension. `data:`/`http(s):`/`blob:` srcs
 * are left to the plain `<img>` the Markdown renderer would emit natively.
 */
export function isWorkspaceImageSrc(src: string | undefined): src is string {
  if (!src) return false;
  if (/^[a-z][a-z0-9+.-]*:/i.test(src)) return false; // any URL scheme
  if (src.startsWith('/') || src.startsWith('\\')) return false;
  return IMAGE_EXTENSIONS.test(src);
}

/** Loads a generated PNG from app-owned durable storage. This works with no
 * selected workspace and survives app restarts because only the content hash
 * is persisted in the transcript. */
export async function loadGeneratedImage(artifactId: string): Promise<string | null> {
  if (!isTauri()) return null;
  const payload = await readDurableArtifact(artifactId);
  return artifactDataUrl('image/png', payload.contentBase64);
}

/** Shape returned by the `workspace_read_image` Tauri command. */
interface WorkspaceImagePayload {
  mime: string;
  contentBase64: string;
  size: number;
}

/**
 * Loads a workspace image file as a `data:` URL for inline display in the
 * chat transcript (see `WorkspaceImagePreview.tsx`). Returns `null` outside
 * Tauri (browser mode has no workspace filesystem to read); throws with the
 * Rust command's own message for a path that is missing, oversized, or not
 * an image, so callers can show it.
 */
export async function loadWorkspaceImage(path: string): Promise<string | null> {
  if (!isTauri()) return null;
  const payload = await invoke<WorkspaceImagePayload>('workspace_read_image', { path });
  return `data:${payload.mime};base64,${payload.contentBase64}`;
}
