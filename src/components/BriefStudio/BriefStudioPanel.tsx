import { useMemo } from "react";
import { CheckCircle2, Loader2, ShieldCheck, X, XCircle } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useBriefStudioStore } from "../../store/briefStudioStore";
import { useSessionStore } from "../../store/sessionStore";
import { useStackStore } from "../../store/stackStore";
import { Button, IconButton } from "../ui";
import {
  TEXT_ASSET_TYPES,
  UNSUPPORTED_ASSET_TYPES,
  type BriefAssetType,
  type BriefSourceKind,
} from "../../lib/briefStudio";

interface BriefStudioPanelProps {
  onClose: () => void;
}

const SOURCE_KINDS: BriefSourceKind[] = ["pasted", "session", "knowledge_stack"];
const ALL_ASSET_TYPES: BriefAssetType[] = [...TEXT_ASSET_TYPES, ...UNSUPPORTED_ASSET_TYPES];

/**
 * Source-Grounded Brief Studio (ROADMAP.md Phase 7, item 7): pick a source
 * (pasted text, a chat session, or a knowledge stack query), pick an asset
 * type, and generate a citation-carrying study asset. Full-screen panel,
 * same toggle pattern as Run Center / Browser Workbench / Issue-to-PR.
 */
export function BriefStudioPanel({ onClose }: BriefStudioPanelProps) {
  const { t } = useT();
  const store = useBriefStudioStore();
  const sessions = useSessionStore((state) => state.sessions);
  const stacks = useStackStore((state) => state.stacks);

  const nonArchivedSessions = useMemo(
    () => sessions.filter((session) => !session.archived).sort((a, b) => b.updatedAt - a.updatedAt),
    [sessions],
  );

  const canGenerate =
    !store.generating &&
    ((store.sourceKind === "pasted" && store.pastedText.trim().length > 0) ||
      (store.sourceKind === "session" && store.selectedSessionId !== null) ||
      (store.sourceKind === "knowledge_stack" && store.selectedStackId !== null && store.focusQuery.trim().length > 0));

  const result = store.result;

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="brief-studio-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="brief-studio-title" className="text-sm font-semibold text-foreground">
            {t("BriefStudio.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("BriefStudio.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("BriefStudio.close")} title={t("BriefStudio.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(18rem,1fr)_minmax(0,1.4fr)]">
        {/* Left column: source picker + asset type + generate */}
        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          <h3 className="text-xs font-semibold text-foreground">{t("BriefStudio.sourceHeading")}</h3>
          <div className="mt-2 flex flex-wrap gap-1.5">
            {SOURCE_KINDS.map((kind) => (
              <button
                key={kind}
                type="button"
                onClick={() => store.setSourceKind(kind)}
                className={`rounded-full border px-3 py-1 text-xs font-medium transition-colors ${
                  store.sourceKind === kind
                    ? "border-accent bg-accent/10 text-foreground"
                    : "border-border bg-background text-muted hover:border-border-strong"
                }`}
              >
                {t(`BriefStudio.sourceKind.${kind}`)}
              </button>
            ))}
          </div>

          {store.sourceKind === "pasted" && (
            <div className="mt-3 flex flex-col gap-2">
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-muted">{t("BriefStudio.pastedLabelLabel")}</span>
                <input
                  type="text"
                  value={store.pastedLabel}
                  onChange={(event) => store.setPastedLabel(event.target.value)}
                  placeholder={t("BriefStudio.pastedLabelPlaceholder")}
                  className="rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-muted">{t("BriefStudio.pastedTextLabel")}</span>
                <textarea
                  value={store.pastedText}
                  onChange={(event) => store.setPastedText(event.target.value)}
                  rows={10}
                  placeholder={t("BriefStudio.pastedTextPlaceholder")}
                  className="resize-none rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                />
              </label>
            </div>
          )}

          {store.sourceKind === "session" && (
            <div className="mt-3 flex flex-col gap-2">
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-muted">{t("BriefStudio.sessionLabel")}</span>
                {nonArchivedSessions.length === 0 ? (
                  <p className="text-xs text-faint">{t("BriefStudio.noSessionsNote")}</p>
                ) : (
                  <select
                    value={store.selectedSessionId ?? ""}
                    onChange={(event) => store.setSelectedSessionId(event.target.value || null)}
                    className="rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                  >
                    <option value="">{t("BriefStudio.sessionPlaceholder")}</option>
                    {nonArchivedSessions.map((session) => (
                      <option key={session.id} value={session.id}>
                        {session.title}
                      </option>
                    ))}
                  </select>
                )}
              </label>
            </div>
          )}

          {store.sourceKind === "knowledge_stack" && (
            <div className="mt-3 flex flex-col gap-2">
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-muted">{t("BriefStudio.stackLabel")}</span>
                {stacks.length === 0 ? (
                  <p className="text-xs text-faint">{t("BriefStudio.noStacksNote")}</p>
                ) : (
                  <select
                    value={store.selectedStackId ?? ""}
                    onChange={(event) => store.setSelectedStackId(event.target.value || null)}
                    className="rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                  >
                    <option value="">{t("BriefStudio.stackPlaceholder")}</option>
                    {stacks.map((stack) => (
                      <option key={stack.id} value={stack.id}>
                        {stack.name}
                      </option>
                    ))}
                  </select>
                )}
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-muted">{t("BriefStudio.focusQueryLabel")}</span>
                <input
                  type="text"
                  value={store.focusQuery}
                  onChange={(event) => store.setFocusQuery(event.target.value)}
                  placeholder={t("BriefStudio.focusQueryPlaceholder")}
                  className="rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                />
              </label>
            </div>
          )}

          <label className="mt-3 flex cursor-pointer items-start gap-2 rounded-md border border-border bg-background p-2.5">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={store.requireLocalOnly}
              onChange={(event) => store.setRequireLocalOnly(event.target.checked)}
            />
            <span>
              <span className="flex items-center gap-1.5 text-xs font-medium text-foreground">
                <ShieldCheck size={13} className="shrink-0 text-accent" />
                {t("BriefStudio.localOnlyLabel")}
              </span>
              <span className="mt-0.5 block text-[11px] leading-4 text-faint">{t("BriefStudio.localOnlyHint")}</span>
            </span>
          </label>

          <h3 className="mt-4 text-xs font-semibold text-foreground">{t("BriefStudio.assetHeading")}</h3>
          <div className="mt-2 flex flex-wrap gap-1.5">
            {ALL_ASSET_TYPES.map((assetType) => {
              const unsupported = (UNSUPPORTED_ASSET_TYPES as readonly BriefAssetType[]).includes(assetType);
              return (
                <button
                  key={assetType}
                  type="button"
                  onClick={() => store.setAssetType(assetType)}
                  className={`rounded-full border px-3 py-1 text-xs font-medium transition-colors ${
                    store.assetType === assetType
                      ? "border-accent bg-accent/10 text-foreground"
                      : "border-border bg-background text-muted hover:border-border-strong"
                  } ${unsupported ? "opacity-70" : ""}`}
                >
                  {t(`BriefStudio.asset.${assetType}`)}
                </button>
              );
            })}
          </div>
          {(UNSUPPORTED_ASSET_TYPES as readonly BriefAssetType[]).includes(store.assetType) && (
            <p className="mt-2 rounded-md border border-dashed border-border p-2.5 text-[11px] leading-4 text-faint">
              {t("BriefStudio.unsupportedNote")}
            </p>
          )}

          <Button
            variant="primary"
            size="sm"
            className="mt-4 w-full"
            disabled={!canGenerate}
            onClick={() => void store.generate()}
          >
            {store.generating ? <Loader2 className="animate-spin" size={14} /> : null}
            {store.generating ? t("BriefStudio.generatingLabel") : t("BriefStudio.generateButton")}
          </Button>
        </div>

        {/* Right column: generated result */}
        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          {store.error && (
            <div role="alert" className="mb-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
              <p className="font-medium">{t("BriefStudio.errorHeading")}</p>
              <p className="mt-1 whitespace-pre-wrap break-words">{store.error}</p>
            </div>
          )}

          {!result ? (
            <p className="p-8 text-center text-xs text-faint">{t("BriefStudio.emptyState")}</p>
          ) : !result.supported ? (
            <p className="rounded-md border border-dashed border-border p-4 text-xs leading-5 text-faint">
              {t("BriefStudio.unsupportedNote")}
            </p>
          ) : (
            <div className="space-y-4">
              <div className="flex flex-wrap items-center gap-2 text-[11px] text-faint">
                <span>{t("BriefStudio.groundedIn", { label: result.sourceLabel })}</span>
                <span>·</span>
                <span>{t("BriefStudio.generatedAt", { time: new Date(result.generatedAtMs).toLocaleString() })}</span>
                <span>·</span>
                <span className="inline-flex items-center gap-1 font-medium text-foreground">
                  {result.ranLocally ? (
                    <>
                      <ShieldCheck size={12} className="text-success" /> {t("BriefStudio.ranLocallyBadge")}
                    </>
                  ) : (
                    t("BriefStudio.ranCloudBadge")
                  )}
                </span>
              </div>

              <div className="rounded-md border border-border bg-background p-3 text-sm leading-6 text-foreground">
                <pre className="whitespace-pre-wrap break-words font-sans text-sm">{result.content}</pre>
              </div>

              <div>
                <div className="flex items-center justify-between gap-2">
                  <h4 className="text-xs font-semibold text-foreground">{t("BriefStudio.citationsHeading")}</h4>
                </div>
                {result.unverifiedCitationCount > 0 && (
                  <p className="mt-1.5 rounded-md border border-warning/40 bg-warning/10 p-2 text-[11px] text-warning">
                    {t("BriefStudio.unverifiedWarning", { count: result.unverifiedCitationCount })}
                  </p>
                )}
                {result.citations.length === 0 ? (
                  <p className="mt-2 text-[11px] text-faint">{t("BriefStudio.citationsEmpty")}</p>
                ) : (
                  <ul className="mt-2 space-y-1.5">
                    {result.citations.map((citation, index) => (
                      <li
                        key={`${citation.refId}-${index}`}
                        className="flex items-start gap-2 rounded-md border border-border bg-background p-2 text-[11px]"
                      >
                        {citation.verified ? (
                          <CheckCircle2 size={13} className="mt-0.5 shrink-0 text-success" />
                        ) : (
                          <XCircle size={13} className="mt-0.5 shrink-0 text-danger" />
                        )}
                        <div className="min-w-0">
                          <p className="font-mono text-faint">
                            {citation.refId}
                            {citation.sourceLabel ? ` · ${citation.sourceLabel}` : ""}
                          </p>
                          <p className="mt-0.5 break-words text-foreground">"{citation.quote}"</p>
                          <p className={citation.verified ? "mt-0.5 text-success" : "mt-0.5 text-danger"}>
                            {citation.verified ? t("BriefStudio.citationVerified") : t("BriefStudio.citationUnverified")}
                          </p>
                        </div>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export default BriefStudioPanel;
