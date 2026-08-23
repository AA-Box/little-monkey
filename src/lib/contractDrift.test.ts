/**
 * The desktop half of the K19 gate.
 *
 * The published contract (`contract/agent-os-contract.json`) is generated in
 * Rust from `src-tauri/src/agent_tools.rs` — the shared agent tool schemas the
 * desktop app and `monkey-cli` both offer. The desktop's own copy is
 * `src/lib/tools.ts`, and a copy that nothing compares drifts: a parameter
 * renamed here and not there is a published contract describing a tool the app
 * does not have.
 *
 * **What is compared, and what deliberately is not.** The *schema* is the
 * contract: the parameter names, which of them are required, their types, and
 * whether extra properties are allowed. The *description* is not, and cannot
 * be — `tools_def.rs` says so in its first line, because the desktop supports
 * multiple workspace folders and tells the model about the `label/` prefix
 * while the CLI, which has one `--workspace` root, has nothing to disambiguate.
 * Two surfaces phrasing the same schema for their own capabilities is correct;
 * two surfaces disagreeing about what the arguments *are* is the defect this
 * test exists to catch.
 *
 * Tools that exist only in `tools.ts` are the desktop extension set, which
 * contract v1 does not publish (`docs/contract-abi.md` states that gap rather
 * than hiding it). Pinning the list here means a seventh one cannot appear
 * unnoticed.
 */
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

import {
  TOOLS,
  PRESENT_PLAN_TOOL,
  TASK_TOOL,
  SKILL_INVOKE_TOOL,
  READ_SKILL_RESOURCE_TOOL,
  GENERATE_IMAGE_TOOL,
  buildTools,
} from './tools';

type ContractTool = {
  name: string;
  description: string;
  availability: string;
  parameters: JsonSchema;
};

type JsonSchema = {
  type?: string;
  properties?: Record<string, { type?: string; enum?: unknown[] }>;
  required?: string[];
  additionalProperties?: boolean;
};

const contract = JSON.parse(
  readFileSync(new URL('../../contract/agent-os-contract.json', import.meta.url), 'utf8'),
) as { contract_version: string; tools: ContractTool[] };

/** Tools the desktop offers that contract v1 does not publish. */
const DESKTOP_ONLY = [
  'computer_clipboard_read',
  'computer_click',
  'computer_double_click',
  'computer_focus',
  'computer_hotkey',
  'computer_inspect',
  'computer_key',
  'computer_list_targets',
  'computer_screenshot',
  'computer_scroll',
  'computer_select',
  'computer_set_value',
  'computer_type',
  'computer_wait',
  'spawn_task',
  'shell_output',
  'shell_kill',
  'skill',
  'read_skill_resource',
  'generate_image',
];

const desktopTools = [
  ...TOOLS,
  PRESENT_PLAN_TOOL,
  TASK_TOOL,
  SKILL_INVOKE_TOOL,
  READ_SKILL_RESOURCE_TOOL,
  GENERATE_IMAGE_TOOL,
  // `search_docs` only exists once a stack is attached; the contract publishes
  // it with the `{stack_names}` placeholder the Rust side splices in.
  ...buildTools(['{stack_names}']).filter((tool) => tool.function.name === 'search_docs'),
];

/** The part of a tool's schema that is the contract rather than the wording. */
function shape(parameters: JsonSchema) {
  return {
    type: parameters.type,
    additionalProperties: parameters.additionalProperties,
    required: [...(parameters.required ?? [])].sort(),
    properties: Object.fromEntries(
      Object.entries(parameters.properties ?? {})
        .map(([name, schema]) => [name, { type: schema.type, enum: schema.enum }] as const)
        .sort(([a], [b]) => a.localeCompare(b)),
    ),
  };
}

/**
 * Optional parameters the desktop accepts on a published tool and the
 * published contract does not carry, each because the capability behind it is
 * desktop-only. Every one is optional by construction — the assertion below
 * proves it — so a client written against the contract still calls the tool
 * correctly; it just cannot reach the extra behaviour. Listed rather than
 * waved through, so a *seventh* extension is a failing test and a decision.
 */
const DESKTOP_ONLY_PARAMETERS: Record<string, string[]> = {
  // Backgrounding a shell needs `shell_output`/`shell_kill` to be reachable,
  // and those are themselves desktop-only.
  run_shell: ['run_in_background'],
};

describe('published contract vs the desktop tool definitions', () => {
  it('publishes a version and a non-empty tool set', () => {
    expect(contract.contract_version).toMatch(/^\d+\.\d+\.\d+$/);
    expect(contract.tools.length).toBeGreaterThan(0);
  });

  it.each(contract.tools.map((tool) => [tool.name, tool] as const))(
    'the desktop implements %s with the published schema',
    (name, published) => {
      const local = desktopTools.find((tool) => tool.function.name === name);
      expect(local, `${name} is published but the desktop does not offer it`).toBeDefined();
      const localShape = shape(local!.function.parameters as JsonSchema);
      const extensions = DESKTOP_ONLY_PARAMETERS[name] ?? [];
      for (const parameter of extensions) {
        expect(
          localShape.required,
          `${name}.${parameter} is a desktop extension, so it must stay optional`,
        ).not.toContain(parameter);
        delete localShape.properties[parameter];
      }
      expect(localShape).toEqual(shape(published.parameters));
    },
  );

  it('has no desktop tool outside the published set except the declared extensions', () => {
    const publishedNames = new Set(contract.tools.map((tool) => tool.name));
    const extra = desktopTools
      .map((tool) => tool.function.name)
      .filter((name) => !publishedNames.has(name))
      .sort();
    expect(extra).toEqual([...DESKTOP_ONLY].sort());
  });
});
