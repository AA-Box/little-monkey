import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { useTeamModeStore, type TeamAuditReport, type TeamMember, type TeamMembersSnapshot } from "./teamModeStore";

function makeMember(overrides: Partial<TeamMember> = {}): TeamMember {
  return {
    id: "member-1",
    display_name: "Ada",
    role: "owner",
    created_at_ms: 1000,
    last_active_ms: 1000,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useTeamModeStore.setState({ members: [], currentMemberId: null, busy: false, error: null });
});

describe("teamModeStore.refresh", () => {
  it("calls team_members_list and stores the snapshot", async () => {
    const snapshot: TeamMembersSnapshot = { members: [makeMember()], current_member_id: "member-1" };
    invokeMock.mockResolvedValueOnce(snapshot);

    await useTeamModeStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("team_members_list");
    expect(useTeamModeStore.getState().members).toEqual([makeMember()]);
    expect(useTeamModeStore.getState().currentMemberId).toBe("member-1");
  });

  it("records the error message and clears busy on failure", async () => {
    invokeMock.mockRejectedValueOnce(new Error("disk error"));

    await expect(useTeamModeStore.getState().refresh()).rejects.toThrow("disk error");

    expect(useTeamModeStore.getState().error).toBe("disk error");
    expect(useTeamModeStore.getState().busy).toBe(false);
  });
});

describe("teamModeStore CRUD actions", () => {
  it("addMember invokes team_members_add with camelCase args then refreshes", async () => {
    const member = makeMember();
    invokeMock.mockResolvedValueOnce(member).mockResolvedValueOnce({ members: [member], current_member_id: member.id });

    const result = await useTeamModeStore.getState().addMember("Ada", "operator");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "team_members_add", { displayName: "Ada", role: "operator" });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "team_members_list");
    expect(result).toEqual(member);
    expect(useTeamModeStore.getState().members).toEqual([member]);
  });

  it("updateRole invokes team_members_update_role then refreshes", async () => {
    const updated = makeMember({ role: "approver" });
    invokeMock.mockResolvedValueOnce(updated).mockResolvedValueOnce({ members: [updated], current_member_id: updated.id });

    await useTeamModeStore.getState().updateRole("member-1", "approver");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "team_members_update_role", { id: "member-1", role: "approver" });
  });

  it("removeMember invokes team_members_remove then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce({ members: [], current_member_id: null });

    await useTeamModeStore.getState().removeMember("member-1");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "team_members_remove", { id: "member-1" });
    expect(useTeamModeStore.getState().members).toEqual([]);
  });

  it("setActive invokes team_members_set_active with a null id to clear the active member", async () => {
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce({ members: [], current_member_id: null });

    await useTeamModeStore.getState().setActive(null);

    expect(invokeMock).toHaveBeenNthCalledWith(1, "team_members_set_active", { id: null });
  });

  it("exportAudit invokes team_audit_export and returns the report without touching members", async () => {
    const report: TeamAuditReport = {
      generated_at_ms: 5000,
      members: [makeMember()],
      entries: [{ member_id: "member-1", member_role: "owner", action: "run:Interactive", occurred_at_ms: 4000, outcome: "Succeeded" }],
    };
    invokeMock.mockResolvedValueOnce(report);

    const result = await useTeamModeStore.getState().exportAudit(25);

    expect(invokeMock).toHaveBeenCalledWith("team_audit_export", { limit: 25 });
    expect(result).toEqual(report);
    expect(useTeamModeStore.getState().members).toEqual([]);
  });

  it("exportAudit defaults limit to null when omitted", async () => {
    invokeMock.mockResolvedValueOnce({ generated_at_ms: 0, members: [], entries: [] });

    await useTeamModeStore.getState().exportAudit();

    expect(invokeMock).toHaveBeenCalledWith("team_audit_export", { limit: null });
  });
});
