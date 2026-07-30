import type { PermissionMode } from '../store/permissionStore';

export const WORKSPACE_MUTATION_CORRECTION =
  '[Workspace mutation required] The user explicitly asked you to change files in the open workspace. Your previous chat-only response was discarded because no file was changed. Inspect the workspace as needed, then use edit_file or write_file to make the requested change. A code block in chat is not a substitute for editing the real file. If a tool is unavailable or permission is denied, say so and do not claim that files changed.';

export const WORKSPACE_MUTATION_FAILURE =
  'No files changed. The selected model did not successfully call write_file or edit_file after one corrective retry. Select a tool-capable model and try again.';

const READ_ONLY_VETOES = [
  /\bread[\s-]*only\b/,
  /\b(?:do not|don't|dont|never)\s+(?:make|apply)\s+(?:any\s+)?(?:changes?|edits?|modifications?)\b/,
  /\b(?:do not|don't|dont|never)\s+(?:write|edit|modify|change|touch|create|delete|remove)\s+(?:any\s+|the\s+|this\s+|my\s+)?(?:files?|code|workspace|repo|repository|codebase|project)\b/,
  /\b(?:do not|don't|dont|never)\s+(?:touch|change)\s+anything\b/,
  /\bwithout\s+(?:(?:making|applying)\s+(?:any\s+)?(?:changes?|edits?|modifications?)|(?:writing|editing|modifying|touching|creating|deleting|removing)\s+(?:any\s+|the\s+|this\s+|my\s+)?(?:files?|code|workspace|repo|repository|codebase|project))\b/,
  /\b(?:only|just)\s+(?:explain|describe|review|inspect|audit|analy[sz]e|diagnose|summari[sz]e|suggest|recommend|show)\b/,
];

const EXPLANATION_LEAD =
  /^(?:(?:please|can you|could you|would you)\s+)?(?:explain|describe|review|inspect|audit|analy[sz]e|diagnose|summari[sz]e|tell me|show me|why\b|what\b|how\b)/;

const HIGH_CONFIDENCE_ACTION =
  '(?:implement|fix|repair|patch|edit|modify|refactor|wire|integrate|install|rename|move|delete|remove)';
const TARGETED_ACTION =
  '(?:create|add|update|change|replace|write|generate|build|configure|work\\s+on|make)';
const ANY_ACTION = `(?:${HIGH_CONFIDENCE_ACTION}|${TARGETED_ACTION})`;
const FEATURE_BEHAVIOR_ACTION = '(?:show|display|render|surface)';

const COMMAND_PREFIX = new RegExp(
  `^(?:(?:please|kindly)\\s+)?(?:(?:(?:can|could|would|will)\\s+you|i\\s+(?:want|need)\\s+you\\s+to|go\\s+ahead\\s+and)\\s+)?${HIGH_CONFIDENCE_ACTION}\\b`,
);
const TARGETED_COMMAND_PREFIX = new RegExp(
  `^(?:(?:please|kindly)\\s+)?(?:(?:(?:can|could|would|will)\\s+you|i\\s+(?:want|need)\\s+you\\s+to|go\\s+ahead\\s+and)\\s+)?${TARGETED_ACTION}\\b`,
);
const COMPOUND_ACTION = new RegExp(`(?:[,;]\\s*|\\b(?:and|then)\\s+)(?:please\\s+)?${ANY_ACTION}\\b`);
const PRODUCT_REQUIREMENT_ACTION = new RegExp(
  `\\b(?:should|must|needs?\\s+to|i\\s+want\\s+(?:it|the\\s+app)\\s+to)\\s+${ANY_ACTION}\\b`,
);
const FEATURE_REQUIREMENT_ACTION = new RegExp(
  `\\b(?:`
    + `(?:i|we)\\s+(?:want|need)\\s+(?:it|(?:the\\s+)?(?:app|application|product|feature|ui)|users?)\\s+to(?:\\s+be\\s+able\\s+to)?`
    + `|(?:it|(?:the\\s+)?(?:app|application|product|feature|ui)|users?)\\s+(?:should|must|needs?\\s+to|has\\s+to)(?:\\s+be\\s+able\\s+to)?`
    + `|(?:let|allow|enable)\\s+users?\\s+(?:to\\s+)?`
  + `)\\s*(?:${ANY_ACTION}|${FEATURE_BEHAVIOR_ACTION})\\b`,
);
const ACTION_ANYWHERE = new RegExp(`\\b${ANY_ACTION}\\b`);

const WORKSPACE_TARGET =
  /\b(?:files?|folders?|director(?:y|ies)|workspace|repositor(?:y|ies)|repo|codebase|projects?|apps?|applications?|source|code|components?|modules?|functions?|classes?|tests?|specs?|configs?|configuration|readme|dependencies|packages?|endpoints?|routes?|apis?|ui|screens?|pages?|databases?|schemas?|migrations?|bugs?|features?|implementation)\b|(?:^|[\s("'`])(?:\.{0,2}\/)?[\w@.-]+(?:\/[\w@.-]+)*\.[a-z0-9]{1,10}\b/i;
/** Common implementation targets that make a short imperative such as
 * "Add dark mode" or "Create a login form" unambiguously about the open app
 * even when it does not repeat "workspace", "codebase", or a file path. */
const CODING_TASK_TARGET =
  /\b(?:auth(?:entication|orization)?|login|logout|sign[\s-]*in|sign[\s-]*out|dark[\s-]*mode|light[\s-]*mode|themes?|buttons?|forms?|navbars?|sidebars?|menus?|modals?|dialogs?|dropdowns?|tooltips?|layouts?|styles?|styling|responsive(?:ness)?|accessibility|hooks?|stores?|state|handlers?|services?|controllers?|validators?|validation|loading[\s-]*states?|empty[\s-]*states?|error[\s-]*states?)\b/i;

const SNIPPET_REQUEST =
  /\b(?:give|show|provide|return|write)\b[^.!?]{0,100}\b(?:a\s+)?(?:code\s+)?(?:snippet|example|sample|code\s+block|pseudocode)\b/;
const EXPLICIT_DISK_DESTINATION =
  /\b(?:save|put|apply|write|add|insert)\b[^.!?]{0,100}\b(?:to|into|in)\b[^.!?]{0,80}\b(?:files?|workspace|repo|repository|codebase|project|disk|src\/)/;
const EXPLICIT_CHANGE_PHRASE =
  /\b(?:make|apply)\s+(?:(?:the|these|those|requested|actual|real)\s+)?(?:changes?|edits?|patch)\b/;
const APPROVED_PLAN_EXECUTION =
  /\b(?:the\s+)?plan\s+is\s+approved\b[^.!?]*[.!?]?\s*\bexecute\s+it\s+now\b/;

const LATER_ACTION_MARKER = new RegExp(
  `(?:[;.!?]\\s*|\\b(?:but|instead|then|now|afterwards|after\\s+that)\\b\\s*)`
    + `(?:please\\s+)?${ANY_ACTION}\\b`,
  'g',
);

function normalizeRequest(text: string): string {
  return text.toLowerCase().replace(/[`*_]/g, ' ').replace(/\s+/g, ' ').trim();
}

function lastReadOnlyVetoEnd(request: string): number | null {
  let lastEnd: number | null = null;
  for (const pattern of READ_ONLY_VETOES) {
    const match = pattern.exec(request);
    if (match) lastEnd = Math.max(lastEnd ?? 0, match.index + match[0].length);
  }
  return lastEnd;
}

/**
 * A read-only/explanation instruction can only be superseded by a later,
 * clearly separated imperative that names a workspace/code target. Merely
 * finding another action word somewhere later ("fix the prose in your
 * answer") is not authorization to touch files.
 */
function hasLaterWorkspaceAuthorization(request: string, afterIndex: number): boolean {
  LATER_ACTION_MARKER.lastIndex = 0;
  for (const match of request.matchAll(LATER_ACTION_MARKER)) {
    if (match.index === undefined || match.index < afterIndex) continue;
    const clause = request.slice(match.index, match.index + 220);
    if (
      WORKSPACE_TARGET.test(clause)
      || EXPLICIT_DISK_DESTINATION.test(clause)
      || EXPLICIT_CHANGE_PHRASE.test(clause)
    ) {
      return true;
    }
  }
  return false;
}

/**
 * Deliberately conservative: only unmistakable requests to alter the open
 * codebase opt into the mutation contract. Questions, reviews, plans, and
 * code/snippet examples remain normal chat turns.
 */
export function isExplicitWorkspaceMutationRequest(text: string): boolean {
  const request = normalizeRequest(text);
  if (!request) return false;
  const hasCompoundAction = COMPOUND_ACTION.test(request);
  const readOnlyVetoEnd = lastReadOnlyVetoEnd(request);
  const laterWorkspaceAuthorization =
    readOnlyVetoEnd !== null
      ? hasLaterWorkspaceAuthorization(request, readOnlyVetoEnd)
      : hasLaterWorkspaceAuthorization(request, 0);
  if (readOnlyVetoEnd !== null && !laterWorkspaceAuthorization) return false;

  const hasExplicitDiskDestination = EXPLICIT_DISK_DESTINATION.test(request);
  if (
    SNIPPET_REQUEST.test(request)
    && !hasExplicitDiskDestination
    && !laterWorkspaceAuthorization
  ) return false;

  if (EXPLANATION_LEAD.test(request) && !laterWorkspaceAuthorization) return false;

  if (
    APPROVED_PLAN_EXECUTION.test(request)
    || EXPLICIT_CHANGE_PHRASE.test(request)
    || hasExplicitDiskDestination
    || laterWorkspaceAuthorization
    || FEATURE_REQUIREMENT_ACTION.test(request)
  ) return true;
  if (COMMAND_PREFIX.test(request) || hasCompoundAction) return true;

  const hasWorkspaceTarget =
    WORKSPACE_TARGET.test(request) || CODING_TASK_TARGET.test(request);
  if (!hasWorkspaceTarget) return false;
  return TARGETED_COMMAND_PREFIX.test(request)
    || PRODUCT_REQUIREMENT_ACTION.test(request)
    || ACTION_ANYWHERE.test(request);
}

export function requiresWorkspaceMutation(text: string, mode: PermissionMode): boolean {
  return mode !== 'plan' && isExplicitWorkspaceMutationRequest(text);
}

export function workspaceMutationPreflightFailure(
  mutationRequired: boolean,
  activeWorkspacePath: string | null,
  sessionWorkspacePath: string | null = null,
): string | null {
  if (!mutationRequired) return null;
  if (activeWorkspacePath === null) {
    return 'No files changed. Open a workspace folder with the folder picker before asking Little Monkey to modify files. A path typed in chat is context, not authorization to access that folder.';
  }
  if (
    sessionWorkspacePath !== null
    && normalizeWorkspacePath(sessionWorkspacePath) !== normalizeWorkspacePath(activeWorkspacePath)
  ) {
    return `No files changed. This chat is linked to "${sessionWorkspacePath}", but the active workspace is "${activeWorkspacePath}". Reopen the chat's workspace with the folder picker before making changes.`;
  }
  return null;
}

export function canRetryWithoutTools(mutationRequired: boolean): boolean {
  return !mutationRequired;
}

export type MutationPlainResponseAction = 'accept' | 'retry' | 'fail';

export function mutationPlainResponseAction(
  mutationRequired: boolean,
  mutationSucceeded: boolean,
  correctiveRetryUsed: boolean,
  mutationAttemptFailed = false,
): MutationPlainResponseAction {
  if (!mutationRequired) return 'accept';
  // A success earlier in the turn must never mask a later unresolved denial
  // or tool failure. The agent loop clears this flag only after that same
  // mutation target is successfully retried.
  if (mutationAttemptFailed) return 'fail';
  if (mutationSucceeded) return 'accept';
  return correctiveRetryUsed ? 'fail' : 'retry';
}

/** Keeps the completion failure truthful when earlier calls changed other
 * files before a later requested edit failed. */
export function mutationAttemptFailureMessage(
  mutationSucceeded: boolean,
  reason: string,
): string {
  return mutationSucceeded
    ? `Some files changed, but a requested file edit was not applied: ${reason}`
    : `No files changed. A requested file edit was not applied: ${reason}`;
}

function normalizeWorkspacePath(path: string): string {
  const normalized = path.replace(/\\/g, '/').replace(/\/+$/, '');
  return /^[a-z]:\//i.test(normalized)
    ? `${normalized[0].toLowerCase()}${normalized.slice(1)}`
    : normalized;
}

export function mutationToolFailureReason(resultContent: string): string | null {
  try {
    const parsed: unknown = JSON.parse(resultContent);
    if (!parsed || typeof parsed !== 'object') return null;
    const error = (parsed as { error?: unknown }).error;
    if (typeof error !== 'string' || error.trim().length === 0) return null;
    return error.trim().slice(0, 500);
  } catch {
    return null;
  }
}
