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
    maxAttempts: 1,
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
    for (const phase of decision.phases) {
      if (phase === 'execute') {
        result = await hooks.execute(attempt);
      } else {
        await hooks.onPhase?.(phase, attempt);
      }
    }
    if (!hooks.verify || await hooks.verify(result, attempt) || attempt === attempts) return result;
  }
  return result;
}
