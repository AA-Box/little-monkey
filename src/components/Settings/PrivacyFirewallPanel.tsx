import { useEffect, useMemo, useState } from "react";
import { AlertOctagon, Ban, Eye, Lock, ShieldCheck, ShieldQuestion, X } from "lucide-react";
import {
  PRIVACY_POLICY_ACTIONS,
  SENSITIVE_DATA_KINDS,
  usePrivacyFirewallStore,
  type PrivacyPolicyAction,
  type SensitiveDataKind,
} from "../../store/privacyFirewallStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";

const ACTION_ICONS: Record<PrivacyPolicyAction, typeof Ban> = {
  allow: Eye,
  redact: ShieldQuestion,
  block: Ban,
  require_approval: AlertOctagon,
};

const ACTION_STYLE: Record<PrivacyPolicyAction, string> = {
  allow: "border-success/30 bg-success/5 text-success",
  redact: "border-warning/40 bg-warning/10 text-warning",
  block: "border-danger/40 bg-danger/10 text-danger",
  require_approval: "border-accent/40 bg-accent/10 text-accent",
};

function errorText(error: unknown): string {
  return errorMessage(error);
}

export function PrivacyFirewallPanel() {
  const { t } = useT();
  const workspaceId = useWorkspaceStore((state) => primaryRoot(state.roots)?.path ?? "global");
  const workspaceLabel = useWorkspaceStore((state) => primaryRoot(state.roots)?.label ?? null);

  const policy = usePrivacyFirewallStore((state) => state.policies[workspaceId]);
  const busy = usePrivacyFirewallStore((state) => state.busy);
  const storeError = usePrivacyFirewallStore((state) => state.error);
  const loadPolicy = usePrivacyFirewallStore((state) => state.loadPolicy);
  const setActionForKind = usePrivacyFirewallStore((state) => state.setActionForKind);
  const setLocalOnlyFallback = usePrivacyFirewallStore((state) => state.setLocalOnlyFallback);
  const addException = usePrivacyFirewallStore((state) => state.addException);
  const removeException = usePrivacyFirewallStore((state) => state.removeException);

  const [loadError, setLoadError] = useState<string | null>(null);
  const [exceptionInput, setExceptionInput] = useState("");

  useEffect(() => {
    setLoadError(null);
    loadPolicy(workspaceId).catch((error) => setLoadError(errorText(error)));
  }, [workspaceId, loadPolicy]);

  const kindLabels: Record<SensitiveDataKind, string> = useMemo(
    () => ({
      private_key: t("PrivacyFirewallPanel.kindPrivateKey"),
      api_credential: t("PrivacyFirewallPanel.kindApiCredential"),
      email: t("PrivacyFirewallPanel.kindEmail"),
      credit_card: t("PrivacyFirewallPanel.kindCreditCard"),
      phone: t("PrivacyFirewallPanel.kindPhone"),
      ip_address: t("PrivacyFirewallPanel.kindIpAddress"),
    }),
    [t],
  );

  const kindDescriptions: Record<SensitiveDataKind, string> = useMemo(
    () => ({
      private_key: t("PrivacyFirewallPanel.kindPrivateKeyDescription"),
      api_credential: t("PrivacyFirewallPanel.kindApiCredentialDescription"),
      email: t("PrivacyFirewallPanel.kindEmailDescription"),
      credit_card: t("PrivacyFirewallPanel.kindCreditCardDescription"),
      phone: t("PrivacyFirewallPanel.kindPhoneDescription"),
      ip_address: t("PrivacyFirewallPanel.kindIpAddressDescription"),
    }),
    [t],
  );

  const actionLabels: Record<PrivacyPolicyAction, string> = useMemo(
    () => ({
      allow: t("PrivacyFirewallPanel.actionAllow"),
      redact: t("PrivacyFirewallPanel.actionRedact"),
      block: t("PrivacyFirewallPanel.actionBlock"),
      require_approval: t("PrivacyFirewallPanel.actionRequireApproval"),
    }),
    [t],
  );

  async function handleAddException() {
    const value = exceptionInput.trim();
    if (value.length === 0) return;
    try {
      await addException(workspaceId, value);
      setExceptionInput("");
    } catch (error) {
      setLoadError(errorText(error));
    }
  }

  return (
    <section className="flex flex-col gap-4" aria-labelledby="privacy-firewall-heading">
      <div className="flex items-start gap-3">
        <span className="rounded-lg border border-accent/30 bg-accent/10 p-2 text-accent">
          <Lock size={20} />
        </span>
        <div>
          <h3 id="privacy-firewall-heading" className="text-sm font-semibold text-foreground">
            {t("PrivacyFirewallPanel.title")}
          </h3>
          <p className="mt-1 text-xs leading-5 text-muted">{t("PrivacyFirewallPanel.description")}</p>
        </div>
      </div>

      <p className="text-[11px] text-faint">
        {t("PrivacyFirewallPanel.workspaceLabel", { workspace: workspaceLabel ?? t("PrivacyFirewallPanel.globalWorkspace") })}
      </p>

      {(loadError ?? storeError) && (
        <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {loadError ?? storeError}
        </p>
      )}

      {!policy ? (
        <p className="text-xs text-muted">{busy ? t("PrivacyFirewallPanel.loading") : t("PrivacyFirewallPanel.notLoaded")}</p>
      ) : (
        <>
          <div className="rounded-lg border border-border bg-surface p-3">
            <label className="flex items-start gap-2 text-xs text-foreground">
              <input
                type="checkbox"
                checked={policy.localOnlyFallback}
                disabled={busy}
                onChange={(event) => {
                  setLocalOnlyFallback(workspaceId, event.target.checked).catch((error) => setLoadError(errorText(error)));
                }}
                className="mt-0.5"
              />
              <span>
                <span className="font-medium">{t("PrivacyFirewallPanel.localOnlyFallbackLabel")}</span>
                <span className="mt-0.5 block leading-4 text-muted">{t("PrivacyFirewallPanel.localOnlyFallbackDescription")}</span>
              </span>
            </label>
          </div>

          <div className="flex flex-col gap-2">
            <h4 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("PrivacyFirewallPanel.kindsHeading")}</h4>
            {SENSITIVE_DATA_KINDS.map((kind) => {
              const action = policy.actions[kind];
              const Icon = ACTION_ICONS[action];
              return (
                <article key={kind} className="rounded-lg border border-border bg-surface p-3">
                  <div className="flex items-start gap-3">
                    <span className={`mt-0.5 inline-flex shrink-0 items-center gap-1 rounded-full border px-2 py-1 text-[10px] font-semibold uppercase tracking-wide ${ACTION_STYLE[action]}`}>
                      <Icon size={13} />
                      {actionLabels[action]}
                    </span>
                    <div className="min-w-0 flex-1">
                      <h5 className="text-xs font-semibold text-foreground">{kindLabels[kind]}</h5>
                      <p className="mt-1 text-xs leading-5 text-muted">{kindDescriptions[kind]}</p>
                      <label className="mt-2 flex items-center gap-2 text-[11px] text-faint">
                        {t("PrivacyFirewallPanel.actionPickerLabel")}
                        <select
                          value={action}
                          disabled={busy}
                          onChange={(event) => {
                            setActionForKind(workspaceId, kind, event.target.value as PrivacyPolicyAction).catch((error) =>
                              setLoadError(errorText(error)),
                            );
                          }}
                          className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
                        >
                          {PRIVACY_POLICY_ACTIONS.map((candidate) => (
                            <option key={candidate} value={candidate}>
                              {actionLabels[candidate]}
                            </option>
                          ))}
                        </select>
                      </label>
                    </div>
                  </div>
                </article>
              );
            })}
          </div>

          <div className="rounded-lg border border-border bg-surface p-3">
            <h4 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("PrivacyFirewallPanel.exceptionsHeading")}</h4>
            <p className="mt-1 text-[11px] leading-4 text-muted">{t("PrivacyFirewallPanel.exceptionsDescription")}</p>
            <div className="mt-2 flex gap-2">
              <input
                type="text"
                value={exceptionInput}
                disabled={busy}
                onChange={(event) => setExceptionInput(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void handleAddException();
                  }
                }}
                placeholder={t("PrivacyFirewallPanel.exceptionsPlaceholder")}
                className="min-w-0 flex-1 rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground"
              />
              <Button variant="secondary" disabled={busy || exceptionInput.trim().length === 0} onClick={() => void handleAddException()}>
                {t("PrivacyFirewallPanel.exceptionsAddButton")}
              </Button>
            </div>
            {policy.exceptions.length === 0 ? (
              <p className="mt-2 text-[11px] text-faint">{t("PrivacyFirewallPanel.exceptionsEmpty")}</p>
            ) : (
              <ul className="mt-2 flex flex-wrap gap-1.5">
                {policy.exceptions.map((value) => (
                  <li
                    key={value}
                    className="flex items-center gap-1 rounded-full border border-border bg-background px-2 py-1 font-mono text-[11px] text-muted"
                  >
                    {value}
                    <button
                      type="button"
                      aria-label={t("PrivacyFirewallPanel.exceptionsRemoveAriaLabel", { value })}
                      disabled={busy}
                      onClick={() => removeException(workspaceId, value).catch((error) => setLoadError(errorText(error)))}
                      className="text-faint hover:text-danger"
                    >
                      <X size={12} />
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <p className="flex items-start gap-2 text-[11px] leading-4 text-faint">
            <ShieldCheck size={14} className="mt-0.5 shrink-0" />
            {t("PrivacyFirewallPanel.scopeNote")}
          </p>
        </>
      )}
    </section>
  );
}

export default PrivacyFirewallPanel;
