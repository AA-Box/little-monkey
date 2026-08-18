import type { ToolDef } from "./llamaClient";
import {
  executableExtensionsClient,
  type InvocationResult,
} from "./executableExtensionsClient";

export type ExtensionToolRegistry = Map<
  string,
  { extensionId: string; capabilityId: string; kind: "tool"; version: string }
>;

function hashSegment(value: string): string {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(36);
}

function sanitizeSegment(value: string): string {
  const cleaned = value.replace(/[^a-zA-Z0-9_-]/g, "_") || "_";
  return cleaned.length <= 20
    ? cleaned
    : `${cleaned.slice(0, 12)}_${hashSegment(value).slice(0, 7)}`;
}

function uniqueName(base: string, used: Set<string>): string {
  if (!used.has(base)) {
    used.add(base);
    return base;
  }
  let suffix = 2;
  while (used.has(`${base}_${suffix}`)) suffix += 1;
  const name = `${base}_${suffix}`;
  used.add(name);
  return name;
}

/** Builds a turn-local capability table. Runtime state is authoritative: a
 * tool is offered only after validation and while it is enabled, running,
 * and healthy. A refresh in another pane cannot repoint this turn's names. */
export async function executableExtensionToolDefs(): Promise<{
  defs: ToolDef[];
  registry: ExtensionToolRegistry;
}> {
  const registry: ExtensionToolRegistry = new Map();
  const defs: ToolDef[] = [];
  const used = new Set<string>();

  let extensions;
  try {
    extensions = await executableExtensionsClient.list();
  } catch {
    return { defs, registry };
  }
  // A backend that answers with anything but a list has told us nothing about
  // what is installed, and a turn must not fail over that. Same outcome as the
  // catch above: no extension tools this turn. Checked rather than assumed
  // because this runs on the path of every single turn, so the one shape that
  // would throw here is the one shape that would break the whole app.
  if (!Array.isArray(extensions)) {
    return { defs, registry };
  }

  for (const extension of extensions) {
    if (
      !extension.health.enabled ||
      !extension.health.running ||
      !extension.health.validated ||
      extension.health.state !== "healthy"
    ) {
      continue;
    }
    for (const capability of extension.manifest.capabilities) {
      if (capability.kind !== "tool") continue;
      const base = `ext__${sanitizeSegment(extension.manifest.extension_id)}__${sanitizeSegment(capability.capability_id)}`;
      const name = uniqueName(base, used);
      registry.set(name, {
        extensionId: extension.manifest.extension_id,
        capabilityId: capability.capability_id,
        kind: "tool",
        version: extension.active_version,
      });
      defs.push({
        type: "function",
        function: {
          name,
          description: `[Extension: ${extension.manifest.display_name}] ${capability.description}`.trim(),
          parameters: capability.input_schema,
        },
      });
    }
  }

  return { defs, registry };
}

export function invokeExecutableExtensionTool(
  name: string,
  args: Record<string, unknown>,
  invocationId: string,
  registry: ExtensionToolRegistry,
): Promise<InvocationResult> {
  const resolved = registry.get(name);
  if (!resolved) {
    return Promise.reject(new Error(`Extension tool "${name}" was not offered this turn.`));
  }
  const input = { ...args };
  delete input.turn_id;
  delete input.tool_call_id;
  // Tool arguments are model-controlled. Artifact authority must come from a
  // trusted attachment resolver, never from an id the model put in JSON.
  delete input.input_artifact_ids;
  return executableExtensionsClient.invoke({
    extension_id: resolved.extensionId,
    capability_id: resolved.capabilityId,
    input_json: JSON.stringify(input),
    invocation_id: invocationId,
    input_artifact_ids: [],
    expected_kind: resolved.kind,
    expected_version: resolved.version,
  });
}
