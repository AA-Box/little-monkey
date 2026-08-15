import { create } from "zustand";

import {
  ROUTING_TASK_CLASSES,
  type RoutingDecision,
  type RoutingPolicy,
  type RoutingSensitivity,
  type RoutingTaskClass,
} from "../lib/modelRouting";

/**
 * The user's authored dispatch policies, in
 * the order they are evaluated.
 *
 * The list *is* the precedence: `matchPolicy` takes the first enabled policy
 * covering a task class, so `movePolicy` is the whole reordering story and
 * there is no priority number that could disagree with the visible order.
 *
 * Persistence follows `costControlStore.ts` exactly (versioned localStorage
 * blob, every field re-validated on read) because the same rule applies: a
 * hand-edited or half-written blob must degrade to defaults, never to a
 * policy that silently routes turns somewhere the user did not author.
 */
export const ROUTING_POLICY_STORAGE_KEY = "little-monkey-routing-policies-v1";
export const MAX_ROUTING_POLICIES = 20;

export interface RoutingPolicyState {
  policies: RoutingPolicy[];
  /** The most recent decision, for per-turn inspection in Settings. Not
   * persisted — it describes this session's dispatch, and a stale one from
   * last week would read as current. */
  lastDecision: RoutingDecision | null;
  addPolicy: (patch?: Partial<RoutingPolicy>) => RoutingPolicy;
  updatePolicy: (id: string, patch: Partial<RoutingPolicy>) => void;
  removePolicy: (id: string) => void;
  /** Moves a policy by `offset` positions, clamped to the list — this is what
   * changes precedence. */
  movePolicy: (id: string, offset: number) => void;
  recordDecision: (decision: RoutingDecision) => void;
}

interface PersistedShape {
  version: 1;
  policies: RoutingPolicy[];
}

export function defaultRoutingPolicy(): RoutingPolicy {
  return {
    id: crypto.randomUUID(),
    name: "New policy",
    enabled: false,
    taskClasses: [],
    preferredTargetKeys: [],
    requiresTools: false,
    sensitivity: "any",
    maxInputPerMillionUsd: null,
    maxOutputPerMillionUsd: null,
    maxTimeToFirstTokenMs: null,
  };
}

function optionalPositive(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : null;
}

function stringList(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const seen = new Set<string>();
  for (const entry of value) {
    if (typeof entry === "string" && entry.length > 0) seen.add(entry);
  }
  return [...seen];
}

function taskClassList(value: unknown): RoutingTaskClass[] {
  if (!Array.isArray(value)) return [];
  return ROUTING_TASK_CLASSES.filter((taskClass) => value.includes(taskClass));
}

function sanitizeSensitivity(value: unknown): RoutingSensitivity {
  return value === "local_only" ? "local_only" : "any";
}

export function sanitizeRoutingPolicy(value: unknown): RoutingPolicy | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<RoutingPolicy>;
  if (typeof candidate.id !== "string" || !candidate.id) return null;
  const name = typeof candidate.name === "string" ? candidate.name.trim() : "";
  return {
    id: candidate.id,
    // A nameless policy is unidentifiable in the decision note that reports
    // it, so it gets a placeholder rather than an empty quote.
    name: name || "Untitled policy",
    enabled: candidate.enabled === true,
    taskClasses: taskClassList(candidate.taskClasses),
    preferredTargetKeys: stringList(candidate.preferredTargetKeys),
    requiresTools: candidate.requiresTools === true,
    sensitivity: sanitizeSensitivity(candidate.sensitivity),
    maxInputPerMillionUsd: optionalPositive(candidate.maxInputPerMillionUsd),
    maxOutputPerMillionUsd: optionalPositive(candidate.maxOutputPerMillionUsd),
    maxTimeToFirstTokenMs: optionalPositive(candidate.maxTimeToFirstTokenMs),
  };
}

function hydrate(): PersistedShape {
  try {
    const raw = localStorage.getItem(ROUTING_POLICY_STORAGE_KEY);
    if (!raw) return { version: 1, policies: [] };
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (!parsed || typeof parsed !== "object") return { version: 1, policies: [] };
    return {
      version: 1,
      policies: (Array.isArray(parsed.policies) ? parsed.policies : [])
        .map(sanitizeRoutingPolicy)
        .filter((policy): policy is RoutingPolicy => policy !== null)
        .slice(0, MAX_ROUTING_POLICIES),
    };
  } catch {
    return { version: 1, policies: [] };
  }
}

function persist(policies: readonly RoutingPolicy[]): void {
  try {
    localStorage.setItem(
      ROUTING_POLICY_STORAGE_KEY,
      JSON.stringify({
        version: 1,
        policies: policies.slice(0, MAX_ROUTING_POLICIES),
      } satisfies PersistedShape),
    );
  } catch {
    // Same posture as cost accounting: a full or unavailable storage quota
    // must never make a model call fail.
  }
}

const initial = hydrate();

export const useRoutingPolicyStore = create<RoutingPolicyState>((set, get) => ({
  policies: initial.policies,
  lastDecision: null,

  addPolicy: (patch) => {
    const policy = sanitizeRoutingPolicy({ ...defaultRoutingPolicy(), ...patch })
      ?? defaultRoutingPolicy();
    set((state) => ({ policies: [...state.policies, policy].slice(0, MAX_ROUTING_POLICIES) }));
    persist(get().policies);
    return policy;
  },

  updatePolicy: (id, patch) => {
    set((state) => ({
      policies: state.policies.map((policy) =>
        policy.id === id
          ? sanitizeRoutingPolicy({ ...policy, ...patch, id: policy.id }) ?? policy
          : policy,
      ),
    }));
    persist(get().policies);
  },

  removePolicy: (id) => {
    set((state) => ({ policies: state.policies.filter((policy) => policy.id !== id) }));
    persist(get().policies);
  },

  movePolicy: (id, offset) => {
    set((state) => {
      const index = state.policies.findIndex((policy) => policy.id === id);
      if (index === -1) return state;
      const target = Math.min(state.policies.length - 1, Math.max(0, index + offset));
      if (target === index) return state;
      const policies = [...state.policies];
      const [moved] = policies.splice(index, 1);
      policies.splice(target, 0, moved);
      return { policies };
    });
    persist(get().policies);
  },

  recordDecision: (decision) => set({ lastDecision: decision }),
}));

export default useRoutingPolicyStore;
