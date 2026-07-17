import { useEffect, useMemo, useState } from "react";
import { ChevronDown, ChevronRight, MessageSquareText, Plus, RefreshCw, Trash2, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import type { Claim, ClaimConfidence, ClaimStatus } from "../../lib/evidenceBoard";
import { useEvidenceBoardStore } from "../../store/evidenceBoardStore";
import { useSessionStore } from "../../store/sessionStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

/**
 * Evidence Board and Claim Checker (ROADMAP.md Phase 7, item 6): a
 * full-screen panel — same toggle pattern as `RunCenter`/`BrowserWorkbench`/
 * `AgentInbox` (see `App.tsx`) — that lets the user audit a report claim by
 * claim instead of trusting one generated summary wholesale. All state
 * lives in `evidenceBoardStore.ts`; this component is presentation plus the
 * small "which board is on screen / is the compose form open" UI state.
 */
interface EvidenceBoardPanelProps {
  sessionId: string | null;
  onClose: () => void;
}

const CONFIDENCE_TONE: Record<ClaimConfidence, PillTone> = {
  high: "success",
  medium: "warning",
  low: "danger",
};

const STATUS_OPTIONS: ClaimStatus[] = ["open", "confirmed", "disputed", "resolved"];

function EvidenceSpanList({ spans, tone, emptyLabel }: { spans: string[]; tone: "success" | "danger"; emptyLabel: string }) {
  if (spans.length === 0) return <p className="text-muted">{emptyLabel}</p>;
  const rowClass = tone === "success" ? "bg-success-soft text-success" : "bg-danger-soft text-danger";
  return (
    <ul className="space-y-1">
      {spans.map((quote, index) => (
        <li key={index} className={`rounded px-2 py-1 ${rowClass}`}>
          &ldquo;{quote}&rdquo;
        </li>
      ))}
    </ul>
  );
}

function ClaimCard({ boardId, claim }: { boardId: string; claim: Claim }) {
  const { t } = useT();
  const [expanded, setExpanded] = useState(false);
  const updateClaimOwner = useEvidenceBoardStore((state) => state.updateClaimOwner);
  const updateClaimStatus = useEvidenceBoardStore((state) => state.updateClaimStatus);
  const deleteClaim = useEvidenceBoardStore((state) => state.deleteClaim);

  return (
    <li className="rounded-md border border-border bg-surface">
      <button
        type="button"
        onClick={() => setExpanded((value) => !value)}
        aria-expanded={expanded}
        className="flex w-full items-start justify-between gap-3 px-3 py-2.5 text-left"
      >
        <div className="flex min-w-0 items-start gap-2">
          {expanded ? (
            <ChevronDown size={14} className="mt-0.5 shrink-0 text-faint" />
          ) : (
            <ChevronRight size={14} className="mt-0.5 shrink-0 text-faint" />
          )}
          <span className="min-w-0 text-sm text-foreground">{claim.text}</span>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {claim.unresolved && <StatusPill tone="warning">{t("EvidenceBoard.unresolvedBadge")}</StatusPill>}
          <StatusPill tone={CONFIDENCE_TONE[claim.confidence]}>{t(`EvidenceBoard.confidence.${claim.confidence}`)}</StatusPill>
        </div>
      </button>

      {expanded && (
        <div className="space-y-3 border-t border-border px-3 py-3 text-xs">
          <div>
            <p className="mb-1 font-semibold uppercase tracking-wide text-faint">{t("EvidenceBoard.supportingEvidence")}</p>
            <EvidenceSpanList spans={claim.supportingEvidence} tone="success" emptyLabel={t("EvidenceBoard.noSupportingEvidence")} />
          </div>
          <div>
            <p className="mb-1 font-semibold uppercase tracking-wide text-faint">{t("EvidenceBoard.conflictingEvidence")}</p>
            <EvidenceSpanList spans={claim.conflictingEvidence} tone="danger" emptyLabel={t("EvidenceBoard.noConflictingEvidence")} />
          </div>
          {claim.unresolvedQuestion && (
            <p className="rounded bg-warning-soft px-2 py-1.5 text-warning">
              <span className="font-semibold">{t("EvidenceBoard.unresolvedQuestionLabel")}:</span> {claim.unresolvedQuestion}
            </p>
          )}
          <div className="flex flex-wrap items-center gap-3 pt-1">
            <label className="flex items-center gap-1.5">
              <span className="text-faint">{t("EvidenceBoard.ownerLabel")}</span>
              <input
                type="text"
                value={claim.owner}
                onChange={(event) => updateClaimOwner(boardId, claim.id, event.target.value)}
                placeholder={t("EvidenceBoard.ownerPlaceholder")}
                className="w-32 rounded border border-border bg-background px-1.5 py-1 text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
              />
            </label>
            <label className="flex items-center gap-1.5">
              <span className="text-faint">{t("EvidenceBoard.statusLabel")}</span>
              <select
                value={claim.status}
                onChange={(event) => updateClaimStatus(boardId, claim.id, event.target.value as ClaimStatus)}
                className="rounded border border-border bg-background px-1.5 py-1 text-foreground"
              >
                {STATUS_OPTIONS.map((status) => (
                  <option key={status} value={status}>
                    {t(`EvidenceBoard.status.${status}`)}
                  </option>
                ))}
              </select>
            </label>
            <button
              type="button"
              onClick={() => deleteClaim(boardId, claim.id)}
              aria-label={t("EvidenceBoard.deleteClaim")}
              className="ml-auto text-faint hover:text-danger"
            >
              <Trash2 size={13} />
            </button>
          </div>
        </div>
      )}
    </li>
  );
}

export function EvidenceBoardPanel({ sessionId, onClose }: EvidenceBoardPanelProps) {
  const { t } = useT();
  const boards = useEvidenceBoardStore((state) => state.boards);
  const activeBoardId = useEvidenceBoardStore((state) => state.activeBoardId);
  const extracting = useEvidenceBoardStore((state) => state.extracting);
  const setActiveBoard = useEvidenceBoardStore((state) => state.setActiveBoard);
  const openSessionBoard = useEvidenceBoardStore((state) => state.openSessionBoard);
  const createPastedBoard = useEvidenceBoardStore((state) => state.createPastedBoard);
  const deleteBoard = useEvidenceBoardStore((state) => state.deleteBoard);
  const runExtraction = useEvidenceBoardStore((state) => state.runExtraction);

  const sessionTitle = useSessionStore((state) => state.sessions.find((session) => session.id === sessionId)?.title ?? null);

  const [composeOpen, setComposeOpen] = useState(false);
  const [pastedName, setPastedName] = useState("");
  const [pastedText, setPastedText] = useState("");
  const [runError, setRunError] = useState<string | null>(null);

  const activeBoard = useMemo(() => boards.find((board) => board.id === activeBoardId) ?? null, [boards, activeBoardId]);

  // Default to whichever board is already tracking the currently active
  // chat session, if one exists — a fresh open of the panel should never
  // land on an empty state when there's obviously relevant work already here.
  useEffect(() => {
    if (activeBoardId) return;
    if (!sessionId) return;
    const existing = boards.find((board) => board.sourceKind === "session" && board.sourceSessionId === sessionId);
    if (existing) setActiveBoard(existing.id);
  }, [activeBoardId, boards, sessionId, setActiveBoard]);

  const handleUseThisChat = () => {
    if (!sessionId) return;
    openSessionBoard(sessionId, sessionTitle ?? "Untitled conversation");
    setRunError(null);
  };

  const handleCreatePasted = () => {
    if (!pastedText.trim()) return;
    createPastedBoard(pastedName, pastedText);
    setPastedName("");
    setPastedText("");
    setComposeOpen(false);
    setRunError(null);
  };

  const handleRunExtraction = async () => {
    if (!activeBoardId) return;
    setRunError(null);
    try {
      await runExtraction(activeBoardId);
    } catch (error) {
      setRunError(error instanceof Error ? error.message : String(error));
    }
  };

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="evidence-board-title">
      <header className="flex shrink-0 flex-col gap-2 border-b border-border px-4 py-3">
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <h1 id="evidence-board-title" className="text-base font-semibold text-foreground">
              {t("EvidenceBoard.title")}
            </h1>
            <p className="truncate text-xs text-muted">{t("EvidenceBoard.subtitle")}</p>
          </div>
          <IconButton size="sm" onClick={onClose} aria-label={t("EvidenceBoard.close")}>
            <X size={16} />
          </IconButton>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <select
            value={activeBoardId ?? ""}
            onChange={(event) => setActiveBoard(event.target.value || null)}
            aria-label={t("EvidenceBoard.selectBoard")}
            className="min-w-[180px] rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground"
          >
            <option value="">{t("EvidenceBoard.selectBoard")}</option>
            {boards.map((board) => (
              <option key={board.id} value={board.id}>
                {board.name} {board.sourceKind === "session" ? `· ${t("EvidenceBoard.sourceSession")}` : `· ${t("EvidenceBoard.sourcePasted")}`}
              </option>
            ))}
          </select>

          <Button size="sm" variant="secondary" onClick={handleUseThisChat} disabled={!sessionId}>
            <MessageSquareText size={13} /> {t("EvidenceBoard.useThisChat")}
          </Button>
          <Button size="sm" variant="secondary" onClick={() => setComposeOpen((value) => !value)}>
            <Plus size={13} /> {t("EvidenceBoard.newPastedBoard")}
          </Button>

          {activeBoard && (
            <>
              <Button size="sm" variant="primary" onClick={() => void handleRunExtraction()} disabled={extracting}>
                <RefreshCw size={13} className={extracting ? "animate-spin" : ""} />
                {extracting ? t("EvidenceBoard.extracting") : t("EvidenceBoard.reExtract")}
              </Button>
              <IconButton
                size="sm"
                onClick={() => deleteBoard(activeBoard.id)}
                aria-label={t("EvidenceBoard.deleteBoard")}
                className="ml-auto"
              >
                <Trash2 size={14} />
              </IconButton>
            </>
          )}
        </div>
      </header>

      {composeOpen && (
        <div className="flex shrink-0 flex-col gap-2 border-b border-border bg-surface px-4 py-3">
          <input
            type="text"
            value={pastedName}
            onChange={(event) => setPastedName(event.target.value)}
            placeholder={t("EvidenceBoard.pastedBoardNamePlaceholder")}
            className="rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
          />
          <textarea
            value={pastedText}
            onChange={(event) => setPastedText(event.target.value)}
            placeholder={t("EvidenceBoard.pastedTextPlaceholder")}
            rows={6}
            className="resize-y rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
          />
          <div className="flex items-center gap-2">
            <Button size="sm" variant="primary" onClick={handleCreatePasted} disabled={!pastedText.trim()}>
              {t("EvidenceBoard.createBoard")}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setComposeOpen(false)}>
              {t("EvidenceBoard.cancel")}
            </Button>
          </div>
        </div>
      )}

      {(runError || activeBoard?.lastExtractionError) && (
        <div role="alert" className="flex items-start justify-between gap-3 border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          <span>{t("EvidenceBoard.extractionError", { error: runError ?? activeBoard?.lastExtractionError ?? "" })}</span>
          <button type="button" className="shrink-0 underline" onClick={() => setRunError(null)}>
            {t("EvidenceBoard.dismiss")}
          </button>
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3 [overscroll-behavior:contain]">
        {!activeBoard ? (
          <div className="flex h-full flex-col items-center justify-center gap-1 text-center">
            <p className="text-sm font-medium text-foreground">{t("EvidenceBoard.noBoards")}</p>
            <p className="max-w-sm text-xs text-muted">{t("EvidenceBoard.noBoardsHint")}</p>
          </div>
        ) : (
          <>
            <div className="mb-3 flex flex-wrap items-center gap-2 text-xs text-faint">
              <span>{t("EvidenceBoard.claimsCount", { count: activeBoard.claims.length })}</span>
              {activeBoard.sourceTruncated && <StatusPill tone="neutral">{t("EvidenceBoard.sourceTruncatedNotice")}</StatusPill>}
            </div>
            {activeBoard.claims.length === 0 ? (
              <div className="flex h-full flex-col items-center justify-center gap-1 text-center">
                <p className="text-sm font-medium text-foreground">{t("EvidenceBoard.noClaimsYet")}</p>
                <p className="max-w-sm text-xs text-muted">{t("EvidenceBoard.noClaimsHint")}</p>
              </div>
            ) : (
              <ul className="space-y-2">
                {activeBoard.claims.map((claim) => (
                  <ClaimCard key={claim.id} boardId={activeBoard.id} claim={claim} />
                ))}
              </ul>
            )}
          </>
        )}
      </div>
    </section>
  );
}

export default EvidenceBoardPanel;
