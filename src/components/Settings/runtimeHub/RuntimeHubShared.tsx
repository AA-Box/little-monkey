import type { ReactNode } from "react";
import { AlertCircle, CheckCircle2, LoaderCircle, TriangleAlert } from "lucide-react";
import { Button } from "../../ui";
import type { M3HardwareCompatibilityReport, M3LocalModelStalenessWarning } from "../../../lib/runtimeHubClient";

/** Re-exported so every Runtime Hub tab keeps its single import site
 * while the implementation stays shared with the rest of the app. */
export { formatBytes } from "../../../lib/format";

export const CONTROL_CLASS =
  "min-h-11 w-full rounded-md border border-border bg-surface-2 px-3 py-2 text-sm text-foreground placeholder:text-faint focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-not-allowed disabled:opacity-50";


export function formatDate(value: number | null | undefined): string {
  return value ? new Date(value).toLocaleString() : "Never";
}

export function labelize(value: string): string {
  return value
    .replace(/_/g, " ")
    .replace(/\b\w/g, (letter: string) => letter.toUpperCase());
}

export function Field({
  label,
  hint,
  children,
}: {
  label: ReactNode;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <label className="flex min-w-0 flex-col gap-1.5 text-sm font-medium text-foreground">
      <span>{label}</span>
      {children}
      {hint && <span className="text-xs font-normal text-muted">{hint}</span>}
    </label>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
  description,
  disabled,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label: string;
  description?: string;
  disabled?: boolean;
}) {
  return (
    <label className="flex min-h-11 cursor-pointer items-center justify-between gap-3 rounded-md border border-border bg-surface-2 px-3 py-2">
      <span className="min-w-0">
        <span className="block text-sm font-medium text-foreground">{label}</span>
        {description && <span className="block text-xs text-muted">{description}</span>}
      </span>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        aria-label={label}
        disabled={disabled}
        onClick={() => onChange(!checked)}
        className={`relative h-7 w-12 shrink-0 cursor-pointer rounded-full transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-not-allowed disabled:opacity-50 ${
          checked ? "bg-accent" : "bg-surface border border-border-strong"
        }`}
      >
        <span
          className={`absolute top-1 h-5 w-5 rounded-full bg-white shadow-sm transition-[left] ${
            checked ? "left-6" : "left-1"
          }`}
        />
      </button>
    </label>
  );
}

export function SectionHeading({ title, description, action }: { title: string; description?: string; action?: ReactNode }) {
  return (
    <div className="flex flex-wrap items-start justify-between gap-3">
      <div className="min-w-0">
        <h3 className="text-sm font-semibold text-foreground">{title}</h3>
        {description && <p className="mt-1 text-xs leading-5 text-muted">{description}</p>}
      </div>
      {action}
    </div>
  );
}

export function ErrorNotice({ message }: { message?: string | null }) {
  if (!message) return null;
  return (
    <div role="alert" className="flex items-start gap-2 rounded-lg border border-danger/30 bg-danger-soft px-3 py-2.5 text-sm text-danger">
      <AlertCircle size={16} className="mt-0.5 shrink-0" aria-hidden="true" />
      <span className="break-words">{message}</span>
    </div>
  );
}

export function SuccessNotice({ children }: { children: ReactNode }) {
  return (
    <div role="status" className="flex items-start gap-2 rounded-lg border border-success/30 bg-success-soft px-3 py-2.5 text-sm text-success">
      <CheckCircle2 size={16} className="mt-0.5 shrink-0" aria-hidden="true" />
      <span className="min-w-0 break-words">{children}</span>
    </div>
  );
}

const RISKY_ACCELERATOR_STATUSES = new Set(["driver_too_old", "tool_missing", "unsupported"]);

/**
 * Accelerators worth interrupting the user for: a driver that's too old, a
 * detection tool that's missing, or a backend this OS/arch can't run at
 * all. `not_detected`/`available` are excluded — those are normal, quiet
 * outcomes that don't need a warning banner.
 */
export function riskyAccelerators(
  report: M3HardwareCompatibilityReport,
): M3HardwareCompatibilityReport["accelerators"] {
  return report.accelerators.filter((accelerator) => RISKY_ACCELERATOR_STATUSES.has(accelerator.status));
}

/**
 * Hardware Compatibility Matrix / "Driver Doctor" warning banner. Renders
 * nothing when the report has no risky backend (this is the common case on
 * a healthy machine) so it only interrupts the model-download, model-load,
 * and runtime-install flows when there is something actionable to say.
 */
export function CompatibilityWarningBanner({
  report,
}: {
  report: M3HardwareCompatibilityReport | null;
}) {
  if (!report) return null;
  const risky = riskyAccelerators(report);
  if (!risky.length && !report.notes.length) return null;
  return (
    <div
      role="alert"
      className="flex items-start gap-2 rounded-lg border border-warning/30 bg-warning-soft px-3 py-2.5 text-sm text-warning"
    >
      <TriangleAlert size={16} className="mt-0.5 shrink-0" aria-hidden="true" />
      <div className="min-w-0 space-y-1">
        <p className="font-medium">Hardware compatibility notes before you continue</p>
        <ul className="list-disc space-y-0.5 pl-4 text-xs leading-5">
          {risky.map((accelerator) => (
            <li key={accelerator.kind}>
              {labelize(accelerator.kind)}: {accelerator.summary}
            </li>
          ))}
          {report.notes.map((note, index) => (
            <li key={`note-${index}`}>{note}</li>
          ))}
        </ul>
      </div>
    </div>
  );
}

/**
 * Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14):
 * an installed local model whose catalog has moved on to a different
 * revision, and which hasn't been refreshed in a long time. Shown in the
 * "Load model" flow — before the load actually starts — with a concrete
 * migration path (the newer catalog entry's display name), mirroring
 * `CompatibilityWarningBanner`'s "render nothing on the common case" shape.
 */
export function ModelRetirementWarningBanner({
  warning,
}: {
  warning: M3LocalModelStalenessWarning | null | undefined;
}) {
  if (!warning) return null;
  const ageDays = Math.floor(warning.ageMs / (24 * 60 * 60 * 1000));
  return (
    <div
      role="alert"
      className="flex items-start gap-2 rounded-lg border border-warning/30 bg-warning-soft px-3 py-2.5 text-sm text-warning"
    >
      <TriangleAlert size={16} className="mt-0.5 shrink-0" aria-hidden="true" />
      <div className="min-w-0 space-y-1">
        <p className="font-medium">This installed model looks outdated</p>
        <p className="text-xs leading-5">
          Installed revision {warning.installedRevision} hasn&apos;t been refreshed in about {ageDays} day{ageDays === 1 ? "" : "s"}, and the configured catalog now lists revision {warning.latestRevision}. Migration path: update to &ldquo;{warning.suggestedReplacementDisplayName}&rdquo; from the model catalog below before loading, or use &ldquo;Find updates&rdquo; on this model&apos;s installed card.
        </p>
      </div>
    </div>
  );
}

export function BusyButton({ busy, children, ...props }: React.ComponentProps<typeof Button> & { busy?: boolean }) {
  return (
    <Button {...props} disabled={busy || props.disabled} className={`min-h-11 ${props.className ?? ""}`}>
      {busy && <LoaderCircle size={15} className="animate-spin motion-reduce:animate-none" aria-hidden="true" />}
      {children}
    </Button>
  );
}

export function JsonView({ value, label }: { value: unknown; label: string }) {
  return (
    <div className="min-w-0">
      <p className="mb-1.5 text-xs font-medium text-muted">{label}</p>
      <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-lg border border-border bg-surface-2 p-3 font-mono text-xs leading-5 text-foreground [overscroll-behavior:contain]">
        {JSON.stringify(value, null, 2)}
      </pre>
    </div>
  );
}
