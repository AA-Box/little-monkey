/**
 * Advisory LLM-judged risk classification for mutating tool calls — Layer 2
 * of the Plan/Act + risk-adaptive-permissions design (Phase 2; see
 * docs/roadmap/p2-plan-act-safety.md). Classifies a `write_file`/`edit_file`/
 * `run_shell` call as low/medium/high risk with a short reason, purely to
 * annotate the permission prompt shown to the user (see `turnEngine.ts`'s
 * `executeToolCall`, which injects the result as frontend-owned
 * `risk_level`/`risk_reason` tool-call arguments) — nothing in this module
 * ever approves or denies a tool call itself. Layer 1, the deterministic
 * `path_risk_floor` in `src-tauri/src/permissions.rs`, is authoritative and
 * always overrides whatever this module produces; and `run_shell` in
 * particular can never be auto-approved off the back of a classification
 * from here, in any mode, at any phase — see that module's doc comment for
 * why (a shell substring blacklist was previously removed from this
 * codebase for exactly this prompt-injection-shaped reason).
 *
 * DEPENDENCY-INJECTED rather than importing `attemptStream` from
 * `turnEngine.ts` directly — exactly the same shape as `contextTrimmer.ts`'s
 * `sendForSummary` option, which likewise doesn't import `attemptStream`
 * itself. This is necessary, not just stylistic: `turnEngine.ts`'s
 * `executeToolCall` is what calls into this module (to compute the
 * `risk_level`/`risk_reason` it injects), and `attemptStream` is defined IN
 * `turnEngine.ts` — importing it here would create a module cycle
 * (`turnEngine.ts` -> `riskJudge.ts` -> `turnEngine.ts`). `agentLoop.ts`
 * builds the actual `callModel` closure around `attemptStream` (the same
 * closure shape as its own `sendForSummary`) and threads it through
 * `turnEngine.ts`'s `RiskAnnotationContext` down into `executeToolCall`.
 */
import type { ChatMessage } from './llamaClient';

export interface RiskClassification {
  level: 'low' | 'medium' | 'high';
  reason: string;
}

/** The minimal subset of `turnEngine.ts`'s `AttemptResult` this module
 * actually needs from `callModel` — deliberately not the whole shape, so this
 * module stays decoupled from `turnEngine.ts`'s types (see the module doc
 * comment on why no import from there exists at all). */
export interface JudgeCallResult {
  content: string;
  streamError: string | null;
}

/** Hard timeout for the one-shot judge call — a slow/hung local model must
 * never stall a permission prompt for longer than a few seconds; the tool
 * call always falls through to a normal, unannotated prompt if this fires. */
export const JUDGE_TIMEOUT_MS = 8000;

/** Cap on how much of the tool call's arguments are inlined into the judge
 * prompt, so one huge `write_file` `content` can't blow up the request. */
const MAX_ARGS_CHARS = 2000;

function truncate(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/**
 * Renders a `(tool, args)` pair into a deterministic cache key so a turn that
 * repeats an identical call (e.g. the model retries the exact same edit after
 * a denial) doesn't pay for a second judge round trip — callers own the
 * actual cache (see `turnEngine.ts`'s `RiskAnnotationContext.cache`), this is
 * just the key function.
 */
export function riskCacheKey(tool: string, args: Record<string, unknown>): string {
  try {
    return `${tool}:${JSON.stringify(args)}`;
  } catch {
    return `${tool}:${String(args)}`;
  }
}

function buildJudgeMessages(tool: string, args: Record<string, unknown>, workspaceRoot: string): ChatMessage[] {
  let argsText: string;
  try {
    argsText = JSON.stringify(args);
  } catch {
    argsText = String(args);
  }
  argsText = truncate(argsText, MAX_ARGS_CHARS);

  return [
    {
      role: 'system',
      content:
        'You are a security risk classifier for an autonomous coding agent\'s tool calls, running as a strict, non-conversational judge. ' +
        'Classify the risk of the single tool call below as "low", "medium", or "high" — consider whether it touches secrets/credentials, ' +
        'CI or build configuration, shell startup files, or other sensitive paths; whether it could exfiltrate data or execute untrusted code; ' +
        'or whether it is a routine, easily reversible source-code change. ' +
        'Reply with ONLY a single-line JSON object of the exact shape {"level":"low","reason":"..."} ' +
        '(level is exactly "low", "medium", or "high"; reason is one short sentence) — no markdown, no other text.',
    },
    {
      role: 'user',
      content: `Workspace root: ${workspaceRoot || '(unknown)'}\nTool: ${tool}\nArguments: ${argsText}`,
    },
  ];
}

/**
 * Strict parse of the judge's reply: anything that isn't exactly
 * `{level: 'low'|'medium'|'high', reason: string}` — extra prose, a missing
 * or blank reason, an out-of-enum level, unparseable JSON — returns `null`.
 * Fails closed: `null` is treated as "unknown" everywhere it's consumed,
 * which is the SAME outcome as risk annotations being off entirely (a normal
 * permission prompt with no badge), never silently treated as "low risk".
 * Tries the raw trimmed content first, then falls back to the first
 * `{...}` span found in it (small local models sometimes wrap otherwise
 * valid JSON in a sentence or code fence) — still strict about the shape
 * once parsed, never about surrounding prose.
 */
export function parseJudgeResponse(content: string): RiskClassification | null {
  const candidates = [content.trim()];
  const embedded = content.match(/\{[\s\S]*\}/);
  if (embedded) candidates.push(embedded[0]);

  for (const candidate of candidates) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(candidate);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const level = (parsed as { level?: unknown }).level;
    const reason = (parsed as { reason?: unknown }).reason;
    if ((level === 'low' || level === 'medium' || level === 'high') && typeof reason === 'string' && reason.trim().length > 0) {
      return { level, reason: reason.trim() };
    }
  }
  return null;
}

/**
 * Classifies a mutating tool call's risk via one one-shot, non-streaming,
 * tool-less `callModel` invocation — reuses the exact transport
 * `agentLoop.ts`'s `sendForSummary` uses (`attemptStream` against the
 * currently active target), just dependency-injected here instead of
 * imported (see this module's top doc comment for why). Fails closed on
 * anything malformed, errored, or slower than `JUDGE_TIMEOUT_MS`: every one
 * of those cases resolves `null`, never a fabricated classification.
 *
 * `signal` (the turn's own abort signal, if any) is raced against the
 * judge's own timeout so the user's Stop button cancels an in-flight judge
 * call exactly like it cancels everything else `executeToolCall` awaits.
 */
export async function classifyToolCall(
  tool: string,
  args: Record<string, unknown>,
  workspaceRoot: string,
  callModel: (messages: ChatMessage[], signal: AbortSignal) => Promise<JudgeCallResult>,
  signal?: AbortSignal
): Promise<RiskClassification | null> {
  const timeoutController = new AbortController();
  const timeoutId = setTimeout(() => timeoutController.abort(), JUDGE_TIMEOUT_MS);
  const onParentAbort = () => timeoutController.abort();
  if (signal) {
    if (signal.aborted) timeoutController.abort();
    else signal.addEventListener('abort', onParentAbort, { once: true });
  }

  try {
    const result = await callModel(buildJudgeMessages(tool, args, workspaceRoot), timeoutController.signal);
    if (result.streamError) return null;
    return parseJudgeResponse(result.content);
  } catch {
    return null;
  } finally {
    clearTimeout(timeoutId);
    signal?.removeEventListener('abort', onParentAbort);
  }
}
