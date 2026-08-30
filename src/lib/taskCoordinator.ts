export type ToolRoute = 'native' | 'browser' | 'shell' | 'workspace' | 'external';

export type CoordinationPhase = 'observe' | 'decide' | 'authorize' | 'execute' | 'verify';

export interface CoordinationDecision {
  route: ToolRoute;
  phases: CoordinationPhase[];
  maxAttempts: number;
  error?: string;
}

export interface CoordinationHooks<T> {
  onPhase?: (phase: CoordinationPhase, attempt: number) => void | Promise<void>;
  execute: (attempt: number) => T | Promise<T>;
  verify?: (result: T, attempt: number) => boolean | Promise<boolean>;
  budget?: ComputerUseRunBudget;
}

export type ComputerUseFailureCode =
  | 'PRECONDITION_CHANGED'
  | 'STALE_OBSERVATION'
  | 'TARGET_NOT_FOUND'
  | 'PROVIDER_TRANSIENT_PRE_INPUT'
  | 'OPERATOR_DENIED'
  | 'APPROVAL_TIMEOUT'
  | 'SECURITY_REFUSED'
  | 'SESSION_PAUSED'
  | 'SESSION_STOPPED'
  | 'SESSION_REVOKED'
  | 'BUDGET_EXCEEDED'
  | 'INPUT_SENT_UNVERIFIED'
  | 'INPUT_MAY_HAVE_BEEN_SENT'
  | 'POSTCONDITION_FAILED'
  | 'PROVIDER_FAILURE'
  | 'UNKNOWN';

export type ComputerUseFailurePhase = 'observe' | 'authorize' | 'pre_execute' | 'execute' | 'verify';

export interface ComputerUseFailure {
  code: ComputerUseFailureCode;
  inputSent: boolean;
  safeToRetry: boolean;
  phase: ComputerUseFailurePhase;
  message: string;
}

export function isComputerUseFailure(value: unknown): value is ComputerUseFailure {
  if (!value || typeof value !== 'object') return false;
  const failure = value as Record<string, unknown>;
  return typeof failure.code === 'string'
    && typeof failure.inputSent === 'boolean'
    && typeof failure.safeToRetry === 'boolean'
    && typeof failure.phase === 'string'
    && typeof failure.message === 'string';
}

export function computerUseFailure(
  message: string,
  overrides: Partial<ComputerUseFailure> = {},
): ComputerUseFailure {
  return {
    code: 'UNKNOWN',
    inputSent: true,
    safeToRetry: false,
    phase: 'execute',
    message,
    ...overrides,
  };
}

export function parseComputerUseFailure(error: unknown): ComputerUseFailure {
  if (isComputerUseFailure(error)) return error;
  const text = error instanceof Error ? error.message : typeof error === 'string' ? error : '';
  try {
    const parsed: unknown = JSON.parse(text);
    if (isComputerUseFailure(parsed)) return parsed;
    if (parsed && typeof parsed === 'object' && isComputerUseFailure((parsed as Record<string, unknown>).failure)) {
      return (parsed as Record<string, unknown>).failure as ComputerUseFailure;
    }
  } catch {
    // Unknown invoke/IPC failures are fail-closed below.
  }
  return computerUseFailure(text || 'Unknown Computer Use execution failure');
}

export const COMPUTER_USE_BUDGET_DEFAULTS = {
  maxActions: 50,
  maxScreenshots: 12,
  // A native action may be retried once only after a pre-input/provider
  // failure. Retrying an action that may already have sent input is refused.
  maxRetries: 1,
  maxModelCalls: 20,
  deadlineMs: 15 * 60 * 1000,
} as const;

export interface ComputerUseRunBudgetOptions {
  maxActions?: number;
  maxScreenshots?: number;
  maxRetries?: number;
  maxModelCalls?: number;
  deadlineMs?: number;
}

export type ComputerUseBudgetCounter = 'actions' | 'screenshots' | 'retries' | 'model_calls';

/** One atomic budget shared by every Computer Use dispatcher in a run. */
export class ComputerUseRunBudget {
  private readonly limits: Record<ComputerUseBudgetCounter, number>;
  private readonly used: Record<ComputerUseBudgetCounter, number> = {
    actions: 0,
    screenshots: 0,
    retries: 0,
    model_calls: 0,
  };
  private deadline: number | null = null;
  private active = false;

  constructor(options: ComputerUseRunBudgetOptions = {}) {
    const configured = { ...COMPUTER_USE_BUDGET_DEFAULTS, ...options };
    this.limits = {
      actions: configured.maxActions,
      screenshots: configured.maxScreenshots,
      retries: configured.maxRetries,
      model_calls: configured.maxModelCalls,
    };
    this.deadlineMs = configured.deadlineMs;
  }

  private readonly deadlineMs: number;

  /** Start the Computer Use ceiling only when a native operation is entered. */
  activate(): void {
    if (!this.active) {
      this.active = true;
      this.deadline = Date.now() + this.deadlineMs;
    }
  }

  consume(counter: ComputerUseBudgetCounter): boolean {
    if (counter !== 'model_calls') this.activate();
    if (counter === 'model_calls' && !this.active) return true;
    if ((this.deadline !== null && Date.now() >= this.deadline) || this.used[counter] >= this.limits[counter]) return false;
    this.used[counter] += 1;
    return true;
  }

  remaining(counter: ComputerUseBudgetCounter): number {
    return Math.max(0, this.limits[counter] - this.used[counter]);
  }
}

export const INPUT_SENT_UNVERIFIED = 'INPUT_SENT_UNVERIFIED';

export class CoordinatedInvocationError extends Error {
  readonly failure: ComputerUseFailure;
  readonly code: ComputerUseFailureCode;

  constructor(failure: ComputerUseFailure = computerUseFailure(
    `${INPUT_SENT_UNVERIFIED}: input was sent but the requested postcondition was not verified`,
    { code: 'INPUT_SENT_UNVERIFIED', inputSent: true, safeToRetry: false, phase: 'verify' },
  )) {
    super(failure.message);
    this.failure = failure;
    this.code = failure.code;
    this.name = 'CoordinatedInvocationError';
  }
}

export class CoordinatedRetryableError extends Error {
  readonly failure: ComputerUseFailure;

  constructor(failure: ComputerUseFailure) {
    super(failure.message);
    this.failure = failure;
    this.name = 'CoordinatedRetryableError';
  }
}

function retryIsExplicitlySafe(error: unknown): error is CoordinatedRetryableError {
  const retryableCodes: ComputerUseFailureCode[] = [
    'PRECONDITION_CHANGED',
    'STALE_OBSERVATION',
    'TARGET_NOT_FOUND',
    'PROVIDER_TRANSIENT_PRE_INPUT',
  ];
  return error instanceof CoordinatedRetryableError
    && error.failure.safeToRetry === true
    && error.failure.inputSent === false
    && retryableCodes.includes(error.failure.code);
}

const NATIVE_PREFIX = 'computer_';
const BROWSER_TOOLS = new Set(['browser_click', 'browser_inspect', 'browser_navigate', 'web_fetch', 'web_search']);
const BROWSER_APPLICATION_IDS = new Set([
  'brave',
  'com.apple.safari',
  'com.brave.browser',
  'com.google.chrome',
  'com.google.chromium',
  'firefox',
  'google chrome',
  'chromium',
  'microsoft edge',
  'microsoft.microsoftedge',
  'msedge',
  'mozilla firefox',
  'org.chromium.chromium',
  'org.mozilla.firefox',
  'safari',
  'brave browser',
]);

/** Single routing authority for tools that can affect an external surface. */
export function coordinateToolInvocation(
  name: string,
  args: Record<string, unknown>,
): CoordinationDecision {
  const route: ToolRoute = name.startsWith(NATIVE_PREFIX)
    ? 'native'
    : BROWSER_TOOLS.has(name)
      ? 'browser'
      : name === 'run_shell'
        ? 'shell'
        : name.startsWith('mcp__') || name.startsWith('ext__')
          ? 'external'
          : 'workspace';
  if (route === 'native') {
    const target = typeof args.target_application_id === 'string' ? args.target_application_id : '';
    const normalizedTarget = target.trim().toLowerCase();
    const browserApplication = [...BROWSER_APPLICATION_IDS].some(
      (browserId) => normalizedTarget === browserId || normalizedTarget.startsWith(`${browserId}::`),
    );
    if (/^https?:\/\//i.test(target) || browserApplication) {
      return {
        route,
        phases: ['observe', 'decide', 'authorize'],
        maxAttempts: 0,
        error: 'Browser targets must use browser tools; native Computer Use cannot drive browser content or chrome.',
      };
    }
  }
  return {
    route,
    phases: ['observe', 'decide', 'authorize', 'execute', 'verify'],
    maxAttempts: route === 'native' ? 2 : 1,
  };
}

/** Executes the coordinator-owned lifecycle with a hard attempt budget. The
 * dispatcher supplies the native observe/authorize hooks; the tool backend
 * remains responsible for its own final target revalidation and postcondition
 * check. No caller may silently bypass these phases by invoking the tool
 * directly through this path.
 */
export async function runCoordinatedInvocation<T>(
  decision: CoordinationDecision,
  hooks: CoordinationHooks<T>,
): Promise<T> {
  if (decision.error) throw new Error(decision.error);
  const attempts = Math.max(1, Math.min(decision.maxAttempts, 2));
  let result!: T;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    if (attempt > 1 && hooks.budget && !hooks.budget.consume('retries')) {
      throw new Error('COMPUTER_USE_BUDGET_EXCEEDED: retry limit reached');
    }
    try {
      for (const phase of decision.phases) {
        if (phase === 'execute') {
          result = await hooks.execute(attempt);
        } else {
          await hooks.onPhase?.(phase, attempt);
        }
      }
      if (!hooks.verify || await hooks.verify(result, attempt)) return result;
      throw new CoordinatedInvocationError(computerUseFailure(
        `${INPUT_SENT_UNVERIFIED}: input was sent but the requested postcondition was not verified`,
        {
          code: 'POSTCONDITION_FAILED',
          inputSent: true,
          safeToRetry: false,
          phase: 'verify',
        },
      ));
    } catch (error) {
      if (!retryIsExplicitlySafe(error) || attempt >= attempts) throw error;
    }
  }
  throw new CoordinatedInvocationError();
}
