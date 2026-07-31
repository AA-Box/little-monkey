import { useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  FileUp,
  FlaskConical,
  Loader2,
  Play,
  Plug,
  RotateCcw,
  X,
  XCircle,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { useConnectorBuilderStore } from "../../store/connectorBuilderStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";

interface ConnectorBuilderPanelProps {
  onClose: () => void;
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

function statusPill(ready: boolean, bridgeBlocked: boolean, t: (key: string) => string) {
  const tone: PillTone = ready ? "success" : "warning";
  const label = ready
    ? t("ConnectorBuilder.statusReady")
    : bridgeBlocked
      ? "Bridge required"
      : t("ConnectorBuilder.statusNotSimulated");
  return <StatusPill tone={tone}>{label}</StatusPill>;
}

const RISK_TONE: Record<string, PillTone> = { low: "success", medium: "warning", high: "danger" };

export function ConnectorBuilderPanel({ onClose }: ConnectorBuilderPanelProps) {
  const { t } = useT();
  const store = useConnectorBuilderStore();
  const [localError, setLocalError] = useState<string | null>(null);

  const definition = store.definition;
  const simulated = store.simulation !== null;

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="connector-builder-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="connector-builder-title" className="text-sm font-semibold text-foreground">
            {t("ConnectorBuilder.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("ConnectorBuilder.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("ConnectorBuilder.close")} title={t("ConnectorBuilder.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      {(store.error || localError) && (
        <div role="alert" className="mx-5 mt-3 whitespace-pre-wrap rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {store.error ?? localError}
        </div>
      )}

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(20rem,1fr)_minmax(0,1.3fr)]">
        {/* Left: spec input */}
        <div className="flex min-h-0 flex-col gap-3 overflow-y-auto">
          <div className="rounded-lg border border-border bg-surface p-3">
            <div className="flex items-center justify-between">
              <h3 className="text-xs font-semibold text-foreground">{t("ConnectorBuilder.specHeading")}</h3>
              <Button size="sm" disabled={store.importing} onClick={() => void store.importFromFile()}>
                {store.importing ? <Loader2 className="animate-spin" size={13} /> : <FileUp size={13} />}
                {t("ConnectorBuilder.importButton")}
              </Button>
            </div>
            {store.specFileName && (
              <p className="mt-1.5 truncate text-[11px] text-muted">{t("ConnectorBuilder.loadedFile", { name: store.specFileName })}</p>
            )}
            <textarea
              className="mt-2 h-40 w-full resize-none rounded-md border border-border bg-background px-2.5 py-1.5 font-mono text-[11px] leading-5 text-foreground outline-none focus:border-accent"
              placeholder={t("ConnectorBuilder.specPlaceholder")}
              value={store.specText}
              onChange={(event) => store.setSpecText(event.target.value)}
            />
            <div className="mt-3 flex justify-end gap-2">
              <Button size="sm" onClick={() => store.reset()}>
                <RotateCcw size={13} /> {t("ConnectorBuilder.resetButton")}
              </Button>
              <Button
                size="sm"
                variant="primary"
                disabled={store.generating || !store.specText.trim()}
                onClick={() => {
                  setLocalError(null);
                  void store.generate().catch((error) => setLocalError(errorText(error)));
                }}
              >
                {store.generating ? <Loader2 className="animate-spin" size={13} /> : <Play size={13} />}
                {t("ConnectorBuilder.generateButton")}
              </Button>
            </div>
          </div>

          {definition && (
            <div className="rounded-lg border border-border bg-surface p-3">
              <div className="flex items-center justify-between gap-2">
                <h3 className="truncate text-xs font-semibold text-foreground">{definition.server.name}</h3>
                {statusPill(store.ready, Boolean(store.availabilityBlockReason), t)}
              </div>
              <p className="mt-1 text-[11px] text-muted">{definition.server.description}</p>

              {store.drafting && (
                <p className="mt-2 flex items-center gap-1.5 text-[11px] text-faint">
                  <Loader2 className="animate-spin" size={12} /> {t("ConnectorBuilder.draftingSummary")}
                </p>
              )}
              {store.summary && <p className="mt-2 rounded-md border border-border bg-background p-2 text-[11px] leading-5 text-foreground">{store.summary}</p>}

              <dl className="mt-3 grid grid-cols-2 gap-x-2 gap-y-1.5 text-[11px]">
                <dt className="text-faint">{t("ConnectorBuilder.authLabel")}</dt>
                <dd className="text-foreground">{t(`ConnectorBuilder.authType.${definition.auth.type}`)}</dd>
                <dt className="text-faint">{t("ConnectorBuilder.rateLimitLabel")}</dt>
                <dd className="text-foreground">
                  {t("ConnectorBuilder.rateLimitValue", { count: definition.rateLimit.requestsPerMinute })}
                </dd>
                <dt className="text-faint">{t("ConnectorBuilder.toolCountLabel")}</dt>
                <dd className="text-foreground">{definition.server.tools.length}</dd>
              </dl>
              <p className="mt-1.5 text-[11px] text-faint">{definition.auth.instructions}</p>
              <p className="mt-1 text-[11px] text-faint">{definition.rateLimit.note}</p>
            </div>
          )}
        </div>

        {/* Right: generated tools + simulator + register */}
        <div className="flex min-h-0 flex-col overflow-hidden rounded-lg border border-border bg-surface">
          {!definition ? (
            <p className="p-8 text-center text-xs text-faint">{t("ConnectorBuilder.noSelectionHint")}</p>
          ) : (
            <div className="flex min-h-0 flex-1 flex-col overflow-y-auto p-4">
              <div className="flex flex-wrap gap-2">
                <Button size="sm" disabled={store.simulating} onClick={() => store.runSimulator()}>
                  {store.simulating ? <Loader2 className="animate-spin" size={13} /> : <FlaskConical size={13} />}
                  {t("ConnectorBuilder.runSimulatorButton")}
                </Button>
                <Button
                  size="sm"
                  variant="primary"
                  disabled={!store.ready || store.registering}
                  title={store.ready ? undefined : store.availabilityBlockReason ?? t("ConnectorBuilder.registerBlockedHint")}
                  onClick={() => {
                    setLocalError(null);
                    void store.registerWithMcp().catch((error) => setLocalError(errorText(error)));
                  }}
                >
                  {store.registering ? <Loader2 className="animate-spin" size={13} /> : <Plug size={13} />}
                  {t("ConnectorBuilder.registerButton")}
                </Button>
              </div>

              {store.registeredServerId && (
                <p className="mt-2 rounded-md border border-success/40 bg-success-soft p-2 text-[11px] text-success">
                  {t("ConnectorBuilder.registeredAs", { id: store.registeredServerId })}
                </p>
              )}

              {store.availabilityBlockReason && (
                <p className="mt-2 flex items-start gap-1.5 rounded-md border border-warning/40 bg-warning/5 p-2.5 text-[11px] text-warning">
                  <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                  {store.availabilityBlockReason}
                </p>
              )}
              {!simulated && !store.availabilityBlockReason && (
                <p className="mt-2 flex items-start gap-1.5 rounded-md border border-warning/40 bg-warning/5 p-2.5 text-[11px] text-warning">
                  <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                  {t("ConnectorBuilder.notSimulatedYetWarning")}
                </p>
              )}

              <div className="mt-3">
                <h4 className="text-xs font-semibold text-foreground">{t("ConnectorBuilder.toolsHeading")}</h4>
                <div className="mt-2 overflow-x-auto rounded-md border border-border">
                  <table className="w-full min-w-[32rem] border-collapse text-left text-[11px]">
                    <thead className="bg-surface-2 text-faint">
                      <tr>
                        <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableTool")}</th>
                        <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableMethod")}</th>
                        <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableRisk")}</th>
                        <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableParams")}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {definition.server.tools.map((tool) => {
                        const permission = definition.permissions.find((p) => p.toolName === tool.name);
                        return (
                          <tr key={tool.name} className="border-t border-border align-top">
                            <td className="px-2 py-1.5">
                              <p className="font-mono text-foreground">{tool.name}</p>
                              <p className="mt-0.5 text-faint">{tool.description}</p>
                            </td>
                            <td className="px-2 py-1.5 font-mono text-muted">{permission?.method}</td>
                            <td className="px-2 py-1.5">
                              {permission && (
                                <StatusPill tone={RISK_TONE[permission.risk]}>{t(`ConnectorBuilder.risk.${permission.risk}`)}</StatusPill>
                              )}
                            </td>
                            <td className="px-2 py-1.5 text-muted">
                              {tool.params.length === 0
                                ? t("ConnectorBuilder.noParams")
                                : tool.params.map((param) => `${param.name}${param.required ? "" : "?"}`).join(", ")}
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              </div>

              {store.simulation && (
                <div className="mt-3">
                  <h4 className="text-xs font-semibold text-foreground">
                    {t("ConnectorBuilder.simulationHeading", {
                      passed: store.simulation.passCount,
                      total: store.simulation.results.length,
                    })}
                  </h4>
                  <div className="mt-2 overflow-x-auto rounded-md border border-border">
                    <table className="w-full min-w-[36rem] border-collapse text-left text-[11px]">
                      <thead className="bg-surface-2 text-faint">
                        <tr>
                          <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableTool")}</th>
                          <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableFixture")}</th>
                          <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableOutcome")}</th>
                          <th className="px-2 py-1.5 font-medium">{t("ConnectorBuilder.tableReason")}</th>
                        </tr>
                      </thead>
                      <tbody>
                        {store.simulation.results.map((result) => (
                          <tr key={result.fixture.id} className="border-t border-border">
                            <td className="px-2 py-1.5 font-mono text-foreground">{result.fixture.toolName}</td>
                            <td className="px-2 py-1.5 text-muted">{result.fixture.label}</td>
                            <td className="px-2 py-1.5">
                              {result.outcome === "pass" ? (
                                <span className="inline-flex items-center gap-1 text-success">
                                  <CheckCircle2 size={12} /> {t("ConnectorBuilder.pass")}
                                </span>
                              ) : (
                                <span className="inline-flex items-center gap-1 text-danger">
                                  <XCircle size={12} /> {t("ConnectorBuilder.fail")}
                                </span>
                              )}
                              {result.injectionDetected && (
                                <span className="ml-1.5 inline-flex items-center gap-1 text-warning" title={t("ConnectorBuilder.injectionDetectedHint")}>
                                  <AlertTriangle size={11} />
                                </span>
                              )}
                            </td>
                            <td className="px-2 py-1.5 text-faint">{result.reason}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export default ConnectorBuilderPanel;
