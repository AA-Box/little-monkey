export interface VerifyResult {
  commandId: string;
  label: string;
  kind: string;
  code: number | null;
  stdout: string;
  stderr: string;
  durationMs: number;
  timedOut: boolean;
}
