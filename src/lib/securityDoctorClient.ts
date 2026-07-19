import { invoke } from "@tauri-apps/api/core";

export type SecurityFindingStatus = "pass" | "info" | "warning" | "critical" | "fixed";

export interface SecurityFinding {
  id: string;
  category: string;
  title: string;
  detail: string;
  status: SecurityFindingStatus;
  fixable: boolean;
  path: string | null;
  remediation: string | null;
}

export interface SecuritySummary {
  passed: number;
  informational: number;
  warnings: number;
  critical: number;
  fixed: number;
}

export interface SecurityAuditReport {
  schemaVersion: number;
  generatedAtMs: number;
  deep: boolean;
  fixRequested: boolean;
  summary: SecuritySummary;
  findings: SecurityFinding[];
}

export async function runSecurityAudit(
  options: { deep?: boolean; fix?: boolean } = {},
): Promise<SecurityAuditReport> {
  return invoke<SecurityAuditReport>("security_audit", {
    deep: options.deep ?? false,
    fix: options.fix ?? false,
  });
}
