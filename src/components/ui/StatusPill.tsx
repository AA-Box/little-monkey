import type { ReactNode } from "react";

export type PillTone = "neutral" | "success" | "warning" | "danger";

export interface StatusPillProps {
  tone?: PillTone;
  children?: ReactNode;
}

const TONE_CLASSES: Record<PillTone, string> = {
  neutral: "bg-surface-2 text-muted",
  success: "bg-success-soft text-success",
  warning: "bg-warning-soft text-warning",
  danger: "bg-danger-soft text-danger",
};

const DOT_CLASSES: Record<PillTone, string> = {
  neutral: "bg-faint",
  success: "bg-success",
  warning: "bg-warning",
  danger: "bg-danger",
};

export function StatusPill({ tone = "neutral", children }: StatusPillProps) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium ${TONE_CLASSES[tone]}`}
    >
      <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${DOT_CLASSES[tone]}`} />
      {children}
    </span>
  );
}
