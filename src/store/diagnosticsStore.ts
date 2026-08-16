import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { errorMessage } from "../lib/errors";

/** Mirrors the Rust `DiagnosticStatus` enum (src-tauri/src/diagnostics.rs)
 * exactly — `#[serde(rename_all = "snake_case")]` on already-lowercase or
 * two-word variant names serializes to exactly these strings. */
export type DiagnosticStatus =
  "pass" | "info" | "warning" | "critical" | "fixed" | "not_configured";

/** Mirrors the Rust `DiagnosticFinding` struct exactly. */
export interface DiagnosticFinding {
  id: string;
  subsystem: string;
  title: string;
  detail: string;
  status: DiagnosticStatus;
  fixable: boolean;
  remediation: string | null;
}

/** Mirrors the Rust `DiagnosticSummary` struct exactly. */
export interface DiagnosticSummary {
  passed: number;
  informational: number;
  warnings: number;
  critical: number;
  fixed: number;
  notConfigured: number;
}

/** Mirrors the Rust `DiagnosticReport` struct exactly. */
export interface DiagnosticReport {
  schemaVersion: number;
  generatedAtMs: number;
  summary: DiagnosticSummary;
  findings: DiagnosticFinding[];
}

/** Mirrors the Rust `support_bundle::TraceEvent` struct exactly. */
export interface SupportTraceEvent {
  atMs: number;
  event: string;
  subject?: string;
  context?: string;
  outcome?: string;
  reason?: string;
}

/** Mirrors the Rust `support_bundle::TraceSection` struct exactly. */
export interface SupportTraceSection {
  events: SupportTraceEvent[];
  /** How many events were dropped to fit the section cap. */
  omitted: number;
  /**
   * Why this section is empty, when it is empty for a reason other than
   * nothing having happened. Rendering an unavailable section as an empty one
   * is how a reader concludes a subsystem was idle when it was unreadable.
   */
  unavailable?: string;
}

/** Mirrors the Rust `support_bundle::SupportBundle` struct exactly. */
export interface SupportTrace {
  schemaVersion: number;
  generatedAtMs: number;
  appVersion: string;
  platform: string;
  redaction: { identifiersPseudonymized: boolean; excluded: string[] };
  /** Keyed by subsystem: `channels`, `telephony`, `peers`, `devices`. */
  sections: Record<string, SupportTraceSection>;
}

/** Mirrors the Rust `DiagnosticsBundle` struct exactly. */
export interface DiagnosticsBundle {
  schemaVersion: number;
  generatedAtMs: number;
  appVersion: string;
  platform: string;
  report: DiagnosticReport;
  /**
   * Redacted lifecycle trace for the daemon-owned subsystems. Absent when the
   * background service could not be asked — see `traceUnavailable`, which is
   * the reason, and is not the same thing as a trace with no events in it.
   */
  trace?: SupportTrace;
  traceUnavailable?: string;
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

const SUMMARY_FIELD: Record<DiagnosticStatus, keyof DiagnosticSummary> = {
  pass: "passed",
  info: "informational",
  warning: "warnings",
  critical: "critical",
  fixed: "fixed",
  not_configured: "notConfigured",
};

/** Applied after a successful `applyFix`: replaces the one finding in place
 * and moves its count from its old status bucket to `fixed` in the summary,
 * so the header tallies stay correct without a full `run()` round trip. */
function replaceFindingAndResummarize(
  report: DiagnosticReport,
  updated: DiagnosticFinding,
): DiagnosticReport {
  const previous = report.findings.find((finding) => finding.id === updated.id);
  const summary = { ...report.summary };
  if (previous) summary[SUMMARY_FIELD[previous.status]] -= 1;
  summary[SUMMARY_FIELD[updated.status]] += 1;
  return {
    ...report,
    summary,
    findings: report.findings.map((finding) =>
      finding.id === updated.id ? updated : finding,
    ),
  };
}

export interface DiagnosticsStore {
  report: DiagnosticReport | null;
  /** The most recently exported support bundle, held only in memory until
   * the user copies or downloads it — never persisted anywhere. */
  bundle: DiagnosticsBundle | null;
  /** Keyed by finding id so one finding's fix in flight never disables the
   * "Run diagnosis" button or another finding's own fix button. */
  busy: Record<string, boolean>;
  error: string | null;

  clearError: () => void;
  run: () => Promise<DiagnosticReport>;
  applyFix: (findingId: string) => Promise<DiagnosticFinding>;
  exportBundle: () => Promise<DiagnosticsBundle>;
  dismissBundle: () => void;
}

export const useDiagnosticsStore = create<DiagnosticsStore>((set) => {
  const perform = async <T>(
    key: string,
    task: () => Promise<T>,
  ): Promise<T> => {
    set((state) => ({ busy: { ...state.busy, [key]: true }, error: null }));
    try {
      return await task();
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    } finally {
      set((state) => ({ busy: { ...state.busy, [key]: false } }));
    }
  };

  return {
    report: null,
    bundle: null,
    busy: {},
    error: null,

    clearError: () => set({ error: null }),

    run: () =>
      perform("run", async () => {
        const report = await invoke<DiagnosticReport>("diagnostics_run");
        set({ report });
        return report;
      }),

    applyFix: (findingId) =>
      perform(`fix-${findingId}`, async () => {
        const updated = await invoke<DiagnosticFinding>(
          "diagnostics_apply_fix",
          { findingId },
        );
        set((state) => ({
          report: state.report
            ? replaceFindingAndResummarize(state.report, updated)
            : state.report,
        }));
        return updated;
      }),

    exportBundle: () =>
      perform("export", async () => {
        const bundle = await invoke<DiagnosticsBundle>(
          "diagnostics_export_bundle",
        );
        set({ bundle });
        return bundle;
      }),

    dismissBundle: () => set({ bundle: null }),
  };
});

export default useDiagnosticsStore;
