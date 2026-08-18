import type {
  QuickJSContext,
  QuickJSDeferredPromise,
  QuickJSHandle,
  QuickJSRuntime,
  QuickJSWASMModule,
} from "quickjs-emscripten";
import { isFail } from "quickjs-emscripten";
import {
  formatProgrammaticExecutionResult,
  PROGRAMMATIC_TOOL_NAME,
  type ProgrammaticExecutionLimits,
  type ProgrammaticExecutionRequest,
  type ProgrammaticExecutionResult,
  type ProgrammaticFailure,
  type ProgrammaticNestedCallEvidence,
  type ProgrammaticNestedToolResult,
  type ProgrammaticRuntimeCapabilities,
  type ProgrammaticExecutionRuntime,
} from "./programmaticExecution";
import { redactPrivatePaths, redactSensitiveText, sanitizeToolArguments } from "./durableRun";

export const DEFAULT_PROGRAMMATIC_LIMITS: ProgrammaticExecutionLimits = {
  maxSourceBytes: 64 * 1024,
  maxWallMs: 30_000,
  maxInstructionInterrupts: 10_000,
  maxNestedCalls: 32,
  maxConcurrentCalls: 4,
  maxLogEntries: 200,
  maxLogBytes: 64 * 1024,
  maxSerializedReturnBytes: 256 * 1024,
  maxPerCallArgumentBytes: 64 * 1024,
  maxRuntimeMemoryBytes: 64 * 1024 * 1024,
  maxRuntimeStackBytes: 1024 * 1024,
};

const PROGRAM_INVALID_RESULT = "__PROGRAM_INVALID_RESULT__";
const PROGRAM_LOG_LIMIT = "__PROGRAM_LOG_LIMIT__";
const PROGRAM_OUTPUT_LIMIT = "__PROGRAM_OUTPUT_LIMIT__";
const PROGRAM_RUNTIME_FAILURE = "__PROGRAM_RUNTIME_FAILURE__";

type QuickJSModule = QuickJSWASMModule;

interface ActiveExecution {
  controller: AbortController;
  cancelRequested: boolean;
}

interface StopReason {
  kind: "cancelled" | "timeout" | "budget";
}

type ResolvedPromise =
  | { value: QuickJSHandle; error?: undefined; dispose: () => void }
  | { error: QuickJSHandle; value?: undefined; dispose: () => void };

function byteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function mergeLimits(overrides: Partial<ProgrammaticExecutionLimits> | undefined): ProgrammaticExecutionLimits {
  const merged = { ...DEFAULT_PROGRAMMATIC_LIMITS, ...(overrides ?? {}) };
  for (const [key, value] of Object.entries(merged)) {
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new Error(`Invalid programmatic execution limit ${key}.`);
    }
  }
  if (merged.maxConcurrentCalls > merged.maxNestedCalls) {
    merged.maxConcurrentCalls = merged.maxNestedCalls;
  }
  return merged;
}

function boundedText(value: string, maxBytes: number): string {
  if (byteLength(value) <= maxBytes) return value;
  let output = "";
  let bytes = 0;
  for (const point of value) {
    const size = byteLength(point);
    if (bytes + size > maxBytes) break;
    output += point;
    bytes += size;
  }
  return `${output}\n[TRUNCATED]`;
}

function safeErrorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

function parseToolContent(content: string): unknown {
  try {
    return JSON.parse(content);
  } catch {
    return content;
  }
}

interface JsonSchemaLike {
  type?: string | string[];
  enum?: unknown[];
  properties?: Record<string, JsonSchemaLike>;
  required?: string[];
  additionalProperties?: boolean;
  items?: JsonSchemaLike;
  minLength?: number;
  maxLength?: number;
  minimum?: number;
  maximum?: number;
}

function jsonEqual(left: unknown, right: unknown): boolean {
  try {
    return JSON.stringify(left) === JSON.stringify(right);
  } catch {
    return false;
  }
}

/** Validates against the exact schema offered to the model; no second schema
 * or coercion path is introduced for programmatic calls. */
function validateToolArguments(value: unknown, schema: object): string | null {
  const visit = (entry: unknown, current: JsonSchemaLike, path: string): string | null => {
    if (current.enum && !current.enum.some((candidate) => jsonEqual(candidate, entry))) {
      return `${path} must match one of the offered values.`;
    }
    const types = current.type ? (Array.isArray(current.type) ? current.type : [current.type]) : [];
    if (types.length > 0) {
      const matches = types.some((type) => {
        switch (type) {
          case "object": return entry !== null && typeof entry === "object" && !Array.isArray(entry);
          case "array": return Array.isArray(entry);
          case "string": return typeof entry === "string";
          case "boolean": return typeof entry === "boolean";
          case "number": return typeof entry === "number" && Number.isFinite(entry);
          case "integer": return typeof entry === "number" && Number.isSafeInteger(entry);
          case "null": return entry === null;
          default: return true;
        }
      });
      if (!matches) return `${path} has the wrong type.`;
    }
    if (typeof entry === "string") {
      if (current.minLength !== undefined && entry.length < current.minLength) return `${path} is too short.`;
      if (current.maxLength !== undefined && entry.length > current.maxLength) return `${path} is too long.`;
    }
    if (typeof entry === "number") {
      if (current.minimum !== undefined && entry < current.minimum) return `${path} is below the minimum.`;
      if (current.maximum !== undefined && entry > current.maximum) return `${path} is above the maximum.`;
    }
    if (Array.isArray(entry) && current.items) {
      for (let index = 0; index < entry.length; index += 1) {
        const error = visit(entry[index], current.items, `${path}[${index}]`);
        if (error) return error;
      }
    }
    if (entry && typeof entry === "object" && !Array.isArray(entry)) {
      const object = entry as Record<string, unknown>;
      for (const required of current.required ?? []) {
        if (!(required in object)) return `${path}.${required} is required.`;
      }
      const properties = current.properties ?? {};
      for (const [key, child] of Object.entries(object)) {
        if (current.additionalProperties === false && !(key in properties)) return `${path}.${key} is not allowed.`;
        const error = properties[key] ? visit(child, properties[key], `${path}.${key}`) : null;
        if (error) return error;
      }
    }
    return null;
  };

  return visit(value, schema as JsonSchemaLike, "arguments");
}

function redactEvidenceValue(value: unknown, workspaceRoots: readonly string[]): unknown {
  if (typeof value === "string") {
    return boundedText(redactPrivatePaths(redactSensitiveText(value), workspaceRoots), 4_000);
  }
  if (Array.isArray(value)) return value.slice(0, 64).map((entry) => redactEvidenceValue(entry, workspaceRoots));
  if (value && typeof value === "object") {
    const output: Record<string, unknown> = {};
    for (const [key, entry] of Object.entries(value).slice(0, 64)) {
      output[key] = redactEvidenceValue(entry, workspaceRoots);
    }
    return output;
  }
  return value;
}

function failure(
  category: ProgrammaticFailure["category"],
  message: string,
  toolName?: string,
): ProgrammaticFailure {
  return { category, message: boundedText(message, 1_000), ...(toolName ? { toolName } : {}) };
}

function nestedFailureContent(result: ProgrammaticNestedToolResult, toolName: string): ProgrammaticFailure {
  if (result.failure) return result.failure;
  if (result.cancelled) return failure("cancelled", "Nested tool call was cancelled.", toolName);
  return failure("nested_tool_failure", "The nested tool call failed.", toolName);
}

function toPromiseError(result: ProgrammaticFailure): string {
  return JSON.stringify({
    category: result.category,
    message: result.message,
    toolName: result.toolName,
  });
}

function quickJsErrorMessage(context: QuickJSContext, handle: QuickJSHandle): string {
  try {
    const dumped = context.dump(handle);
    if (typeof dumped === "string") return dumped;
    if (dumped && typeof dumped === "object" && "message" in dumped) {
      const error = dumped as { name?: unknown; message: unknown };
      const message = String(error.message);
      return error.name === "SyntaxError" ? `SyntaxError: ${message}` : message;
    }
    return String(dumped);
  } catch {
    return "The embedded runtime returned an unreadable error.";
  }
}

function classifyRuntimeFailure(message: string, stop: StopReason | null): ProgrammaticFailure {
  if (stop?.kind === "cancelled") return failure("cancelled", "Program execution was cancelled.");
  if (stop?.kind === "timeout") return failure("execution_timeout", "Program execution exceeded its wall-clock limit.");
  if (stop?.kind === "budget") return failure("execution_budget", "Program execution exceeded its instruction budget.");
  if (message.includes(PROGRAM_INVALID_RESULT)) {
    return failure("invalid_result", message.replace(PROGRAM_INVALID_RESULT, "").trim());
  }
  if (message.includes(PROGRAM_LOG_LIMIT) || message.includes(PROGRAM_OUTPUT_LIMIT)) {
    return failure("output_limit", message);
  }
  if (message.includes(PROGRAM_RUNTIME_FAILURE)) {
    return failure("runtime_failure", message.replace(PROGRAM_RUNTIME_FAILURE, "").trim());
  }
  if (/^SyntaxError\b/.test(message)) {
    return failure("invalid_source", message);
  }
  return failure("runtime_exception", message || "The program raised an exception.");
}

function createProgramSource(source: string, toolNames: readonly string[]): string {
  const tools = toolNames
    .map(
      (name) =>
        `${JSON.stringify(name)}: async (args) => JSON.parse(await __lmInvoke(${JSON.stringify(name)}, __lmJson(args)))`,
    )
    .join(",");
  const programBody = JSON.stringify(`"use strict"; return (async () => {
${source}
})();`);
  return `(async () => {
  const __lmSeen = new WeakSet();
  const __lmJson = (value) => {
    const visit = (entry) => {
      if (entry === null || typeof entry === "string" || typeof entry === "boolean") return entry;
      if (typeof entry === "number") {
        if (!Number.isFinite(entry)) throw new TypeError("${PROGRAM_INVALID_RESULT} non-finite number");
        return entry;
      }
      if (typeof entry !== "object") throw new TypeError("${PROGRAM_INVALID_RESULT} unsupported value");
      if (__lmSeen.has(entry)) throw new TypeError("${PROGRAM_INVALID_RESULT} cyclic value");
      __lmSeen.add(entry);
      if (Array.isArray(entry)) {
        const array = entry.map(visit);
        __lmSeen.delete(entry);
        return array;
      }
      const prototype = Object.getPrototypeOf(entry);
      if (prototype !== Object.prototype && prototype !== null) throw new TypeError("${PROGRAM_INVALID_RESULT} unsupported object");
      const object = {};
      for (const key of Object.keys(entry)) object[key] = visit(entry[key]);
      __lmSeen.delete(entry);
      return object;
    };
    const serialized = JSON.stringify(visit(value));
    if (serialized === undefined) throw new TypeError("${PROGRAM_INVALID_RESULT} undefined result");
    return serialized;
  };
  const __lmInvoke = globalThis.__lmInvoke;
  const __lmLog = globalThis.__lmLog;
  delete globalThis.__lmInvoke;
  delete globalThis.__lmLog;
  const console = Object.freeze({
    log: (...args) => __lmLog(__lmJson(args)),
    info: (...args) => __lmLog(__lmJson(args)),
    warn: (...args) => __lmLog(__lmJson(args)),
    error: (...args) => __lmLog(__lmJson(args)),
  });
  const tools = Object.freeze(Object.assign(Object.create(null), {${tools}}));
  const __lmProgram = Function("tools", "console", ${programBody});
  const __lmResult = await __lmProgram(tools, console);
  return __lmJson(__lmResult);
})()`;
}

/** QuickJS compiled to WebAssembly: no standard library or host APIs are linked. */
export class QuickJsProgrammaticRuntime implements ProgrammaticExecutionRuntime {
  private modulePromise: Promise<QuickJSModule> | null = null;
  private capabilityPromise: Promise<ProgrammaticRuntimeCapabilities> | null = null;
  private readonly active = new Map<string, ActiveExecution>();

  private loadModule(): Promise<QuickJSModule> {
    this.modulePromise ??= import("quickjs-emscripten").then(({ getQuickJS }) => getQuickJS());
    return this.modulePromise;
  }

  async capabilities(): Promise<ProgrammaticRuntimeCapabilities> {
    this.capabilityPromise ??= this.loadModule()
      .then((QuickJS) => {
        const runtime = QuickJS.newRuntime();
        const context = runtime.newContext();
        context.dispose();
        runtime.dispose();
        return { provider: "quickjs-wasm", healthy: true, supportsAsync: true };
      })
      .catch((error) => ({
        provider: "quickjs-wasm",
        healthy: false,
        supportsAsync: true,
        reason: `Embedded runtime unavailable: ${safeErrorMessage(error)}`,
      }));
    return this.capabilityPromise;
  }

  cancel(executionId: string): boolean {
    const active = this.active.get(executionId);
    if (!active) return false;
    active.cancelRequested = true;
    active.controller.abort();
    return true;
  }

  async execute(request: ProgrammaticExecutionRequest): Promise<ProgrammaticExecutionResult> {
    const startedAt = Date.now();
    const limits = mergeLimits(request.limits);
    const executionId = request.executionId;
    const workspaceRoots = request.workspaceRoots ?? [];
    const nestedCalls: ProgrammaticNestedCallEvidence[] = [];
    const logs: string[] = [];
    const controller = new AbortController();
    const activeSignal = controller.signal;
    const activeExecution: ActiveExecution = {
      controller,
      cancelRequested: request.signal?.aborted === true,
    };
    const onAbort = () => {
      activeExecution.cancelRequested = true;
      controller.abort();
    };
    request.signal?.addEventListener("abort", onAbort, { once: true });
    this.active.set(executionId, activeExecution);

    if (request.signal?.aborted) controller.abort();

    try {
      const sourceBytes = byteLength(request.source);
      if (sourceBytes === 0 || sourceBytes > limits.maxSourceBytes) {
        return {
          executionId,
          status: "failed",
          logs,
          nestedCalls,
          durationMs: Date.now() - startedAt,
          failure: failure("invalid_source", `Program source must be between 1 and ${limits.maxSourceBytes} bytes.`),
        };
      }
      if (activeSignal.aborted) {
        return {
          executionId,
          status: "cancelled",
          logs,
          nestedCalls,
          durationMs: Date.now() - startedAt,
          failure: failure("cancelled", "Program execution was cancelled."),
        };
      }

      const QuickJS = await this.loadModule();
      return await this.executeInRuntime(
        QuickJS,
        request,
        limits,
        controller,
        activeSignal,
        startedAt,
        workspaceRoots,
        nestedCalls,
        logs,
      );
    } catch (error) {
      const stop = activeExecution.cancelRequested
        ? { kind: "cancelled" as const }
        : null;
      return {
        executionId,
        status: stop ? "cancelled" : "failed",
        logs,
        nestedCalls,
        durationMs: Date.now() - startedAt,
        failure: stop ? failure("cancelled", "Program execution was cancelled.") : failure("runtime_failure", safeErrorMessage(error)),
      };
    } finally {
      request.signal?.removeEventListener("abort", onAbort);
      this.active.delete(executionId);
    }
  }

  private async executeInRuntime(
    QuickJS: QuickJSModule,
    request: ProgrammaticExecutionRequest,
    limits: ProgrammaticExecutionLimits,
    controller: AbortController,
    signal: AbortSignal,
    startedAt: number,
    workspaceRoots: readonly string[],
    nestedCalls: ProgrammaticNestedCallEvidence[],
    logs: string[],
  ): Promise<ProgrammaticExecutionResult> {
    const runtime: QuickJSRuntime = QuickJS.newRuntime();
    runtime.setMemoryLimit(limits.maxRuntimeMemoryBytes);
    runtime.setMaxStackSize(limits.maxRuntimeStackBytes);
    const deadline = startedAt + limits.maxWallMs;
    let interruptCount = 0;
    let stopReason: StopReason | null = signal.aborted ? { kind: "cancelled" } : null;
    runtime.setInterruptHandler(() => {
      interruptCount += 1;
      if (signal.aborted) {
        stopReason = { kind: "cancelled" };
        return true;
      }
      if (Date.now() >= deadline) {
        stopReason = { kind: "timeout" };
        controller.abort();
        return true;
      }
      if (interruptCount > limits.maxInstructionInterrupts) {
        stopReason = { kind: "budget" };
        controller.abort();
        return true;
      }
      return false;
    });

    const context: QuickJSContext = runtime.newContext();
    const inFlight = new Set<Promise<void>>();
    const deferredPromises = new Set<QuickJSDeferredPromise>();
    let nestedCount = 0;
    let activeNested = 0;
    const waiters: Array<() => void> = [];

    const releaseSlot = () => {
      activeNested = Math.max(0, activeNested - 1);
      const next = waiters.shift();
      if (next) {
        activeNested += 1;
        next();
      }
    };
    const acquireSlot = async () => {
      if (signal.aborted) return false;
      if (activeNested < limits.maxConcurrentCalls) {
        activeNested += 1;
        return true;
      }
      await new Promise<void>((resolve) => waiters.push(resolve));
      return !signal.aborted;
    };

    const logHandle = context.newFunction("__lmLog", (serializedHandle) => {
      if (logs.length >= limits.maxLogEntries) {
        throw new Error(`${PROGRAM_LOG_LIMIT} maximum log entries exceeded`);
      }
      const serialized = context.getString(serializedHandle);
      if (byteLength(serialized) > limits.maxLogBytes) {
        throw new Error(`${PROGRAM_LOG_LIMIT} maximum log bytes exceeded`);
      }
      const remaining = limits.maxLogBytes - logs.reduce((sum, entry) => sum + byteLength(entry), 0);
      if (byteLength(serialized) > remaining) {
        throw new Error(`${PROGRAM_LOG_LIMIT} maximum log bytes exceeded`);
      }
      logs.push(
        boundedText(
          redactPrivatePaths(redactSensitiveText(JSON.stringify(parseToolContent(serialized))), workspaceRoots),
          8_000,
        ),
      );
      return context.undefined;
    });
    context.setProp(context.global, "__lmLog", logHandle);
    logHandle.dispose();

    const invokeHandle = context.newFunction("__lmInvoke", (nameHandle, argsHandle) => {
      const toolName = context.getString(nameHandle);
      const serializedArgs = context.getString(argsHandle);
      if (byteLength(serializedArgs) > limits.maxPerCallArgumentBytes) {
        throw new Error(`${PROGRAM_OUTPUT_LIMIT} nested tool arguments exceed the per-call limit`);
      }

      let args: Record<string, unknown>;
      try {
        const parsed: unknown = JSON.parse(serializedArgs);
        if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("arguments must be an object");
        args = parsed as Record<string, unknown>;
      } catch (error) {
        throw new Error(`${PROGRAM_INVALID_RESULT} invalid nested tool arguments: ${safeErrorMessage(error)}`);
      }

      const definition = request.toolDefinitions.find((candidate) => candidate.function.name === toolName);
      const schemaError = definition ? validateToolArguments(args, definition.function.parameters) : null;

      const offered = request.toolDefinitions.some(
        (definition) => definition.function.name === toolName && definition.function.name !== PROGRAMMATIC_TOOL_NAME,
      );
      const evidence: ProgrammaticNestedCallEvidence = {
        id: `${request.executionId}:nested:${nestedCount + 1}`,
        toolName,
        arguments: sanitizeToolArguments(serializedArgs, workspaceRoots).value,
        status: "running",
      };
      nestedCalls.push(evidence);
      nestedCount += 1;
      const deferred: QuickJSDeferredPromise = context.newPromise();
      deferredPromises.add(deferred);

      const settleFailure = (nestedFailure: ProgrammaticFailure) => {
        evidence.status = nestedFailure.category === "cancelled" ? "cancelled" : "failed";
        evidence.failure = nestedFailure;
        const errorHandle = context.newError(toPromiseError(nestedFailure));
        deferred.reject(errorHandle);
        errorHandle.dispose();
        void deferred.settled.then(() => {
          deferred.dispose();
          deferredPromises.delete(deferred);
        });
      };

      if (!offered) {
        settleFailure(failure("nested_tool_failure", `Tool "${toolName}" was not offered this turn.`, toolName));
        return deferred.handle;
      }
      if (toolName === PROGRAMMATIC_TOOL_NAME) {
        settleFailure(failure("nested_tool_failure", "Recursive programmatic execution is not allowed.", toolName));
        return deferred.handle;
      }
      if (nestedCount > limits.maxNestedCalls) {
        settleFailure(failure("execution_budget", "The nested tool-call limit was exceeded.", toolName));
        return deferred.handle;
      }
      if (schemaError) {
        settleFailure(failure("nested_tool_failure", `Invalid arguments: ${schemaError}`, toolName));
        return deferred.handle;
      }

      const nestedToolCallId = evidence.id;
      const work = (async () => {
        const acquired = await acquireSlot();
        if (!acquired) {
          settleFailure(failure("cancelled", "Nested tool call was cancelled before it started.", toolName));
          return;
        }
        const nestedStartedAt = Date.now();
        try {
          const result = await request.invokeTool(toolName, args, nestedToolCallId, signal);
          evidence.durationMs = Date.now() - nestedStartedAt;
          if (result.failure || result.cancelled) {
            settleFailure(nestedFailureContent(result, toolName));
            return;
          }
          const value = parseToolContent(result.content);
          const serializedValue = JSON.stringify(value);
          if (serializedValue === undefined || byteLength(serializedValue) > limits.maxSerializedReturnBytes) {
            settleFailure(failure("output_limit", "Nested tool result exceeded the serialized return limit.", toolName));
            return;
          }
          evidence.status = "succeeded";
          evidence.result = redactEvidenceValue(value, workspaceRoots);
          const guestValue = context.newString(serializedValue);
          deferred.resolve(guestValue);
          guestValue.dispose();
          void deferred.settled.then(() => {
            deferred.dispose();
            deferredPromises.delete(deferred);
          });
        } catch (error) {
          evidence.durationMs = Date.now() - nestedStartedAt;
          settleFailure(failure("nested_tool_failure", safeErrorMessage(error), toolName));
        } finally {
          releaseSlot();
        }
      })();
      inFlight.add(work);
      void work.finally(() => inFlight.delete(work));
      return deferred.handle;
    });
    context.setProp(context.global, "__lmInvoke", invokeHandle);
    invokeHandle.dispose();

    let promiseHandle: QuickJSHandle | null = null;
    let resolution: ReturnType<QuickJSContext["resolvePromise"]> | null = null;
    let resolved: ResolvedPromise | null = null;
    let resolutionDone = false;
    try {
      const evaluated = context.evalCode(createProgramSource(
        request.source,
        request.toolDefinitions
          .map((definition) => definition.function.name)
          .filter((name) => name !== PROGRAMMATIC_TOOL_NAME),
      ));
      promiseHandle = context.unwrapResult(evaluated);
      resolution = context.resolvePromise(promiseHandle);
      void resolution.then((value) => {
        resolved = value;
        resolutionDone = true;
      });

      while (!resolutionDone) {
        if (signal.aborted) {
          stopReason ??= { kind: "cancelled" };
          controller.abort();
          break;
        }
        if (Date.now() >= deadline) {
          stopReason = { kind: "timeout" };
          controller.abort();
          break;
        }
        const jobs = runtime.executePendingJobs(256);
        if (isFail(jobs)) {
          const message = quickJsErrorMessage(context, jobs.error);
          jobs.error.dispose();
          throw new Error(message);
        }
        await new Promise((resolveDelay) => setTimeout(resolveDelay, 0));
      }

      if (!resolutionDone && !signal.aborted && !stopReason) {
        await Promise.race([
          resolution,
          new Promise((resolveDelay) => setTimeout(resolveDelay, Math.max(1, deadline - Date.now()))),
        ]);
      }
      await Promise.allSettled([...inFlight]);

      if (stopReason || signal.aborted) {
        const stopped = stopReason ?? { kind: "cancelled" as const };
        return {
          executionId: request.executionId,
          status: stopped.kind === "cancelled" ? "cancelled" : "failed",
          logs,
          nestedCalls,
          durationMs: Date.now() - startedAt,
          failure: classifyRuntimeFailure("", stopped),
        };
      }
      if (!resolved) throw new Error(`${PROGRAM_RUNTIME_FAILURE} Program promise did not settle.`);
      const resolvedRecord = resolved as unknown as {
        value?: QuickJSHandle;
        error?: QuickJSHandle;
        dispose?: () => void;
      };
      if (resolvedRecord.error) {
        const message = quickJsErrorMessage(context, resolvedRecord.error);
        const parsedFailure = (() => {
          try {
            const parsed = JSON.parse(message) as ProgrammaticFailure;
            return parsed.category && parsed.message ? parsed : null;
          } catch {
            return null;
          }
        })();
        const programFailure = parsedFailure ?? classifyRuntimeFailure(message, stopReason);
        resolvedRecord.dispose?.();
        return {
          executionId: request.executionId,
          status: programFailure.category === "cancelled" ? "cancelled" : "failed",
          logs,
          nestedCalls,
          durationMs: Date.now() - startedAt,
          failure: programFailure,
        };
      }
      if (!resolvedRecord.value) throw new Error(`${PROGRAM_RUNTIME_FAILURE} Program promise returned no value.`);
      const serializedResult = context.getString(resolvedRecord.value);
      resolvedRecord.dispose?.();
      if (byteLength(serializedResult) > limits.maxSerializedReturnBytes) {
        return {
          executionId: request.executionId,
          status: "failed",
          logs,
          nestedCalls,
          durationMs: Date.now() - startedAt,
          failure: failure("output_limit", "Program return value exceeded the serialized return limit."),
        };
      }
      return {
        executionId: request.executionId,
        status: "succeeded",
        value: JSON.parse(serializedResult),
        logs,
        nestedCalls,
        durationMs: Date.now() - startedAt,
      };
    } catch (error) {
      const message = safeErrorMessage(error);
      const programFailure = classifyRuntimeFailure(message, stopReason ?? (signal.aborted ? { kind: "cancelled" } : null));
      return {
        executionId: request.executionId,
        status: programFailure.category === "cancelled" ? "cancelled" : "failed",
        logs,
        nestedCalls,
        durationMs: Date.now() - startedAt,
        failure: programFailure,
      };
    } finally {
      controller.abort();
      for (const resolve of waiters.splice(0)) resolve();
      await Promise.allSettled([...inFlight]);
      for (const deferred of deferredPromises) deferred.dispose();
      deferredPromises.clear();
      resolution = null;
      promiseHandle?.dispose();
      context.dispose();
      runtime.dispose();
    }
  }
}

export function serializeProgrammaticExecutionResult(result: ProgrammaticExecutionResult): string {
  return formatProgrammaticExecutionResult(result);
}
