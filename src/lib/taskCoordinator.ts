export type ToolRoute = 'native' | 'browser' | 'shell' | 'workspace' | 'external';

export type CoordinationPhase = 'observe' | 'decide' | 'authorize' | 'execute' | 'verify';

export interface CoordinationDecision {
  route: ToolRoute;
  phases: CoordinationPhase[];
  maxAttempts: number;
  error?: string;
}

const NATIVE_PREFIX = 'computer_';
const BROWSER_TOOLS = new Set(['browser_click', 'browser_inspect', 'browser_navigate', 'web_fetch', 'web_search']);

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
    if (/^https?:\/\//i.test(target)) {
      return {
        route,
        phases: ['observe', 'decide', 'authorize'],
        maxAttempts: 0,
        error: 'Web targets must use browser tools; native Computer Use cannot drive a browser URL.',
      };
    }
  }
  return {
    route,
    phases: ['observe', 'decide', 'authorize', 'execute', 'verify'],
    maxAttempts: 1,
  };
}
