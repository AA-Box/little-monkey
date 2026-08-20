import { afterEach, describe, expect, it } from "vitest";
import { cancelPendingSkillActivationApprovals, useSkillActivationApprovalStore } from "./skillActivationApprovalStore";
import type { SlashSkill } from "../lib/skills";

const skill: SlashSkill = {
  id: "test",
  source: "native",
  command: "deploy",
  name: "Deploy",
  description: "Deploy the project",
  instructions: "deploy",
  version: "1",
  contentSha256: "hash",
  permissions: [],
};

afterEach(() => cancelPendingSkillActivationApprovals());

describe("skill activation approval queue", () => {
  it("resolves Allow once", async () => {
    const decision = useSkillActivationApprovalStore.getState().request(skill);
    expect(useSkillActivationApprovalStore.getState().pending?.command).toBe("deploy");
    useSkillActivationApprovalStore.getState().allowOnce();
    await expect(decision).resolves.toBe(true);
  });

  it("resolves Deny", async () => {
    const decision = useSkillActivationApprovalStore.getState().request(skill);
    useSkillActivationApprovalStore.getState().deny();
    await expect(decision).resolves.toBe(false);
  });

  it("cancels an approval when the turn aborts", async () => {
    const controller = new AbortController();
    const decision = useSkillActivationApprovalStore.getState().request(skill, controller.signal);
    controller.abort();
    await expect(decision).resolves.toBe(false);
    expect(useSkillActivationApprovalStore.getState().pending).toBeNull();
  });

  it("serializes parallel Ask requests", async () => {
    const first = useSkillActivationApprovalStore.getState().request(skill);
    const secondSkill = { ...skill, id: "test-2", command: "review", name: "Review" };
    const second = useSkillActivationApprovalStore.getState().request(secondSkill);
    expect(useSkillActivationApprovalStore.getState().pending?.command).toBe("deploy");
    useSkillActivationApprovalStore.getState().allowOnce();
    await expect(first).resolves.toBe(true);
    expect(useSkillActivationApprovalStore.getState().pending?.command).toBe("review");
    useSkillActivationApprovalStore.getState().deny();
    await expect(second).resolves.toBe(false);
  });
});
