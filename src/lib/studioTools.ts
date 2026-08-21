/**
 * Studio's sidecar tools: the plugin tier.
 *
 * A tool is a separate executable, not code loaded into this app — see
 * `src-tauri/src/studio_tools.rs` for why that boundary is the whole design.
 * What matters here is the consequence: a tool declares its inputs in a
 * manifest, and this module turns that declaration into a form. Nothing in the
 * UI knows what a face swapper or a segmenter is, so adding a tool adds a panel
 * without adding a component.
 *
 * The backend validates every input again before a run. These helpers exist so
 * the form can disable its own Run button and pre-fill its own controls, not as
 * the enforcement — that lives on the other side of the IPC boundary, where the
 * tool's own manifest is the one being checked.
 */
import { invoke } from "@tauri-apps/api/core";

/** Rolling catalog published by the face-swap package workflow. */
export const DEFAULT_STUDIO_TOOL_CATALOG_URL =
  "https://github.com/AA-Box/little-monkey/releases/download/face-swap-catalog/face-swap-catalog.json";

export type ToolInputKind = "image" | "text" | "number" | "toggle" | "choice";

export interface ToolChoice {
  value: string;
  label: string;
}

export interface ToolInput {
  key: string;
  label: string;
  kind: ToolInputKind;
  required: boolean;
  default?: ToolInputValue | null;
  min?: number | null;
  max?: number | null;
  step?: number | null;
  options: ToolChoice[];
  hint?: string | null;
}

export interface ToolManifest {
  schemaVersion: number;
  id: string;
  name: string;
  description?: string | null;
  licenseNotice?: ToolLicenseNotice | null;
  inputs: ToolInput[];
}

export interface ToolLicenseNotice {
  title: string;
  message: string;
  commercialUseAllowed: boolean;
  url?: string | null;
}

/** A tool in the user's library. */
export interface StudioTool {
  id: string;
  name: string;
  /** Absolute path to the executable. */
  path: string;
  version?: string | null;
  /** True when the bytes arrived through the Runtime Hub, which checked them
   *  against a declared SHA-256. False is a binary the user pointed at
   *  themselves — allowed, and labelled so the two are never confused. */
  managed: boolean;
}

export type ToolInputValue = string | number | boolean;

/** What the form holds. An absent key is an untouched control. */
export type ToolInputs = Record<string, ToolInputValue>;

/**
 * The starting state of a tool's form.
 *
 * A declared default is used as-is. Without one, each kind gets the empty value
 * its control actually renders — a `<select>` with no value shows its first
 * option while reporting none, which is how a required choice ends up looking
 * answered and failing on Run.
 */
export function toolDefaults(manifest: ToolManifest): ToolInputs {
  const values: ToolInputs = {};
  for (const input of manifest.inputs) {
    if (input.default !== undefined && input.default !== null) {
      values[input.key] = input.default;
      continue;
    }
    switch (input.kind) {
      case "toggle":
        values[input.key] = false;
        break;
      case "number":
        values[input.key] = input.min ?? 0;
        break;
      case "choice":
        if (input.options[0]) values[input.key] = input.options[0].value;
        break;
      default:
        break;
    }
  }
  return values;
}

/**
 * Which required inputs are still empty, by label.
 *
 * Labels rather than keys because the only caller puts them in a sentence for
 * the user. Whitespace counts as empty for the same reason the backend treats
 * it that way: a cleared box is not an answer.
 */
export function missingRequired(manifest: ToolManifest, values: ToolInputs): string[] {
  return manifest.inputs
    .filter((input) => {
      if (!input.required) return false;
      const value = values[input.key];
      if (value === undefined || value === null) return true;
      return typeof value === "string" && value.trim().length === 0;
    })
    .map((input) => input.label);
}

/**
 * Clamps a number the user typed into the range its input declares.
 *
 * Applied on change rather than on submit so the control cannot sit showing a
 * value the run is going to refuse. A blank or unparseable field falls back to
 * the minimum, because `Number("")` is 0 and that is a legal value in ranges
 * that do not contain it.
 */
export function clampToolNumber(input: ToolInput, raw: string): number {
  const parsed = Number(raw);
  const fallback = input.min ?? 0;
  let value = Number.isFinite(parsed) && raw.trim() !== "" ? parsed : fallback;
  if (input.min !== undefined && input.min !== null) value = Math.max(input.min, value);
  if (input.max !== undefined && input.max !== null) value = Math.min(input.max, value);
  return value;
}

/** Image inputs carry megabytes of base64; everything else is what a person
 *  would recognise in a summary line. */
export function isImageInput(input: ToolInput): boolean {
  return input.kind === "image";
}

export const toolsClient = {
  list: () => invoke<StudioTool[]>("studio_tools"),
  add: (tool: StudioTool) => invoke<StudioTool[]>("studio_tool_add", { tool }),
  remove: (toolId: string) => invoke<StudioTool[]>("studio_tool_remove", { toolId }),
  /** Starts the tool if it is not running, and returns what it declares. */
  manifest: (toolId: string) =>
    invoke<ToolManifest>("studio_tool_manifest", { toolId }),
  run: (toolId: string, inputs: ToolInputs) =>
    invoke<import("./studioClient").GenerationEntry[]>("studio_tool_run", {
      toolId,
      inputs,
    }),
  /** Tool ids holding memory right now. Several can be warm at once. */
  running: () => invoke<string[]>("studio_tools_running"),
  /** One tool, or every resident one when `toolId` is omitted. */
  stop: (toolId?: string) => invoke<void>("studio_tool_stop", { toolId: toolId ?? null }),
  /** Merges a published catalog's `studio_tool` entries into the component
   *  registry, which is what puts them behind the one-click Install. */
  importCatalog: (path: string) =>
    invoke<import("./runtimeHubClient").M3ComponentCatalogEntry[]>(
      "studio_tool_import_catalog",
      { path },
    ),
};
