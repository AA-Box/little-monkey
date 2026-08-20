import { create } from "zustand";

export type SkillActivationPolicy = "automatic" | "ask" | "manual";

const STORAGE_KEY = "little-monkey.skill-activation-policies";

function readPolicies(): Record<string, SkillActivationPolicy> {
  if (typeof localStorage === "undefined") return {};
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}");
    if (!parsed || typeof parsed !== "object") return {};
    return Object.fromEntries(
      Object.entries(parsed).filter(([, value]) => value === "automatic" || value === "ask" || value === "manual"),
    ) as Record<string, SkillActivationPolicy>;
  } catch {
    return {};
  }
}

export interface SkillActivationPolicyStore {
  policies: Record<string, SkillActivationPolicy>;
  getPolicy: (key: string) => SkillActivationPolicy;
  setPolicy: (key: string, policy: SkillActivationPolicy) => void;
}

export const useSkillActivationPolicyStore = create<SkillActivationPolicyStore>((set, get) => ({
  policies: readPolicies(),
  getPolicy: (key) => get().policies[key] ?? "automatic",
  setPolicy: (key, policy) => {
    set((state) => {
      const policies = { ...state.policies, [key]: policy };
      if (typeof localStorage !== "undefined") localStorage.setItem(STORAGE_KEY, JSON.stringify(policies));
      return { policies };
    });
  },
}));

/** Stable keys keep a skill's policy across content updates. */
export function skillActivationPolicyKey(
  source: "local" | "native" | "package",
  command: string,
  id?: string,
): string {
  if (source === "local") return `local:${id ?? command}`;
  if (source === "package") return `package:${id ?? command}`;
  return `native:${id ?? "any"}:${command}`;
}

export function skillActivationPolicyFor(key: string): SkillActivationPolicy {
  return useSkillActivationPolicyStore.getState().getPolicy(key);
}
