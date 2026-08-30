import { invoke } from "@tauri-apps/api/core";

export type SkillActivationPolicy = "automatic" | "ask" | "manual";

export interface SkillActivationPreference {
  policy: SkillActivationPolicy;
  pinned: boolean;
  updated_at_unix_ms: number;
}

export interface SkillActivationEntry extends SkillActivationPreference {
  key: string;
}

export const skillActivationClient = {
  list: () => invoke<SkillActivationEntry[]>("skill_activation_list"),
  get: (key: string) => invoke<SkillActivationEntry | null>("skill_activation_get", { key }),
  set: (key: string, policy: SkillActivationPolicy, pinned: boolean) =>
    invoke<SkillActivationEntry>("skill_activation_set", { key, policy, pinned }),
  migrate: (entries: SkillActivationEntry[]) =>
    invoke<SkillActivationEntry[]>("skill_activation_migrate", { entries }),
};
