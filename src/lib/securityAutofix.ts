/**
 * AI Security Autofix Pipeline (ROADMAP.md Phase 7) — MVP scope, deliberately
 * narrowed to two finding sources this app can run entirely on its own,
 * without any new external infra, paid API, or new Rust module:
 *
 *  - "dependency": the workspace's own package manager audit
 *    (`pnpm audit --json`, a normal local package-manager operation), parsed
 *    from `runDependencyAudit` below.
 *  - "secret": a regex-based scan over workspace files via the EXISTING
 *    `grep` tool (`tools.ts`/`tools.rs`'s `tool_grep`), never a bespoke
 *    filesystem walker.
 *
 * SAST and license scanning are explicit non-goals for this slice — this app
 * doesn't bundle a real static-analysis or license-classification engine, and
 * faking one with regexes would be dishonest busywork dressed up as a
 * feature. A future slice adding a real SAST/license scanner is the natural
 * follow-up; see ROADMAP Phase 7 item 14's own acceptance note.
 *
 * Every step here reuses an existing primitive rather than inventing a new
 * backend flow:
 *  - Scanning drives the SAME `tool_${name}` dispatch a normal chat turn
 *    uses, via `turnEngine.ts`'s `executeToolCall` (see `issueToPrRunner.ts`
 *    for the sibling feature that established this reuse pattern) — so a
 *    `run_shell`/`grep` call made here goes through the exact same
 *    permission gate as any other agent-initiated tool call.
 *  - Proposing a fix is one one-shot, non-streaming, tool-less `attemptStream`
 *    call — the same transport `riskJudge.ts`'s `classifyToolCall` and
 *    `agentLoop.ts`'s `sendForSummary` use, dependency-injected here for the
 *    same reason `riskJudge.ts` documents (keeps this module decoupled from
 *    `turnEngine.ts`'s own types/module graph).
 *  - Applying an approved fix creates (or reuses) an owned worktree/branch
 *    via `gitDelivery.ts`'s existing `DeliveryMutation::CreateWorktree`
 *    primitive — the SAME one `issue_to_pr.rs` builds on — attaches it as a
 *    secondary workspace root via the existing `add_secondary_workspace_root`
 *    command, then drives a real headless agent turn against it with
 *    `runSecurityAutofixAgent` below. It shares `headlessAgentRunner.ts` with
 *    `issueToPrRunner.ts` and `migrationAgentRunner.ts`, so all three use the
 *    same permission-gated tool dispatch, cancellation, and Run Capsule
 *    recording path.
 *
 * Pushing the branch and opening a PR are explicitly OUT of scope here — the
 * resulting branch is left for the user to inspect and push through the
 * EXISTING Git Delivery panel (`GitDeliveryPanel.tsx`), which already owns
 * the confirm-and-type-the-phrase flow for that real external GitHub write.
 * This mirrors the acceptance criterion's own phrasing: fixes are generated
 * in isolated branches and verified BEFORE user approval — the push/PR is the
 * user-approval step itself, and stays a human's job in an existing,
 * already-audited flow rather than a second one built here.
 */
import { invoke } from '@tauri-apps/api/core';

import {
  executeDeliveryMutation,
  prepareDeliveryMutation,
  validateCreateRequest,
  type DeliveryMutation,
  type OwnedWorktreeRecord,
  type WorktreeCreateRequest,
} from './gitDelivery';
import { effortForTarget } from '../store/modelStore';
import { primaryRoot, useWorkspaceStore, type WorkspaceRootInfo } from '../store/workspaceStore';
import { resolveTarget } from './agentLoop';
import { runHeadlessAgent } from './headlessAgentRunner';
import type { ChatMessage, ToolCall } from './llamaClient';
import type { McpToolRegistry } from './mcpTools';
import { parseModelJsonCandidates } from './modelJson';
import { attemptStream, executeToolCall } from './turnEngine';

// ---------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------

export type SecurityFindingKind = 'dependency' | 'secret';
export type SecuritySeverity = 'critical' | 'high' | 'moderate' | 'low' | 'info';

export interface DependencyFindingDetails {
  packageName: string;
  currentVersion: string | null;
  patchedVersions: string | null;
  vulnerableRange: string | null;
  advisoryTitle: string;
  advisoryUrl: string | null;
  advisoryId: string;
}

export interface SecretFindingDetails {
  path: string;
  line: number;
  ruleName: string;
  /** Never the raw matched text — see `redact` below. A security feature
   * that stores/displays the very secrets it finds, in a store that other
   * panels/tests can read, would be the exact failure mode it exists to
   * catch. */
  redactedSnippet: string;
}

export interface SecurityFinding {
  id: string;
  kind: SecurityFindingKind;
  severity: SecuritySeverity;
  title: string;
  description: string;
  detectedAtMs: number;
  dependency?: DependencyFindingDetails;
  secret?: SecretFindingDetails;
}

export interface SecurityFixProposal {
  findingId: string;
  exploitabilityNote: string;
  proposedFix: string;
  testPlan: string;
  generatedAtMs: number;
  /** `'model'` when the local/provider model produced a well-formed
   * proposal; `'fallback'` when it didn't (empty reply, malformed JSON,
   * stream error, timeout) and a deterministic templated proposal derived
   * straight from the finding's own fields was used instead — never a
   * fabricated-looking model answer. Surfaced in the panel so the user can
   * tell the difference. */
  source: 'model' | 'fallback';
}

function emptyMcpRegistry(): McpToolRegistry {
  return new Map();
}

function severityRank(severity: SecuritySeverity): number {
  switch (severity) {
    case 'critical': return 4;
    case 'high': return 3;
    case 'moderate': return 2;
    case 'low': return 1;
    case 'info': return 0;
  }
}

/** Sorts findings most-severe first, stable on a tie by detection order —
 * the panel's own display order, but exported so tests can pin it directly. */
export function sortFindingsBySeverity(findings: SecurityFinding[]): SecurityFinding[] {
  return [...findings].sort((a, b) => severityRank(b.severity) - severityRank(a.severity));
}

// ---------------------------------------------------------------------
// Dependency audit — `pnpm audit --json`, parsed defensively
// ---------------------------------------------------------------------

const AUDIT_TURN_ID = 'security-autofix-audit';
const AUDIT_OUTPUT_MAX_CHARS = 4000;

function normalizeSeverity(value: unknown): SecuritySeverity {
  const lowered = typeof value === 'string' ? value.toLowerCase() : '';
  if (lowered === 'critical' || lowered === 'high' || lowered === 'moderate' || lowered === 'low' || lowered === 'info') {
    return lowered;
  }
  return 'moderate';
}

/**
 * Parses `pnpm audit --json`'s stdout. Supports the two shapes pnpm's audit
 * output has actually shipped (both npm-audit-compatible):
 *  - the classic `{ advisories: { [id]: {...} } }` report (npm audit v1,
 *    which pnpm's `--json` still emits on most installed versions), and
 *  - the newer `{ vulnerabilities: { [packageName]: {...} } }` report (npm
 *    audit v7+ shape) as a best-effort fallback.
 * Any other shape — or invalid JSON — returns an empty list rather than
 * throwing: a corrupt/unexpected audit report must never crash the scan, it
 * just means "nothing found this way" (the secret scan runs independently).
 * Each entry is parsed defensively on its own so one malformed advisory can't
 * drop every other one.
 */
export function parsePnpmAuditJson(raw: string): SecurityFinding[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!parsed || typeof parsed !== 'object') return [];
  const findings: SecurityFinding[] = [];
  const now = Date.now();

  const advisories = (parsed as { advisories?: unknown }).advisories;
  if (advisories && typeof advisories === 'object') {
    for (const value of Object.values(advisories as Record<string, unknown>)) {
      try {
        if (!value || typeof value !== 'object') continue;
        const advisory = value as Record<string, unknown>;
        const moduleName = typeof advisory.module_name === 'string' ? advisory.module_name : 'unknown package';
        const title = typeof advisory.title === 'string' ? advisory.title : 'Dependency vulnerability';
        const id = advisory.id !== undefined ? String(advisory.id) : `${moduleName}-${title}`;
        const findingsArray = Array.isArray(advisory.findings) ? advisory.findings : [];
        const firstFinding = findingsArray.length > 0 && typeof findingsArray[0] === 'object' ? (findingsArray[0] as Record<string, unknown>) : null;
        findings.push({
          id: `dep-${id}`,
          kind: 'dependency',
          severity: normalizeSeverity(advisory.severity),
          title: `${moduleName}: ${title}`,
          description: typeof advisory.overview === 'string' && advisory.overview.trim() ? advisory.overview : title,
          detectedAtMs: now,
          dependency: {
            packageName: moduleName,
            currentVersion: firstFinding && typeof firstFinding.version === 'string' ? firstFinding.version : null,
            patchedVersions: typeof advisory.patched_versions === 'string' ? advisory.patched_versions : null,
            vulnerableRange: typeof advisory.vulnerable_versions === 'string' ? advisory.vulnerable_versions : null,
            advisoryTitle: title,
            advisoryUrl: typeof advisory.url === 'string' ? advisory.url : null,
            advisoryId: id,
          },
        });
      } catch {
        // Skip this one advisory, keep parsing the rest.
      }
    }
    if (findings.length > 0) return findings;
  }

  const vulnerabilities = (parsed as { vulnerabilities?: unknown }).vulnerabilities;
  if (vulnerabilities && typeof vulnerabilities === 'object') {
    for (const [name, value] of Object.entries(vulnerabilities as Record<string, unknown>)) {
      try {
        if (!value || typeof value !== 'object') continue;
        const entry = value as Record<string, unknown>;
        const via = Array.isArray(entry.via) ? entry.via : [];
        const viaDetail = via.find((item) => item && typeof item === 'object') as Record<string, unknown> | undefined;
        const title = viaDetail && typeof viaDetail.title === 'string' ? viaDetail.title : `Vulnerability in ${name}`;
        const fixAvailable = entry.fixAvailable;
        const patchedVersion =
          fixAvailable && typeof fixAvailable === 'object' && typeof (fixAvailable as Record<string, unknown>).version === 'string'
            ? ((fixAvailable as Record<string, unknown>).version as string)
            : null;
        findings.push({
          id: `dep-${name}`,
          kind: 'dependency',
          severity: normalizeSeverity(entry.severity),
          title: `${name}: ${title}`,
          description: title,
          detectedAtMs: now,
          dependency: {
            packageName: name,
            currentVersion: null,
            patchedVersions: patchedVersion,
            vulnerableRange: typeof entry.range === 'string' ? entry.range : null,
            advisoryTitle: title,
            advisoryUrl: viaDetail && typeof viaDetail.url === 'string' ? viaDetail.url : null,
            advisoryId: name,
          },
        });
      } catch {
        // Skip this one entry, keep parsing the rest.
      }
    }
  }

  return findings;
}

export interface DependencyAuditResult {
  findings: SecurityFinding[];
  /** Non-null only when the command itself failed to produce any parseable
   * report (e.g. `pnpm` not on PATH, no lockfile) — NOT set just because
   * vulnerabilities were found (pnpm exits non-zero for those, which is
   * normal and not a scan failure). */
  error: string | null;
}

/**
 * Runs `pnpm audit --json` via the EXISTING `run_shell` tool primitive
 * (`executeToolCall` against a synthetic tool call) — the exact same
 * permission-gated path a model-driven `run_shell` call takes, so the user
 * sees the same permission prompt they'd see for any other shell command this
 * app runs on their behalf. `cwd` defaults to the primary workspace root.
 */
export async function runDependencyAudit(options: { cwd?: string } = {}): Promise<DependencyAuditResult> {
  const toolCall: ToolCall = {
    id: 'security-autofix-scan-pnpm-audit',
    type: 'function',
    function: { name: 'run_shell', arguments: JSON.stringify({ command: 'pnpm audit --json', cwd: options.cwd }) },
  };
  const raw = await executeToolCall(toolCall, null, AUDIT_TURN_ID, emptyMcpRegistry());
  try {
    const parsed = JSON.parse(raw) as { stdout?: string; stderr?: string; code?: number | null; error?: string };
    if (parsed.error) return { findings: [], error: parsed.error };
    const stdout = parsed.stdout ?? '';
    const findings = parsePnpmAuditJson(stdout);
    if (findings.length === 0 && !stdout.trim() && (parsed.stderr ?? '').trim()) {
      return { findings: [], error: (parsed.stderr as string).slice(0, AUDIT_OUTPUT_MAX_CHARS) };
    }
    return { findings, error: null };
  } catch (err) {
    return { findings: [], error: err instanceof Error ? err.message : String(err) };
  }
}

// ---------------------------------------------------------------------
// Secret scan — regex patterns run through the EXISTING `grep` tool
// ---------------------------------------------------------------------

const SECRET_SCAN_TURN_ID = 'security-autofix-secret-scan';
const MAX_SECRET_FINDINGS = 100;

export interface SecretPattern {
  name: string;
  /** Rust `regex` crate syntax (no lookaround/backreferences — that crate
   * doesn't support them) — this runs through `tool_grep`'s `Regex::new`,
   * not a JS regex engine. */
  regex: string;
}

/** Heuristic, MVP-scope patterns for common secret formats — a real secret
 * scanner (entropy analysis, provider-specific validators) is a follow-up;
 * this deliberately favors a short, readable, low-false-negative list over
 * exhaustive coverage. */
export const SECRET_PATTERNS: SecretPattern[] = [
  { name: 'AWS Access Key ID', regex: 'AKIA[0-9A-Z]{16}' },
  { name: 'GitHub Token', regex: 'gh[pousr]_[A-Za-z0-9]{36}' },
  { name: 'Slack Token', regex: 'xox[baprs]-[0-9A-Za-z-]{10,48}' },
  { name: 'Private Key Block', regex: '-----BEGIN (RSA|EC|OPENSSH|DSA|PGP) PRIVATE KEY-----' },
  { name: 'Stripe Live Secret Key', regex: 'sk_live_[0-9a-zA-Z]{16,}' },
  { name: 'Google API Key', regex: 'AIza[0-9A-Za-z_-]{35}' },
  { name: 'Generic Secret Assignment', regex: "(?i)(api[_-]?key|secret|token|password)\\s*[:=]\\s*['\"][A-Za-z0-9_-]{16,}['\"]" },
];

/** Keeps at most a few characters at each end so a genuine secret is never
 * fully readable from the finding alone — enough to recognize which secret
 * it is, never enough to reuse it. */
export function redactSecretSnippet(text: string): string {
  const trimmed = text.trim();
  if (trimmed.length <= 12) return '*'.repeat(trimmed.length || 1);
  return `${trimmed.slice(0, 6)}…${trimmed.slice(-4)}`;
}

async function hashKey(value: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(value));
  return [...new Uint8Array(digest)].slice(0, 8).map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

/**
 * Scans the workspace for hardcoded-secret-shaped strings by running each of
 * `SECRET_PATTERNS` through the EXISTING `grep` tool primitive (never a
 * bespoke file walker) — same `executeToolCall` reuse as `runDependencyAudit`
 * above. `grep`/`read_file` don't require user permission (see `tools.ts`),
 * so this scan runs without a permission prompt, unlike the dependency audit.
 */
export async function runSecretScan(options: { path?: string } = {}): Promise<SecurityFinding[]> {
  const findings: SecurityFinding[] = [];
  const seen = new Set<string>();
  const now = Date.now();

  for (const pattern of SECRET_PATTERNS) {
    const toolCall: ToolCall = {
      id: `security-autofix-secret-scan-${pattern.name}`,
      type: 'function',
      function: { name: 'grep', arguments: JSON.stringify({ pattern: pattern.regex, path: options.path }) },
    };
    let raw: string;
    try {
      raw = await executeToolCall(toolCall, null, SECRET_SCAN_TURN_ID, emptyMcpRegistry());
    } catch {
      continue;
    }
    let matches: unknown;
    try {
      matches = JSON.parse(raw);
    } catch {
      continue;
    }
    if (!Array.isArray(matches)) continue;

    for (const match of matches) {
      if (!match || typeof match !== 'object') continue;
      const file = typeof (match as Record<string, unknown>).file === 'string' ? ((match as Record<string, unknown>).file as string) : null;
      const line = typeof (match as Record<string, unknown>).line === 'number' ? ((match as Record<string, unknown>).line as number) : null;
      const text = typeof (match as Record<string, unknown>).text === 'string' ? ((match as Record<string, unknown>).text as string) : '';
      if (!file || line === null) continue;
      const dedupeKey = `${file}:${line}:${pattern.name}`;
      if (seen.has(dedupeKey)) continue;
      seen.add(dedupeKey);
      // eslint-disable-next-line no-await-in-loop
      const id = await hashKey(dedupeKey);
      findings.push({
        id: `secret-${id}`,
        kind: 'secret',
        severity: 'high',
        title: `Possible ${pattern.name} in ${file}`,
        description: `A pattern matching "${pattern.name}" was found on line ${line} of ${file}. Treat it as a live credential until proven otherwise.`,
        detectedAtMs: now,
        secret: { path: file, line, ruleName: pattern.name, redactedSnippet: redactSecretSnippet(text) },
      });
      if (findings.length >= MAX_SECRET_FINDINGS) return findings;
    }
  }

  return findings;
}

/** Runs both scans and merges the results, most-severe first. Never throws:
 * either scan failing independently (e.g. `pnpm` missing) still surfaces
 * whatever the other one found, with the failure reported separately. */
export async function runSecurityScan(): Promise<{ findings: SecurityFinding[]; auditError: string | null }> {
  const [audit, secrets] = await Promise.all([runDependencyAudit(), runSecretScan()]);
  return { findings: sortFindingsBySeverity([...audit.findings, ...secrets]), auditError: audit.error };
}

// ---------------------------------------------------------------------
// Propose a fix — one one-shot, tool-less model call (same transport as
// riskJudge.ts's classifyToolCall / agentLoop.ts's sendForSummary)
// ---------------------------------------------------------------------

export interface ProposeFixCallResult {
  content: string;
  streamError: string | null;
}

export type ProposeFixCallModel = (messages: ChatMessage[], signal?: AbortSignal) => Promise<ProposeFixCallResult>;

const MAX_DESCRIPTION_CHARS = 1500;

function truncate(value: string, max: number): string {
  return value.length > max ? `${value.slice(0, max)}…` : value;
}

export function buildProposalPrompt(finding: SecurityFinding): ChatMessage[] {
  const detail =
    finding.kind === 'dependency' && finding.dependency
      ? [
          `Package: ${finding.dependency.packageName}`,
          `Current/vulnerable version(s): ${finding.dependency.vulnerableRange ?? finding.dependency.currentVersion ?? 'unknown'}`,
          `Patched version(s): ${finding.dependency.patchedVersions ?? 'unknown'}`,
          `Advisory: ${finding.dependency.advisoryTitle}${finding.dependency.advisoryUrl ? ` (${finding.dependency.advisoryUrl})` : ''}`,
        ].join('\n')
      : finding.secret
        ? [
            `File: ${finding.secret.path}:${finding.secret.line}`,
            `Rule matched: ${finding.secret.ruleName}`,
            `Redacted snippet: ${finding.secret.redactedSnippet}`,
          ].join('\n')
        : '(no further detail)';

  return [
    {
      role: 'system',
      content:
        'You are a security remediation advisor for an autonomous coding agent, running as a strict, non-conversational judge. ' +
        'Given one security finding, reply with ONLY a single-line JSON object of the exact shape ' +
        '{"exploitabilityNote":"...","proposedFix":"...","testPlan":"..."} — no markdown, no other text. ' +
        '"exploitabilityNote" is 1-2 sentences on how realistic exploitation is given the finding. ' +
        '"proposedFix" is a short, concrete description of the source change to make (for a dependency finding: which version to upgrade to and why; for a secret finding: remove the secret from source control, replace it with an environment-variable/config reference, and note that the credential itself must be rotated with its issuing provider — never claim you rotated it yourself). ' +
        '"testPlan" is 1-2 sentences on how to verify the fix (e.g. which existing test/build script to run).',
    },
    {
      role: 'user',
      content: `Finding kind: ${finding.kind}\nSeverity: ${finding.severity}\nTitle: ${finding.title}\nDescription: ${truncate(finding.description, MAX_DESCRIPTION_CHARS)}\n${detail}`,
    },
  ];
}

/** Strict parse, same fail-closed shape as `riskJudge.ts`'s
 * `parseJudgeResponse`: anything not exactly the expected three-string shape
 * returns `null` rather than a partially-fabricated proposal. */
export function parseProposalResponse(
  content: string,
): { exploitabilityNote: string; proposedFix: string; testPlan: string } | null {
  for (const parsed of parseModelJsonCandidates(content, 'object')) {
    const { exploitabilityNote, proposedFix, testPlan } = parsed;
    if (
      typeof exploitabilityNote === 'string' && exploitabilityNote.trim() &&
      typeof proposedFix === 'string' && proposedFix.trim() &&
      typeof testPlan === 'string' && testPlan.trim()
    ) {
      return { exploitabilityNote: exploitabilityNote.trim(), proposedFix: proposedFix.trim(), testPlan: testPlan.trim() };
    }
  }
  return null;
}

/** Deterministic, templated proposal derived straight from the finding's own
 * fields — used whenever the model call fails, times out, or returns
 * something unparseable, so the user is NEVER shown a fabricated-looking
 * model answer for a failure. Marked `source: 'fallback'` so the panel can
 * say so. */
export function fallbackProposal(finding: SecurityFinding): SecurityFixProposal {
  if (finding.kind === 'dependency' && finding.dependency) {
    const dep = finding.dependency;
    return {
      findingId: finding.id,
      exploitabilityNote: `Automated proposal generation was unavailable. ${dep.advisoryTitle} affects ${dep.packageName}${dep.vulnerableRange ? ` (${dep.vulnerableRange})` : ''}; treat as exploitable until reviewed.`,
      proposedFix: `Upgrade ${dep.packageName} to ${dep.patchedVersions ?? 'the latest patched version'} in package.json and the lockfile.`,
      testPlan: "Run this repository's own test/build scripts (see package.json) after the upgrade.",
      generatedAtMs: Date.now(),
      source: 'fallback',
    };
  }
  const secret = finding.secret;
  return {
    findingId: finding.id,
    exploitabilityNote: 'Automated proposal generation was unavailable. Treat any hardcoded credential as compromised until rotated.',
    proposedFix: secret
      ? `Remove the matched secret from ${secret.path}:${secret.line}, replace it with an environment-variable/config reference, and rotate the credential with its issuing provider.`
      : 'Remove the hardcoded secret and replace it with an environment-variable/config reference; rotate the credential with its issuing provider.',
    testPlan: "Run this repository's own test/build scripts to confirm the change that reads the new config value still works.",
    generatedAtMs: Date.now(),
    source: 'fallback',
  };
}

export async function proposeFixForFinding(
  finding: SecurityFinding,
  callModel: ProposeFixCallModel,
  signal?: AbortSignal,
): Promise<SecurityFixProposal> {
  try {
    const result = await callModel(buildProposalPrompt(finding), signal);
    if (!result.streamError) {
      const parsed = parseProposalResponse(result.content);
      if (parsed) {
        return { findingId: finding.id, ...parsed, generatedAtMs: Date.now(), source: 'model' };
      }
    }
  } catch {
    // Falls through to the deterministic fallback below.
  }
  return fallbackProposal(finding);
}

/** Builds the `callModel` closure `proposeFixForFinding` needs, against the
 * currently active chat target — the same `attemptStream`-against-`target`
 * shape `agentLoop.ts` builds for `riskJudge.ts`'s `classifyToolCall`. */
export async function defaultProposeFixCallModel(runId: string): Promise<ProposeFixCallModel> {
  const target = await resolveTarget();
  const effort = effortForTarget(target);
  return (messages, signal) => attemptStream(target, messages, [], signal, effort, runId, undefined, false);
}

// ---------------------------------------------------------------------
// Apply an approved fix in an isolated, owned branch
// ---------------------------------------------------------------------

const BRANCH_PREFIX = 'security-autofix/';
const DEFAULT_PROTECTED_BRANCHES = ['main', 'master', 'develop', 'release'];

function slugify(value: string, max = 40): string {
  const slug = value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return (slug || 'finding').slice(0, max);
}

export interface IsolatedBranch {
  worktreeId: string;
  branch: string;
  workspaceLabel: string;
  canonicalPath: string;
}

/**
 * Creates (or the caller may choose to reuse, by inspecting
 * `listOwnedWorktrees()` themselves — not done automatically here, since
 * unlike Issue-to-PR there's no natural "same run" key to dedupe on) an owned
 * worktree/branch for one finding's fix, via the EXISTING
 * `DeliveryMutation::CreateWorktree` primitive, then attaches it as a
 * secondary workspace root via the EXISTING `add_secondary_workspace_root`
 * command — no new Rust, both already exposed to the frontend.
 *
 * The mutation is driven straight through (prepare, then immediately execute
 * with the preview's own digest/confirmation phrase) exactly like
 * `issue_to_pr.rs`'s own `create_worktree_for_issue` does server-side —
 * creating a local, non-destructive worktree is not one of the flows that
 * needs the user to read a preview and type a phrase (see
 * `gitDelivery.ts`'s `isExternalMutation`, which returns `false` for
 * `create_worktree`); only push/PR/review-publish do, and none of those run
 * here.
 */
export async function createIsolatedBranchForFinding(
  finding: SecurityFinding,
  repositorySlug: string,
): Promise<IsolatedBranch> {
  const root = primaryRoot(useWorkspaceStore.getState().roots);
  if (!root) throw new Error('Open a primary workspace folder first.');

  const label = `security-${finding.kind}-${slugify(
    finding.kind === 'dependency' ? finding.dependency?.packageName ?? finding.id : finding.id,
  )}`;

  const request: WorktreeCreateRequest = {
    repositoryRoot: root.path,
    repositorySlug: repositorySlug.trim(),
    baseRef: 'HEAD',
    label,
    allowedRemotes: ['origin'],
    branchPrefix: BRANCH_PREFIX,
    protectedBranches: DEFAULT_PROTECTED_BRANCHES,
    allowPush: true,
    allowCreatePullRequest: true,
    allowReviewComment: false,
    allowForkWrites: false,
  };
  const validationErrors = validateCreateRequest(request);
  if (validationErrors.length > 0) throw new Error(validationErrors.join(' '));

  const mutation: DeliveryMutation = { kind: 'create_worktree', payload: request };
  const preview = await prepareDeliveryMutation(mutation);
  const result = await executeDeliveryMutation(mutation, preview.digest, preview.confirmationPhrase);
  if (!result || typeof result !== 'object' || !('marker' in result)) {
    throw new Error('Owned worktree creation returned an unexpected shape.');
  }
  const record = result as OwnedWorktreeRecord;

  const rootInfo = await invoke<WorkspaceRootInfo>('add_secondary_workspace_root', {
    path: record.marker.canonicalPath,
  });
  await useWorkspaceStore.getState().refreshRoots();

  return {
    worktreeId: record.marker.worktreeId,
    branch: record.marker.branch,
    workspaceLabel: rootInfo.label,
    canonicalPath: record.marker.canonicalPath,
  };
}

// ---------------------------------------------------------------------
// Apply the fix — a real shared headless agent turn in the owned worktree
// ---------------------------------------------------------------------

export const MAX_SECURITY_AUTOFIX_ITERATIONS = 40;

export interface RunSecurityAutofixAgentParams {
  runId: string;
  finding: SecurityFinding;
  proposal: SecurityFixProposal;
  branch: string;
  workspaceLabel: string;
  signal: AbortSignal;
  onToolActivity?: (label: string) => void;
}

export interface SecurityAutofixAgentResult {
  outcome: 'completed' | 'cancelled' | 'error';
  summary: string;
  durableRunId: string | null;
}

function buildAutofixSystemPrompt(params: RunSecurityAutofixAgentParams): string {
  const { finding, proposal } = params;
  const findingDetail =
    finding.kind === 'dependency' && finding.dependency
      ? `Dependency finding: upgrade "${finding.dependency.packageName}" (currently in range ${finding.dependency.vulnerableRange ?? 'unknown'}) to a patched version (${finding.dependency.patchedVersions ?? 'the latest patched release'}) to resolve: ${finding.dependency.advisoryTitle}.`
      : finding.secret
        ? `Secret finding: a hardcoded credential matching "${finding.secret.ruleName}" was found at ${finding.secret.path}:${finding.secret.line}. Remove it from source control and replace it with an environment-variable/config reference. You cannot rotate the actual credential yourself (no access to the issuing provider) — leave a clear TODO/comment for the user to rotate it.`
        : finding.title;

  return [
    'You are Little Monkey, running the AI Security Autofix Pipeline: a headless, panel-driven run with no one watching live — never ask a question, just make the best reasonable call and note any assumption in your final summary.',
    `Your task is to apply an already-approved fix for one security finding, on the app-owned branch "${params.branch}".`,
    findingDetail,
    `Exploitability note: ${proposal.exploitabilityNote}`,
    `Proposed fix: ${proposal.proposedFix}`,
    `Test plan: ${proposal.testPlan}`,
    `Every file, list_dir, glob, grep, write_file, edit_file, and run_shell path/cwd you use MUST be prefixed with "${params.workspaceLabel}/" — that is the only root this run may touch. Never use an absolute path or an unprefixed relative path.`,
    'Read the relevant code first, then make the minimal correct change. Prefer a small, reviewable diff over a broad rewrite.',
    "Once the change looks complete, detect and run this repository's own test/build scripts yourself (e.g. read package.json for a \"test\"/\"build\" script and run it with run_shell) and fix anything they surface before finishing.",
    'Hard limits, never do any of these — they stay outside this flow entirely and are handled by a human reviewer afterward: do not run `git merge`, do not force-push, do not delete any branch, do not push to a remote, and do not attempt to rotate any external credential yourself.',
    'When you are done, reply with a short final summary: what you changed and why, and the result of the checks you ran. Do not call any more tools after that summary.',
  ].join('\n');
}

/**
 * Runs the shared model->tools->model loop to completion around one security
 * finding and its approved proposal. Never throws; every outcome is reported
 * through the returned `SecurityAutofixAgentResult`.
 */
export async function runSecurityAutofixAgent(
  params: RunSecurityAutofixAgentParams,
): Promise<SecurityAutofixAgentResult> {
  const systemPrompt = buildAutofixSystemPrompt(params);

  const userMessage = `Apply the approved fix for finding "${params.finding.title}" (${params.finding.kind}, severity ${params.finding.severity}) as described in the system prompt.`;
  return runHeadlessAgent({
    runId: params.runId,
    signal: params.signal,
    systemPrompt,
    userMessage,
    maxIterations: MAX_SECURITY_AUTOFIX_ITERATIONS,
    executionSource: 'security-autofix',
    durableRun: {
      task: `Security autofix: ${params.finding.title}`,
      instructions: `Owned branch ${params.branch}`,
    },
    onToolActivity: params.onToolActivity,
  });
}
