import { useMemo, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { AlertTriangle, CheckCircle2, Download, FileJson, RefreshCw, Sparkles, Upload, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { isReleaseReady, type ApiChange } from "../../lib/apiContractDiff";
import { useApiContractDiffStore, type DiffSlot } from "../../store/apiContractDiffStore";
import { Button, IconButton, StatusPill } from "../ui";
import { errorMessage } from "../../lib/errors";

/**
 * API Contract Diff and Mock Lab (ROADMAP.md Phase 7, item 23): a
 * full-screen panel — same toggle pattern as `SopCompilerPanel`/
 * `EvidenceBoardPanel` (see `App.tsx`) — for comparing two local OpenAPI
 * JSON/YAML files and getting a breaking-vs-non-breaking change report, one
 * example mock response per schema, executable generated contract tests, and (for
 * every breaking change) a drafted plain-English client-impact note plus
 * migration suggestion. All state lives in `apiContractDiffStore.ts`; this
 * component is presentation only. MVP scope: OpenAPI JSON/YAML, two local
 * files. GraphQL/protobuf/webhook/event-schema diffing is a follow-up.
 */
interface ApiContractDiffLabPanelProps {
  onClose: () => void;
}

function FileSlotCard({ slot }: { slot: DiffSlot }) {
  const { t } = useT();
  const spec = useApiContractDiffStore((state) => (slot === "old" ? state.oldSpec : state.newSpec));
  const loadingSlot = useApiContractDiffStore((state) => state.loadingSlot);
  const loadFile = useApiContractDiffStore((state) => state.loadFile);
  const busy = loadingSlot === slot;

  return (
    <div className="flex min-w-0 flex-1 flex-col gap-1.5 rounded-md border border-border bg-surface px-3 py-2.5">
      <span className="text-[11px] font-semibold uppercase tracking-wide text-faint">
        {slot === "old" ? t("ApiContractDiffLab.oldVersionLabel") : t("ApiContractDiffLab.newVersionLabel")}
      </span>
      <div className="flex min-w-0 items-center gap-2">
        <FileJson size={14} className="shrink-0 text-faint" />
        <span className="min-w-0 flex-1 truncate text-sm text-foreground">
          {spec ? spec.fileName : t("ApiContractDiffLab.noFileLoaded")}
        </span>
      </div>
      {spec && (
        <span className="truncate text-xs text-muted">
          {t("ApiContractDiffLab.specSummary", { title: spec.doc.title, version: spec.doc.version, count: spec.doc.operations.length })}
        </span>
      )}
      <Button size="sm" variant="secondary" onClick={() => void loadFile(slot)} disabled={busy}>
        <Upload size={12} />
        {busy ? t("ApiContractDiffLab.loading") : t("ApiContractDiffLab.chooseFile")}
      </Button>
    </div>
  );
}

function ChangeRow({ change }: { change: ApiChange }) {
  const { t } = useT();
  const impactNotes = useApiContractDiffStore((state) => state.impactNotes);
  const note = impactNotes.find((entry) => entry.changeId === change.id);
  return (
    <li className="rounded-md border border-border bg-surface px-3 py-2">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <span className="min-w-0 text-sm font-medium text-foreground">{change.operationLabel}</span>
        <StatusPill tone={change.severity === "breaking" ? "danger" : "success"}>
          {t(`ApiContractDiffLab.kind.${change.kind}`)}
        </StatusPill>
      </div>
      <p className="mt-1 text-xs text-muted">{change.detail}</p>
      {note && (
        <div className="mt-2 space-y-1 rounded bg-danger-soft px-2 py-1.5 text-xs text-danger">
          <p>
            <span className="font-semibold">{t("ApiContractDiffLab.impactLabel")}:</span> {note.impact}
          </p>
          <p>
            <span className="font-semibold">{t("ApiContractDiffLab.migrationLabel")}:</span> {note.migration}
          </p>
        </div>
      )}
    </li>
  );
}

export function ApiContractDiffLabPanel({ onClose }: ApiContractDiffLabPanelProps) {
  const { t } = useT();
  const oldSpec = useApiContractDiffStore((state) => state.oldSpec);
  const newSpec = useApiContractDiffStore((state) => state.newSpec);
  const loadError = useApiContractDiffStore((state) => state.loadError);
  const changes = useApiContractDiffStore((state) => state.changes);
  const mocks = useApiContractDiffStore((state) => state.mocks);
  const testStub = useApiContractDiffStore((state) => state.testStub);
  const contractTests = useApiContractDiffStore((state) => state.contractTests);
  const hasRun = useApiContractDiffStore((state) => state.hasRun);
  const diffError = useApiContractDiffStore((state) => state.diffError);
  const drafting = useApiContractDiffStore((state) => state.drafting);
  const draftError = useApiContractDiffStore((state) => state.draftError);
  const runDiff = useApiContractDiffStore((state) => state.runDiff);
  const draftImpactNotes = useApiContractDiffStore((state) => state.draftImpactNotes);
  const reset = useApiContractDiffStore((state) => state.reset);

  const [saveError, setSaveError] = useState<string | null>(null);
  const [saveBusy, setSaveBusy] = useState(false);

  const breaking = useMemo(() => changes.filter((change) => change.severity === "breaking"), [changes]);
  const nonBreaking = useMemo(() => changes.filter((change) => change.severity === "non-breaking"), [changes]);
  const releaseReady = isReleaseReady(changes, contractTests);

  const handleSaveTestStub = async () => {
    setSaveError(null);
    setSaveBusy(true);
    try {
      const destination = await save({
        defaultPath: "api-contract.test.ts",
        filters: [{ name: "TypeScript", extensions: ["ts"] }],
      });
      if (!destination) return;
      await writeTextFile(destination, testStub);
    } catch (err) {
      setSaveError(errorMessage(err));
    } finally {
      setSaveBusy(false);
    }
  };

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="api-contract-diff-lab-title">
      <header className="flex shrink-0 flex-col gap-2 border-b border-border px-4 py-3">
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <h1 id="api-contract-diff-lab-title" className="text-base font-semibold text-foreground">
              {t("ApiContractDiffLab.title")}
            </h1>
            <p className="truncate text-xs text-muted">{t("ApiContractDiffLab.subtitle")}</p>
          </div>
          <IconButton size="sm" onClick={onClose} aria-label={t("ApiContractDiffLab.close")}>
            <X size={16} />
          </IconButton>
        </div>

        <div className="flex flex-wrap gap-2">
          <FileSlotCard slot="old" />
          <FileSlotCard slot="new" />
        </div>

        {loadError && <p className="text-xs text-danger">{loadError}</p>}

        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="primary" onClick={runDiff} disabled={!oldSpec || !newSpec}>
            <RefreshCw size={13} />
            {t("ApiContractDiffLab.runDiff")}
          </Button>
          {hasRun && (
            <Button size="sm" variant="ghost" onClick={reset}>
              {t("ApiContractDiffLab.startOver")}
            </Button>
          )}
        </div>
      </header>

      {diffError && (
        <div role="alert" className="border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          {diffError}
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3 [overscroll-behavior:contain]">
        {!hasRun ? (
          <div className="flex h-full flex-col items-center justify-center gap-1 text-center">
            <p className="text-sm font-medium text-foreground">{t("ApiContractDiffLab.noReportYet")}</p>
            <p className="max-w-sm text-xs text-muted">{t("ApiContractDiffLab.noReportYetHint")}</p>
          </div>
        ) : (
          <div className="space-y-4">
            <div
              role="status"
              className={`flex flex-wrap items-center gap-2 rounded-md border px-3 py-2.5 text-sm font-medium ${
                releaseReady ? "border-success/30 bg-success-soft text-success" : "border-danger/30 bg-danger-soft text-danger"
              }`}
            >
              {releaseReady ? <CheckCircle2 size={16} /> : <AlertTriangle size={16} />}
              {releaseReady
                ? t("ApiContractDiffLab.verdictReady")
                : breaking.length > 0
                  ? t("ApiContractDiffLab.verdictNotReady", { count: breaking.length })
                  : "Not release ready: generated contract tests did not produce a clean executable report."}
            </div>

            {contractTests && (
              <section className="rounded-md border border-border bg-surface px-3 py-2.5">
                <h2 className="text-sm font-semibold text-foreground">Executable contract report</h2>
                <p className={`mt-1 text-xs ${contractTests.clean ? "text-success" : "text-danger"}`}>
                  {contractTests.passCount}/{contractTests.results.length} generated request/response cases passed.
                  {contractTests.results.length === 0 ? " No schema-backed cases were generated, so this is not release evidence." : ""}
                </p>
                {contractTests.failCount > 0 && (
                  <ul className="mt-2 list-disc space-y-1 pl-5 text-xs text-danger">
                    {contractTests.results.filter((result) => !result.passed).map((result) => (
                      <li key={result.id}>{result.label}: {result.errors.join(" ")}</li>
                    ))}
                  </ul>
                )}
              </section>
            )}

            {breaking.length > 0 && (
              <section>
                <div className="mb-2 flex items-center justify-between gap-2">
                  <h2 className="text-sm font-semibold text-foreground">
                    {t("ApiContractDiffLab.breakingHeading", { count: breaking.length })}
                  </h2>
                  <Button size="sm" variant="secondary" onClick={() => void draftImpactNotes()} disabled={drafting}>
                    <Sparkles size={12} className={drafting ? "animate-pulse" : ""} />
                    {drafting ? t("ApiContractDiffLab.drafting") : t("ApiContractDiffLab.draftImpactNotes")}
                  </Button>
                </div>
                {draftError && <p className="mb-2 text-xs text-danger">{draftError}</p>}
                <ul className="space-y-2">
                  {breaking.map((change) => (
                    <ChangeRow key={change.id} change={change} />
                  ))}
                </ul>
              </section>
            )}

            {nonBreaking.length > 0 && (
              <section>
                <h2 className="mb-2 text-sm font-semibold text-foreground">
                  {t("ApiContractDiffLab.nonBreakingHeading", { count: nonBreaking.length })}
                </h2>
                <ul className="space-y-2">
                  {nonBreaking.map((change) => (
                    <ChangeRow key={change.id} change={change} />
                  ))}
                </ul>
              </section>
            )}

            {changes.length === 0 && (
              <p className="text-sm text-muted">{t("ApiContractDiffLab.noChanges")}</p>
            )}

            {mocks.length > 0 && (
              <section>
                <h2 className="mb-2 text-sm font-semibold text-foreground">{t("ApiContractDiffLab.mocksHeading")}</h2>
                <ul className="space-y-2">
                  {mocks.map((mock, index) => (
                    <li key={`${mock.operationLabel}-${mock.status}-${index}`} className="rounded-md border border-border bg-surface px-3 py-2">
                      <p className="mb-1 text-xs font-medium text-foreground">
                        {mock.operationLabel} · {t("ApiContractDiffLab.statusLabel", { status: mock.status })}
                      </p>
                      <pre className="max-h-48 overflow-auto rounded bg-background p-2 text-xs text-muted">
                        {JSON.stringify(mock.example, null, 2)}
                      </pre>
                    </li>
                  ))}
                </ul>
              </section>
            )}

            {testStub && (
              <section>
                <div className="mb-2 flex items-center justify-between gap-2">
                  <h2 className="text-sm font-semibold text-foreground">{t("ApiContractDiffLab.testStubHeading")}</h2>
                  <Button size="sm" variant="secondary" onClick={() => void handleSaveTestStub()} disabled={saveBusy}>
                    <Download size={12} />
                    {t("ApiContractDiffLab.saveTestStub")}
                  </Button>
                </div>
                {saveError && <p className="mb-2 text-xs text-danger">{saveError}</p>}
                <pre className="max-h-72 overflow-auto rounded-md border border-border bg-surface p-3 text-xs text-muted">
                  {testStub}
                </pre>
              </section>
            )}
          </div>
        )}
      </div>
    </section>
  );
}

export default ApiContractDiffLabPanel;
