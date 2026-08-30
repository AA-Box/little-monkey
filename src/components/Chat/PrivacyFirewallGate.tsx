import { useEffect, useState } from "react";
import { APPROVAL_LAYER } from "../../lib/overlayLayers";
import { Ban, Laptop, Lock, ShieldAlert, X } from "lucide-react";
import { usePrivacyFirewallStore, type PrivacyFinding } from "../../store/privacyFirewallStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";

/**
 * Renders whenever `usePrivacyFirewallStore().pendingApproval` is set — a
 * `block` or `require_approval` verdict paused mid-turn (see
 * `agentLoop.ts`'s pre-turn gate). Mounted once, globally, in `App.tsx`,
 * exactly like `PermissionModal.tsx`. Every button here calls
 * `resolveDecision`, which settles the `Promise` the paused turn is actually
 * `await`ing — nothing is sent to a cloud model until the user clicks one of
 * these.
 */
export function PrivacyFirewallGate() {
  const pending = usePrivacyFirewallStore((state) => state.pendingApproval);
  const resolveDecision = usePrivacyFirewallStore((state) => state.resolveDecision);
  const [entered, setEntered] = useState(false);
  const { t } = useT();

  useEffect(() => {
    if (!pending) return;
    setEntered(false);
    const raf = requestAnimationFrame(() => setEntered(true));
    return () => cancelAnimationFrame(raf);
  }, [pending?.digest]);

  if (!pending) return null;

  const isBlock = pending.report.verdict === "block";
  const findingsToShow = pending.report.findings.filter((finding) => finding.action !== "allow");

  return (
    <div
      className={`fixed inset-0 ${APPROVAL_LAYER} flex items-center justify-center bg-black/40 p-4 backdrop-blur-[2px]`}
      role="dialog"
      aria-modal="true"
      aria-labelledby="privacy-firewall-gate-title"
    >
      <div
        className={`w-full max-w-lg rounded-xl border border-border bg-background p-5 shadow-xl transition-all duration-200 ease-out ${
          entered ? "scale-100 opacity-100" : "scale-95 opacity-0"
        }`}
      >
        <div className="flex items-start gap-3">
          <div className={`flex h-9 w-9 shrink-0 items-center justify-center rounded-full ${isBlock ? "bg-danger-soft text-danger" : "bg-warning-soft text-warning"}`}>
            {isBlock ? <Ban size={18} /> : <ShieldAlert size={18} />}
          </div>
          <div className="min-w-0 pt-0.5">
            <h2 id="privacy-firewall-gate-title" className="text-sm font-semibold text-foreground">
              {isBlock ? t("PrivacyFirewallGate.blockedTitle") : t("PrivacyFirewallGate.approvalTitle")}
            </h2>
            <p className="mt-0.5 text-xs text-muted">
              {t("PrivacyFirewallGate.findingsSummary", { count: findingsToShow.length })}
            </p>
          </div>
        </div>

        <ul className="mt-4 flex max-h-40 flex-col gap-1.5 overflow-auto rounded-md border border-border bg-surface-2 p-2.5">
          {findingsToShow.map((finding: PrivacyFinding, index) => (
            <li key={`${finding.kind}-${finding.byteStart}-${index}`} className="flex items-center justify-between gap-2 text-xs">
              <span className="font-mono text-muted">{finding.maskedPreview}</span>
              <span className="shrink-0 rounded-full border border-border px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-faint">
                {t(`PrivacyFirewallGate.kind.${finding.kind}`)}
              </span>
            </li>
          ))}
        </ul>

        <div className="mt-3 rounded-md border border-border bg-surface-2 p-2.5">
          <p className="text-[10px] font-medium uppercase tracking-wide text-faint">{t("PrivacyFirewallGate.redactedPreviewLabel")}</p>
          <p className="mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] text-muted">
            {pending.report.redactedPreview}
          </p>
        </div>

        <div className="mt-4 flex flex-col gap-2 sm:flex-row sm:flex-wrap sm:justify-end">
          <Button type="button" variant="secondary" onClick={() => void resolveDecision("cancel")}>
            <X size={14} />
            {t("PrivacyFirewallGate.cancelButton")}
          </Button>
          {pending.report.localOnlyFallbackAvailable && (
            <Button type="button" variant="secondary" onClick={() => void resolveDecision("switch_local")}>
              <Laptop size={14} />
              {t("PrivacyFirewallGate.switchLocalButton")}
            </Button>
          )}
          {!isBlock && (
            <>
              <Button type="button" variant="primary" onClick={() => void resolveDecision("send_redacted")} autoFocus>
                <Lock size={14} />
                {t("PrivacyFirewallGate.sendRedactedButton")}
              </Button>
              <Button type="button" variant="danger" onClick={() => void resolveDecision("send_unredacted")}>
                {t("PrivacyFirewallGate.sendUnredactedButton")}
              </Button>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

export default PrivacyFirewallGate;
