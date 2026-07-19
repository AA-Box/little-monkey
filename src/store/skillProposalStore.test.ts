import { beforeAll, beforeEach, describe, expect, it } from "vitest";

import { usePromptStore } from "./promptStore";
import { useSkillProposalStore } from "./skillProposalStore";

describe("governed /learn proposals", () => {
  beforeAll(() => {
    const values = new Map<string, string>();
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        clear: () => values.clear(),
        getItem: (key: string) => values.get(key) ?? null,
        setItem: (key: string, value: string) => values.set(key, value),
        removeItem: (key: string) => values.delete(key),
      },
    });
  });

  beforeEach(() => {
    localStorage.clear();
    usePromptStore.setState({ entries: [], defaultPersonaId: null, hasSeededDefaults: true, persistError: null });
    useSkillProposalStore.setState({ proposals: [] });
  });

  it("quarantines a digest-bound proposal before explicit approval", async () => {
    const proposal = await useSkillProposalStore.getState().createProposal("release-check", "Review the release diff and report risks.");
    expect(proposal.status).toBe("quarantined");
    expect(usePromptStore.getState().entries).toHaveLength(0);

    await useSkillProposalStore.getState().approveProposal(proposal.id, proposal.contentSha256);
    expect(usePromptStore.getState().entries[0]).toMatchObject({ kind: "skill", command: "release-check" });
  });

  it("rejects built-in collisions and supports rollback", async () => {
    await expect(useSkillProposalStore.getState().createProposal("status", "Always report project status clearly."))
      .rejects.toThrow("reserved");
    const proposal = await useSkillProposalStore.getState().createProposal("explain-code", "Explain selected code in plain language.");
    await useSkillProposalStore.getState().approveProposal(proposal.id, proposal.contentSha256);
    useSkillProposalStore.getState().rollbackProposal(proposal.id);
    expect(usePromptStore.getState().entries).toHaveLength(0);
    expect(useSkillProposalStore.getState().proposals[0].status).toBe("rolled_back");
  });
});
