/**
 * K9 (docs/agent-os-roadmap.md) — the dispatch-policy engine: given the
 * targets this profile actually has, which one executes this turn, and why.
 *
 * Deliberately pure and store-free. Every input is passed in — candidates,
 * the active target, the turn's own hard requirements — so the whole decision
 * is testable without a browser, a provider, or a model. `routeTarget` in
 * `agentLoop.ts` is the one place that reads live stores and calls in here.
 *
 * Three rules this engine is built around, in the order they matter:
 *
 * 1. **It selects, it never invents.** Every key it can return came in as a
 *    candidate, and candidates are built from `buildModelTargetInventory` —
 *    the same inventory the model picker shows. A policy therefore cannot
 *    name a provider the user has not configured, and cannot reach a model
 *    the user's own credentials do not already cover.
 * 2. **It cannot widen anything.** It has no concept of a permission, a
 *    privacy decision, or an egress rule, and takes none as input. The
 *    Privacy Firewall runs *after* routing at the call site and still owns
 *    the final word (including its own switch-to-local), which is why a
 *    routed target is gated exactly like a manually picked one — see
 *    `routing_never_precedes_the_privacy_gate` in the tests.
 * 3. **It cannot break the turn.** A policy that matches but whose
 *    constraints exclude everything falls back to the active target and says
 *    so in `reason`. Routing is allowed to be unhelpful; it is not allowed to
 *    leave a turn with nowhere to run.
 */
import type { CapabilityState } from "./modelTargets";

/** The dispatch surfaces a policy can be scoped to. Each value corresponds to
 * a real call site that resolves a target (see `routeFromActive`'s callers),
 * not to an invented taxonomy — a class nothing dispatches under would be a
 * criterion the user could set and never observe.
 *
 * Subagents are here as two classes rather than one, because the two profiles
 * are different work: `explore` reads and reports, `code` mutates a workspace,
 * and a user who wants a cheap model for the first and a careful one for the
 * second cannot say so with a single "subagent" class. They dispatch through
 * `subagent.ts::resolveSubagentTarget`, which reads `targetRouting.ts` — the
 * module target resolution was lifted into precisely so that a subagent could
 * route without `subagent.ts` importing `agentLoop.ts` and closing a cycle
 * through `turnEngine.ts`.
 *
 * `settingsStore.subagentProfileModels` still wins where it is set: it is an
 * explicit per-profile choice the user made, and a policy is a rule about work
 * the user did not pin. */
export type RoutingTaskClass =
  | "chat"
  | "summarize"
  | "subagent_explore"
  | "subagent_code";

export const ROUTING_TASK_CLASSES: readonly RoutingTaskClass[] = [
  "chat",
  "summarize",
  "subagent_explore",
  "subagent_code",
];

/** A policy's data-sensitivity constraint. `local_only` restricts candidates
 * to targets that execute on this machine, which is a *narrowing* of where
 * this turn may go — never a widening, and never a substitute for the Privacy
 * Firewall, which still runs on whatever this engine returns. */
export type RoutingSensitivity = "any" | "local_only";

export interface RoutingPolicy {
  readonly id: string;
  /** User-authored name. Shown in the transcript note that reports the
   * decision, so it is what "which policy chose this" actually answers. */
  readonly name: string;
  readonly enabled: boolean;
  /** Empty means every task class. */
  readonly taskClasses: readonly RoutingTaskClass[];
  /** Ordered `ModelTargetSnapshot.key`s to prefer, most-preferred first. A
   * preferred key still has to satisfy every constraint below and still has
   * to be available — a pin is a preference, not an override. */
  readonly preferredTargetKeys: readonly string[];
  /** Restrict to targets that can call tools. */
  readonly requiresTools: boolean;
  readonly sensitivity: RoutingSensitivity;
  /** Rate ceilings in USD per million tokens, against the rates the user
   * entered themselves (`costControlStore.rates`). Deliberately a *rate*
   * ceiling rather than a per-turn cost ceiling: a turn's token count is not
   * known before the turn runs, so a "max $ per turn" criterion could only be
   * enforced by guessing at one. */
  readonly maxInputPerMillionUsd: number | null;
  readonly maxOutputPerMillionUsd: number | null;
  /** Ceiling on *measured* time-to-first-token (median of this target's
   * recorded attempts — see `observedTimeToFirstTokenMs`). A target with no
   * measurement yet is not excluded by this, because "not measured" is not
   * "too slow"; it simply ranks behind every target measured within the
   * ceiling. */
  readonly maxTimeToFirstTokenMs: number | null;
}

export interface RoutingCandidate {
  readonly key: string;
  /** Human label for the decision sentence. */
  readonly label: string;
  /** Whether this target executes on this machine (no model egress at all).
   * Ollama cloud tags are remote despite the local daemon, so this is not
   * `kind !== "provider"`. */
  readonly isLocal: boolean;
  readonly available: boolean;
  readonly toolCalling: CapabilityState;
  readonly vision: CapabilityState;
  /** User-entered rates, or null when this target has none configured. Local
   * targets are free rather than unknown, so they pass any ceiling. */
  readonly inputPerMillionUsd: number | null;
  readonly outputPerMillionUsd: number | null;
  /** Median measured time-to-first-token, or null when never measured. */
  readonly observedTimeToFirstTokenMs: number | null;
}

export interface RoutingRequest {
  readonly taskClass: RoutingTaskClass;
  /** Hard requirement from the turn itself (an image is attached), not a
   * policy preference — a policy can never route a turn with an image to a
   * target that cannot see it. */
  readonly requiresVision: boolean;
  /** Hard requirement from the turn itself (this surface offers tools). */
  readonly requiresTools: boolean;
}

export interface RoutingRejection {
  readonly key: string;
  readonly reason: string;
}

export interface RoutingDecision {
  /** Which dispatch surface asked — a decision is only interpretable next to
   * it, since the same policy list answers differently per class. */
  readonly taskClass: RoutingTaskClass;
  /** Null when no enabled policy matched this task class, which is the
   * default state of a fresh profile: dispatch behaves exactly as it did
   * before this engine existed. */
  readonly policyId: string | null;
  readonly policyName: string | null;
  /** The target to run, or null when the caller should keep the active one. */
  readonly chosenKey: string | null;
  /** Whether the choice actually moves off the active target. False when a
   * policy applied and the active model already satisfied it, which is the
   * steady state of a working conversation — callers use this to avoid
   * announcing a switch that did not happen on every single turn. */
  readonly changedFromActive: boolean;
  /** Ordered attempt sequence, chosen target first. Supplies the failover
   * order too, so a policy replaces the fixed provider sequence rather than
   * being overruled by it on the second attempt. */
  readonly sequence: readonly string[];
  /** One sentence naming the policy and why this target won — this is the
   * "inspect which policy chose the target and why" half of K9. */
  readonly reason: string;
  readonly rejected: readonly RoutingRejection[];
}

/** No enabled policy covers this task class. */
function unrouted(taskClass: RoutingTaskClass, reason: string): RoutingDecision {
  return {
    taskClass,
    policyId: null,
    policyName: null,
    chosenKey: null,
    changedFromActive: false,
    sequence: [],
    reason,
    rejected: [],
  };
}

/** The first enabled policy whose task classes cover `taskClass`. Order is
 * the list's own order, which is what makes "reorder to change precedence"
 * work without a priority field to keep consistent. */
export function matchPolicy(
  policies: readonly RoutingPolicy[],
  taskClass: RoutingTaskClass,
): RoutingPolicy | null {
  return (
    policies.find(
      (policy) =>
        policy.enabled
        && (policy.taskClasses.length === 0 || policy.taskClasses.includes(taskClass)),
    ) ?? null
  );
}

/** Why `candidate` cannot serve this policy, or null when it can. */
function rejectionFor(
  candidate: RoutingCandidate,
  policy: RoutingPolicy,
  request: RoutingRequest,
): string | null {
  if (!candidate.available) return "not available right now";
  // Only an explicit "no" excludes. A provider model inventory reports
  // `unknown` for both capabilities (see `modelTargets.ts::providerTarget`),
  // and treating unknown as "cannot" would reject every cloud model the
  // moment a policy asked for tools — a criterion that silently matched
  // nothing. Unknown means the request is attempted and the provider gets to
  // answer, which is what happens without a policy too.
  if (request.requiresVision && candidate.vision === "no") {
    return "cannot see images, and this turn has one attached";
  }
  if ((request.requiresTools || policy.requiresTools) && candidate.toolCalling === "no") {
    return "cannot call tools";
  }
  if (policy.sensitivity === "local_only" && !candidate.isLocal) {
    return "runs off this machine, and the policy is local-only";
  }
  if (policy.maxInputPerMillionUsd !== null) {
    if (candidate.inputPerMillionUsd === null) return "has no input rate configured";
    if (candidate.inputPerMillionUsd > policy.maxInputPerMillionUsd) {
      return `input rate $${candidate.inputPerMillionUsd}/M is over the $${policy.maxInputPerMillionUsd}/M ceiling`;
    }
  }
  if (policy.maxOutputPerMillionUsd !== null) {
    if (candidate.outputPerMillionUsd === null) return "has no output rate configured";
    if (candidate.outputPerMillionUsd > policy.maxOutputPerMillionUsd) {
      return `output rate $${candidate.outputPerMillionUsd}/M is over the $${policy.maxOutputPerMillionUsd}/M ceiling`;
    }
  }
  // Measured-and-over is excluded; never-measured is not — see
  // `maxTimeToFirstTokenMs`'s doc comment.
  if (
    policy.maxTimeToFirstTokenMs !== null
    && candidate.observedTimeToFirstTokenMs !== null
    && candidate.observedTimeToFirstTokenMs > policy.maxTimeToFirstTokenMs
  ) {
    return `measured ${Math.round(candidate.observedTimeToFirstTokenMs)}ms to first token, over the ${policy.maxTimeToFirstTokenMs}ms target`;
  }
  return null;
}

/** Sort key for candidates that already satisfy the policy: the active target
 * first (session affinity — a policy should not reshuffle a working
 * conversation every turn), then cheapest measured output rate, then fastest
 * measured first token, then key, so the order is total and deterministic
 * rather than dependent on inventory iteration order. Unknown rate or latency
 * sorts after every known value instead of being treated as zero. */
function compareCandidates(
  a: RoutingCandidate,
  b: RoutingCandidate,
  activeKey: string | null,
  latencyMatters: boolean,
): number {
  if (a.key === activeKey && b.key !== activeKey) return -1;
  if (b.key === activeKey && a.key !== activeKey) return 1;
  // Compared, never subtracted: two unmeasured values are both
  // `POSITIVE_INFINITY`, and `Infinity - Infinity` is `NaN`, which makes a
  // comparator return "unordered" and leaves `sort` free to do anything. That
  // is the common case, not an exotic one — a fresh profile has no rates and
  // no latency samples for anything.
  const unknownLast = (value: number | null) =>
    value === null ? Number.POSITIVE_INFINITY : value;
  const byNumber = (left: number, right: number) =>
    left === right ? 0 : left < right ? -1 : 1;
  const byRate = byNumber(
    unknownLast(a.outputPerMillionUsd),
    unknownLast(b.outputPerMillionUsd),
  );
  if (byRate !== 0) return byRate;
  if (latencyMatters) {
    const byLatency = byNumber(
      unknownLast(a.observedTimeToFirstTokenMs),
      unknownLast(b.observedTimeToFirstTokenMs),
    );
    if (byLatency !== 0) return byLatency;
  }
  return a.key.localeCompare(b.key);
}

/**
 * Applies the first policy matching this request's task class and returns the
 * ordered attempt sequence, or an unrouted decision when no policy covers it.
 *
 * `activeKey` is the target the user currently has selected: it is both the
 * fallback when a policy can satisfy nothing and the affinity tiebreaker
 * above, so an enabled policy that happens to be satisfied by the current
 * model is a no-op rather than a switch.
 */
export function routeRequest(
  policies: readonly RoutingPolicy[],
  candidates: readonly RoutingCandidate[],
  request: RoutingRequest,
  activeKey: string | null,
): RoutingDecision {
  const policy = matchPolicy(policies, request.taskClass);
  if (!policy) {
    return unrouted(request.taskClass, "No enabled routing policy covers this task class.");
  }

  const rejected: RoutingRejection[] = [];
  const eligible: RoutingCandidate[] = [];
  for (const candidate of candidates) {
    const rejection = rejectionFor(candidate, policy, request);
    if (rejection) rejected.push({ key: candidate.key, reason: rejection });
    else eligible.push(candidate);
  }

  if (eligible.length === 0) {
    return {
      taskClass: request.taskClass,
      policyId: policy.id,
      policyName: policy.name,
      chosenKey: null,
      changedFromActive: false,
      sequence: [],
      reason: `“${policy.name}” matched this turn but no configured model satisfies it, so the active model was kept.`,
      rejected,
    };
  }

  // Preferred keys keep the user's own order; everything else that qualifies
  // follows as failover, ranked. A preferred key that was rejected above is
  // simply absent here — it is already recorded in `rejected` with why.
  const byKey = new Map(eligible.map((candidate) => [candidate.key, candidate]));
  const pinned = policy.preferredTargetKeys
    .map((key) => byKey.get(key))
    .filter((candidate): candidate is RoutingCandidate => candidate !== undefined);
  const pinnedKeys = new Set(pinned.map((candidate) => candidate.key));
  const rest = eligible
    .filter((candidate) => !pinnedKeys.has(candidate.key))
    .sort((a, b) => compareCandidates(a, b, activeKey, policy.maxTimeToFirstTokenMs !== null));

  const ordered = [...pinned, ...rest];
  const chosen = ordered[0];
  const pinnedFirst = pinnedKeys.has(chosen.key);
  const why = pinnedFirst
    ? "it is the policy's first available preferred model"
    : chosen.key === activeKey
      ? "the active model already satisfies the policy"
      : "it is the cheapest configured model that satisfies the policy";

  return {
    taskClass: request.taskClass,
    policyId: policy.id,
    policyName: policy.name,
    chosenKey: chosen.key,
    changedFromActive: chosen.key !== activeKey,
    sequence: ordered.map((candidate) => candidate.key),
    reason: `“${policy.name}” chose ${chosen.label} — ${why}.`,
    rejected,
  };
}

/**
 * Median measured time-to-first-token for `targetKey` over the most recent
 * `sampleSize` attempts that recorded one.
 *
 * Median rather than mean because one cold start or one rate-limited retry
 * would otherwise drag a target's latency far past a ceiling it normally
 * meets. Returns null when nothing has been measured, which every caller
 * treats as unknown rather than as fast or slow — this app does not display
 * or act on a latency number it did not measure itself.
 */
export function observedTimeToFirstTokenMs(
  samples: readonly { targetKey: string; timeToFirstTokenMs?: number | null }[],
  targetKey: string,
  sampleSize = 20,
): number | null {
  const observations: number[] = [];
  // Newest first: walk backwards and stop at `sampleSize`, so a long history
  // is not fully scanned and a target's recent behaviour is what counts.
  for (let index = samples.length - 1; index >= 0 && observations.length < sampleSize; index -= 1) {
    const sample = samples[index];
    if (sample.targetKey !== targetKey) continue;
    const value = sample.timeToFirstTokenMs;
    if (typeof value === "number" && Number.isFinite(value) && value >= 0) observations.push(value);
  }
  if (observations.length === 0) return null;
  observations.sort((a, b) => a - b);
  const middle = Math.floor(observations.length / 2);
  return observations.length % 2 === 1
    ? observations[middle]
    : (observations[middle - 1] + observations[middle]) / 2;
}
