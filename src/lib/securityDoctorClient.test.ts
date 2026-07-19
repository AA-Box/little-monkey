import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { runSecurityAudit } from "./securityDoctorClient";

describe("securityDoctorClient", () => {
  beforeEach(() => invokeMock.mockReset());

  it("runs read-only by default", async () => {
    invokeMock.mockResolvedValue({ findings: [] });
    await runSecurityAudit();
    expect(invokeMock).toHaveBeenCalledWith("security_audit", { deep: false, fix: false });
  });

  it("forwards deep and explicit safe-fix consent", async () => {
    invokeMock.mockResolvedValue({ findings: [] });
    await runSecurityAudit({ deep: true, fix: true });
    expect(invokeMock).toHaveBeenCalledWith("security_audit", { deep: true, fix: true });
  });
});
