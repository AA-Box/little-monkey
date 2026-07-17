import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { useDiagnosticsStore, type DiagnosticFinding, type DiagnosticReport } from "./diagnosticsStore";

function makeFinding(overrides: Partial<DiagnosticFinding> = {}): DiagnosticFinding {
  return {
    id: "llama.reachability",
    subsystem: "llama",
    title: "Local chat model is stopped",
    detail: "Not currently running.",
    status: "pass",
    fixable: false,
    remediation: null,
    ...overrides,
  };
}

function makeReport(findings: DiagnosticFinding[]): DiagnosticReport {
  const summary = { passed: 0, informational: 0, warnings: 0, critical: 0, fixed: 0, notConfigured: 0 };
  const field: Record<DiagnosticFinding["status"], keyof typeof summary> = {
    pass: "passed",
    info: "informational",
    warning: "warnings",
    critical: "critical",
    fixed: "fixed",
    not_configured: "notConfigured",
  };
  for (const finding of findings) summary[field[finding.status]] += 1;
  return { schemaVersion: 1, generatedAtMs: 1700000000000, summary, findings };
}

beforeEach(() => {
  invokeMock.mockReset();
  useDiagnosticsStore.setState({ report: null, bundle: null, busy: {}, error: null });
});

describe("diagnosticsStore.run", () => {
  it("invokes diagnostics_run with no args and stores the returned report", async () => {
    const report = makeReport([makeFinding()]);
    invokeMock.mockResolvedValueOnce(report);

    const result = await useDiagnosticsStore.getState().run();

    expect(invokeMock).toHaveBeenCalledWith("diagnostics_run");
    expect(result).toEqual(report);
    expect(useDiagnosticsStore.getState().report).toEqual(report);
    expect(useDiagnosticsStore.getState().busy.run).toBe(false);
  });

  it("surfaces a rejected run without swallowing it, and clears busy", async () => {
    invokeMock.mockRejectedValueOnce(new Error("backend unavailable"));

    await expect(useDiagnosticsStore.getState().run()).rejects.toThrow("backend unavailable");
    expect(useDiagnosticsStore.getState().error).toBe("backend unavailable");
    expect(useDiagnosticsStore.getState().busy.run).toBe(false);
  });
});

describe("diagnosticsStore.applyFix", () => {
  it("passes findingId through and replaces just that finding in the report", async () => {
    const critical = makeFinding({ id: "llama.reachability", status: "critical", fixable: true });
    const other = makeFinding({ id: "ollama.reachability", status: "info", fixable: false });
    useDiagnosticsStore.setState({ report: makeReport([critical, other]) });

    const fixed = makeFinding({ id: "llama.reachability", status: "fixed", fixable: true, detail: "Restarted." });
    invokeMock.mockResolvedValueOnce(fixed);

    const result = await useDiagnosticsStore.getState().applyFix("llama.reachability");

    expect(invokeMock).toHaveBeenCalledWith("diagnostics_apply_fix", { findingId: "llama.reachability" });
    expect(result).toEqual(fixed);
    const { report } = useDiagnosticsStore.getState();
    expect(report?.findings.find((f) => f.id === "llama.reachability")).toEqual(fixed);
    expect(report?.findings.find((f) => f.id === "ollama.reachability")).toEqual(other);
  });

  it("moves the fixed finding's count from its old bucket into fixed in the summary", async () => {
    const critical = makeFinding({ id: "llama.reachability", status: "critical", fixable: true });
    useDiagnosticsStore.setState({ report: makeReport([critical]) });
    expect(useDiagnosticsStore.getState().report?.summary.critical).toBe(1);
    expect(useDiagnosticsStore.getState().report?.summary.fixed).toBe(0);

    invokeMock.mockResolvedValueOnce(makeFinding({ id: "llama.reachability", status: "fixed", fixable: true }));
    await useDiagnosticsStore.getState().applyFix("llama.reachability");

    const { summary } = useDiagnosticsStore.getState().report!;
    expect(summary.critical).toBe(0);
    expect(summary.fixed).toBe(1);
  });

  it("tracks busy per finding id so one fix in flight doesn't block another", async () => {
    let resolveFirst: (value: DiagnosticFinding) => void = () => {};
    invokeMock.mockImplementationOnce(
      () => new Promise<DiagnosticFinding>((resolve) => { resolveFirst = resolve; }),
    );
    useDiagnosticsStore.setState({ report: makeReport([makeFinding({ id: "a" }), makeFinding({ id: "b" })]) });

    const inFlight = useDiagnosticsStore.getState().applyFix("a");
    expect(useDiagnosticsStore.getState().busy["fix-a"]).toBe(true);
    expect(useDiagnosticsStore.getState().busy["fix-b"]).toBeFalsy();

    resolveFirst(makeFinding({ id: "a", status: "fixed" }));
    await inFlight;
    expect(useDiagnosticsStore.getState().busy["fix-a"]).toBe(false);
  });
});

describe("diagnosticsStore.exportBundle", () => {
  it("invokes diagnostics_export_bundle and stores the returned bundle", async () => {
    const bundle = {
      schemaVersion: 1,
      generatedAtMs: 1700000000000,
      appVersion: "0.1.0",
      platform: "macos",
      report: makeReport([makeFinding()]),
    };
    invokeMock.mockResolvedValueOnce(bundle);

    const result = await useDiagnosticsStore.getState().exportBundle();

    expect(invokeMock).toHaveBeenCalledWith("diagnostics_export_bundle");
    expect(result).toEqual(bundle);
    expect(useDiagnosticsStore.getState().bundle).toEqual(bundle);
  });
});

describe("diagnosticsStore.dismissBundle", () => {
  it("clears bundle without touching anything else", () => {
    useDiagnosticsStore.setState({ bundle: { schemaVersion: 1, generatedAtMs: 0, appVersion: "0.1.0", platform: "macos", report: makeReport([]) } });
    useDiagnosticsStore.getState().dismissBundle();
    expect(useDiagnosticsStore.getState().bundle).toBeNull();
  });
});

describe("diagnosticsStore.clearError", () => {
  it("clears error without touching anything else", () => {
    useDiagnosticsStore.setState({ error: "boom" });
    useDiagnosticsStore.getState().clearError();
    expect(useDiagnosticsStore.getState().error).toBeNull();
  });
});
