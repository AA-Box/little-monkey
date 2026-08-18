import type { ToolDef } from "./llamaClient";
import { unwrapUntrustedContent } from "./untrustedContent";

export const PROGRAMMATIC_TOOL_NAME = "run_program";

export const PROGRAMMATIC_SYSTEM_GUIDANCE =
  "For bounded batch work, call run_program with a short async JavaScript program. Use only the tools object exposed in this turn (for example await tools.read_file({path: \"src/index.ts\"})); use Promise.all only for independent calls. The program cannot access files, network, processes, environment variables, secrets, imports, or host APIs, cannot call run_program recursively, and must return JSON-compatible data. One-off or approval-sensitive work should remain an ordinary tool call.";

export const PROGRAMMATIC_TOOL: ToolDef = {
  type: "function",
  function: {
    name: PROGRAMMATIC_TOOL_NAME,
    description:
      "Run one bounded JavaScript program that can call the other tools exposed in this turn through tools[<name>](args). Use it for repeated reads, filtering, fan-out queries, deterministic transformations, or several independent lookups. Prefer ordinary tool calls for one operation, interactive output, or an operation likely to need approval immediately. The program has no filesystem, network, process, environment, secret, import, or host API access. Return a JSON-compatible value.",
    parameters: {
      type: "object",
      properties: {
        source: {
          type: "string",
          description:
            "A bounded async JavaScript program. Call approved tools as await tools[\"tool_name\"](arguments), use Promise.all for bounded independent work, and finish with a JSON-compatible value.",
        },
      },
      required: ["source"],
      additionalProperties: false,
    },
  },
};

export type ProgrammaticFailureCategory =
  | "invalid_source"
  | "runtime_exception"
  | "execution_timeout"
  | "execution_budget"
  | "cancelled"
  | "nested_tool_failure"
  | "permission_denied"
  | "invalid_result"
  | "output_limit"
  | "runtime_failure";

export interface ProgrammaticFailure {
  category: ProgrammaticFailureCategory;
  message: string;
  toolName?: string;
}

export interface ProgrammaticNestedCallEvidence {
  id: string;
  toolName: string;
  arguments: unknown;
  status: "running" | "succeeded" | "failed" | "cancelled";
  durationMs?: number;
  result?: unknown;
  failure?: ProgrammaticFailure;
}

export interface ProgrammaticExecutionResult {
  executionId: string;
  status: "succeeded" | "failed" | "cancelled";
  value?: unknown;
  logs: string[];
  nestedCalls: ProgrammaticNestedCallEvidence[];
  durationMs: number;
  failure?: ProgrammaticFailure;
}

export interface ProgrammaticNestedToolResult {
  content: string;
  cancelled: boolean;
  failure?: ProgrammaticFailure;
}

export interface ProgrammaticExecutionRequest {
  executionId: string;
  source: string;
  toolDefinitions: readonly ToolDef[];
  signal?: AbortSignal;
  workspaceRoots?: readonly string[];
  limits?: Partial<ProgrammaticExecutionLimits>;
  invokeTool: (
    toolName: string,
    args: Record<string, unknown>,
    nestedToolCallId: string,
    signal: AbortSignal,
  ) => Promise<ProgrammaticNestedToolResult>;
}

export interface ProgrammaticToolContext {
  toolDefinitions: readonly ToolDef[];
  workspaceRoots?: readonly string[];
  invokeTool: ProgrammaticExecutionRequest["invokeTool"];
}

export interface ProgrammaticExecutionLimits {
  maxSourceBytes: number;
  maxWallMs: number;
  maxInstructionInterrupts: number;
  maxNestedCalls: number;
  maxConcurrentCalls: number;
  maxLogEntries: number;
  maxLogBytes: number;
  maxSerializedReturnBytes: number;
  maxPerCallArgumentBytes: number;
  maxRuntimeMemoryBytes: number;
  maxRuntimeStackBytes: number;
}

export interface ProgrammaticRuntimeCapabilities {
  provider: string;
  healthy: boolean;
  supportsAsync: boolean;
  reason?: string;
}

export interface ProgrammaticExecutionRuntime {
  capabilities(): Promise<ProgrammaticRuntimeCapabilities>;
  execute(request: ProgrammaticExecutionRequest): Promise<ProgrammaticExecutionResult>;
  cancel(executionId: string): boolean;
}

function createDefaultProgrammaticRuntime(): ProgrammaticExecutionRuntime {
  let instance: ProgrammaticExecutionRuntime | null = null;
  let loading: Promise<ProgrammaticExecutionRuntime> | null = null;
  const get = async (): Promise<ProgrammaticExecutionRuntime> => {
    if (instance) return instance;
    loading ??= import("./programmaticQuickJsRuntime").then(({ QuickJsProgrammaticRuntime }) => {
      instance = new QuickJsProgrammaticRuntime();
      return instance;
    });
    return loading;
  };
  return {
    capabilities: async () => (await get()).capabilities(),
    execute: async (request) => (await get()).execute(request),
    cancel: (executionId) => instance?.cancel(executionId) ?? false,
  };
}

/** Provider-neutral entry point used by the model-facing consumer. */
export class ProgrammaticExecutionService {
  private readonly runtime: ProgrammaticExecutionRuntime;

  constructor(runtime?: ProgrammaticExecutionRuntime) {
    this.runtime = runtime ?? createDefaultProgrammaticRuntime();
  }

  capabilities(): Promise<ProgrammaticRuntimeCapabilities> {
    return this.runtime.capabilities();
  }

  execute(request: ProgrammaticExecutionRequest): Promise<ProgrammaticExecutionResult> {
    return this.runtime.execute(request);
  }

  cancel(executionId: string): boolean {
    return this.runtime.cancel(executionId);
  }
}

export const programmaticExecutionService = new ProgrammaticExecutionService();

export function formatProgrammaticExecutionResult(result: ProgrammaticExecutionResult): string {
  try {
    return JSON.stringify(result);
  } catch {
    return JSON.stringify({
      executionId: result.executionId,
      status: "failed",
      logs: [],
      nestedCalls: [],
      durationMs: result.durationMs,
      failure: {
        category: "runtime_failure",
        message: "The program result could not be serialized.",
      },
    });
  }
}

export function parseProgrammaticExecutionResult(raw: string): ProgrammaticExecutionResult | null {
  try {
    const value: unknown = JSON.parse(unwrapUntrustedContent(raw));
    if (!value || typeof value !== "object") return null;
    const result = value as Partial<ProgrammaticExecutionResult>;
    if (
      typeof result.executionId !== "string" ||
      (result.status !== "succeeded" && result.status !== "failed" && result.status !== "cancelled") ||
      !Array.isArray(result.logs) ||
      !Array.isArray(result.nestedCalls) ||
      typeof result.durationMs !== "number"
    ) {
      return null;
    }
    return value as ProgrammaticExecutionResult;
  } catch {
    return null;
  }
}

export function isProgrammaticTool(name: string): boolean {
  return name === PROGRAMMATIC_TOOL_NAME;
}
