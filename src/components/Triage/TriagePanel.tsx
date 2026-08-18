import { useEffect, useMemo, useState } from "react";
import {
  ExternalLink,
  GitPullRequest,
  MessageCircle,
  RefreshCw,
  Ticket,
  Trash2,
  type LucideIcon,
} from "lucide-react";
import { Button, StatusPill } from "../ui";
import {
  useTriageStore,
  type DraftActionKind,
  type TriageItem,
  type TriageSource,
  type TriageSourceSpec,
} from "../../store/triageStore";
import { useConnectorsStore } from "../../store/connectorsStore";
import { getActiveChatTarget } from "../../store/modelStore";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";

const SOURCE_ICONS: Record<TriageSource, LucideIcon> = {
  github: GitPullRequest,
  slack: MessageCircle,
  jira: Ticket,
};

const SOURCE_LABEL_KEYS: Record<TriageSource, string> = {
  github: "TriagePanel.sourceGithub",
  slack: "TriagePanel.sourceSlack",
  jira: "TriagePanel.sourceJira",
};

const ACTION_LABEL_KEYS: Record<DraftActionKind, string> = {
  reply: "TriagePanel.actionReply",
  comment: "TriagePanel.actionComment",
  status_update: "TriagePanel.actionStatusUpdate",
};

const SOURCE_FIELD_CLASS =
  "h-8 min-w-0 flex-1 rounded-md border border-border bg-surface text-sm leading-5 text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent";

function formatScore(score: number): string {
  return score.toFixed(1);
}

/** GitHub owner/repo source row — no connector picker needed, identity comes
 * from the machine-wide `gh` CLI session, same as everywhere else GitHub is
 * read in this app. */
function GithubSourceRow({ onAdd }: { onAdd: (spec: TriageSourceSpec) => void }) {
  const { t } = useT();
  const [owner, setOwner] = useState("");
  const [repo, setRepo] = useState("");
  const canAdd = owner.trim().length > 0 && repo.trim().length > 0;
  return (
    <div className="flex items-center gap-2">
      <input
        type="text"
        value={owner}
        onChange={(event) => setOwner(event.target.value)}
        placeholder={t("TriagePanel.githubOwnerPlaceholder")}
        className={`${SOURCE_FIELD_CLASS} px-2.5`}
      />
      <input
        type="text"
        value={repo}
        onChange={(event) => setRepo(event.target.value)}
        placeholder={t("TriagePanel.githubRepoPlaceholder")}
        className={`${SOURCE_FIELD_CLASS} px-2.5`}
      />
      <Button
        size="sm"
        disabled={!canAdd}
        onClick={() => {
          onAdd({ kind: "github", owner: owner.trim(), repo: repo.trim() });
          setOwner("");
          setRepo("");
        }}
      >
        {t("TriagePanel.addSourceButton")}
      </Button>
    </div>
  );
}

/** Slack/Jira source row — needs an existing Connector Catalog account of the
 * matching provider (see Settings → Connectors) plus a channel id / project
 * key. */
function ConnectorSourceRow({
  provider,
  onAdd,
}: {
  provider: "slack" | "jira";
  onAdd: (spec: TriageSourceSpec) => void;
}) {
  const { t } = useT();
  const accounts = useConnectorsStore((s) => s.accounts).filter((a) => a.provider === provider);
  const [accountId, setAccountId] = useState("");
  const [value, setValue] = useState("");
  const canAdd = accountId.length > 0 && value.trim().length > 0;

  return (
    <div className="flex items-center gap-2">
      <select
        value={accountId}
        onChange={(event) => setAccountId(event.target.value)}
        className={`${SOURCE_FIELD_CLASS} px-2`}
      >
        <option value="">{t("TriagePanel.selectConnectorPlaceholder")}</option>
        {accounts.map((account) => (
          <option key={account.id} value={account.id}>
            {account.label}
          </option>
        ))}
      </select>
      <input
        type="text"
        value={value}
        onChange={(event) => setValue(event.target.value)}
        placeholder={
          provider === "slack"
            ? t("TriagePanel.slackChannelPlaceholder")
            : t("TriagePanel.jiraProjectPlaceholder")
        }
        className={`${SOURCE_FIELD_CLASS} px-2.5`}
      />
      <Button
        size="sm"
        disabled={!canAdd}
        onClick={() => {
          if (provider === "slack") {
            onAdd({ kind: "slack", connector_account_id: accountId, channel_id: value.trim() });
          } else {
            onAdd({ kind: "jira", connector_account_id: accountId, project_key: value.trim() });
          }
          setValue("");
        }}
      >
        {t("TriagePanel.addSourceButton")}
      </Button>
    </div>
  );
}

function sourceSpecLabel(spec: TriageSourceSpec): string {
  if (spec.kind === "github") return `${spec.owner}/${spec.repo}`;
  if (spec.kind === "slack") return `#${spec.channel_id}`;
  return spec.project_key;
}

function ItemRow({ item, selected, onSelect }: { item: TriageItem; selected: boolean; onSelect: () => void }) {
  const { t } = useT();
  const Icon = SOURCE_ICONS[item.source];
  return (
    <button
      type="button"
      onClick={onSelect}
      className={`flex w-full items-start gap-2 rounded-lg border p-2.5 text-left transition-colors ${
        selected ? "border-accent bg-surface-2" : "border-border bg-background hover:bg-surface-2"
      }`}
    >
      <span className="mt-0.5 shrink-0 text-muted" title={t(SOURCE_LABEL_KEYS[item.source])}>
        <Icon size={15} />
      </span>
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-foreground">{item.title}</p>
        <p className="mt-0.5 truncate text-xs text-faint">{item.summary}</p>
      </div>
      <span className="shrink-0 text-xs font-semibold text-accent" title={t("TriagePanel.rankScoreLabel")}>
        {formatScore(item.rank_score)}
      </span>
    </button>
  );
}

function ItemDetail({ item, onDiscard }: { item: TriageItem; onDiscard: (itemId: string) => void }) {
  const { t } = useT();
  const generateDraft = useTriageStore((s) => s.generateDraft);
  const sendDraft = useTriageStore((s) => s.sendDraft);
  const [generating, setGenerating] = useState(false);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const chatTarget = getActiveChatTarget();
  const canGenerate = chatTarget.kind === "provider" && !!chatTarget.providerId && !!chatTarget.model;

  async function handleGenerate() {
    if (chatTarget.kind !== "provider" || !chatTarget.providerId || !chatTarget.model) return;
    setGenerating(true);
    setError(null);
    try {
      await generateDraft(item.id, chatTarget.providerId, chatTarget.model);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setGenerating(false);
    }
  }

  async function handleSend() {
    setSending(true);
    setError(null);
    try {
      await sendDraft(item.id);
    } catch (err) {
      setError(errorMessage(err));
      setSending(false);
    }
  }

  const action = item.suggested_action;
  const hasDraft = !!action?.draft_text.trim();

  return (
    <div className="flex flex-col gap-3 p-3">
      <div>
        <h4 className="text-sm font-semibold text-foreground">{item.title}</h4>
        <p className="mt-1 text-xs leading-5 text-muted">{item.summary}</p>
        <a
          href={item.url}
          target="_blank"
          rel="noreferrer"
          className="mt-1 inline-flex items-center gap-1 text-xs text-accent hover:underline"
        >
          {t("TriagePanel.openSourceLink")}
          <ExternalLink size={11} />
        </a>
      </div>

      {action && (
        <div className="rounded-md border border-border bg-surface-2 p-2.5">
          <div className="flex items-center justify-between gap-2">
            <StatusPill tone="neutral">{t(ACTION_LABEL_KEYS[action.kind])}</StatusPill>
            <span className="truncate text-xs text-faint">{action.target}</span>
          </div>
          <p className="mt-2 whitespace-pre-wrap text-sm text-foreground">
            {hasDraft ? action.draft_text : t("TriagePanel.noDraftYet")}
          </p>
        </div>
      )}

      {!canGenerate && !hasDraft && (
        <p className="rounded-md bg-surface-2 px-2 py-1.5 text-xs text-muted">
          {t("TriagePanel.noModelSelectedNotice")}
        </p>
      )}
      {error && <p className="text-xs text-danger">{error}</p>}

      <div className="flex flex-wrap justify-end gap-2">
        <Button variant="ghost" size="sm" onClick={() => onDiscard(item.id)} disabled={sending}>
          <Trash2 size={12} />
          {t("TriagePanel.discardButton")}
        </Button>
        <Button
          variant="secondary"
          size="sm"
          onClick={() => void handleGenerate()}
          disabled={!canGenerate || generating || sending}
        >
          <RefreshCw size={12} className={generating ? "animate-spin" : ""} />
          {generating
            ? t("TriagePanel.generatingButton")
            : hasDraft
              ? t("TriagePanel.regenerateButton")
              : t("TriagePanel.generateDraftButton")}
        </Button>
        <Button size="sm" onClick={() => void handleSend()} disabled={!hasDraft || sending}>
          {sending ? t("TriagePanel.sendingButton") : t("TriagePanel.approveAndSendButton")}
        </Button>
      </div>
    </div>
  );
}

/**
 * Settings "Inbox Triage" tab (ROADMAP.md, Phase 3): read-only ranked queues
 * over GitHub issues/PRs, Slack channels, and Jira issues built on the
 * Connector Catalog, with draft-only reply/comment/status-update generation.
 * Every send goes through the same permission modal as any other tool call —
 * see `triage.rs`'s module doc. Gmail/Outlook triage is an explicit non-goal
 * (no PAT-equivalent auth model for a real inbox — see the notice below).
 */
export function TriagePanel() {
  const { t } = useT();
  const items = useTriageStore((s) => s.items);
  const loading = useTriageStore((s) => s.loading);
  const error = useTriageStore((s) => s.error);
  const list = useTriageStore((s) => s.list);
  const refresh = useTriageStore((s) => s.refresh);
  const discard = useTriageStore((s) => s.discard);
  const refreshConnectors = useConnectorsStore((s) => s.refresh);

  const [sources, setSources] = useState<TriageSourceSpec[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  useEffect(() => {
    void list();
    void refreshConnectors();
  }, [list, refreshConnectors]);

  const selected = useMemo(() => items.find((item) => item.id === selectedId) ?? null, [items, selectedId]);

  function addSource(spec: TriageSourceSpec) {
    setSources((prev) => [...prev, spec]);
  }

  function removeSource(index: number) {
    setSources((prev) => prev.filter((_, i) => i !== index));
  }

  async function handleRefresh() {
    if (sources.length === 0) return;
    await refresh(sources);
  }

  return (
    <div className="flex flex-col gap-3 py-2">
      <p className="text-xs text-muted">{t("TriagePanel.description")}</p>
      <p className="rounded-md bg-surface-2 px-2 py-1.5 text-xs text-muted">{t("TriagePanel.nonGoalNotice")}</p>
      {error && <p className="text-xs text-danger">{error}</p>}

      <section className="rounded-lg border border-border bg-surface p-3">
        <h3 className="text-sm font-semibold text-foreground">{t("TriagePanel.sourcesHeading")}</h3>
        <div className="mt-3 flex flex-col gap-2">
          <GithubSourceRow onAdd={addSource} />
          <ConnectorSourceRow provider="slack" onAdd={addSource} />
          <ConnectorSourceRow provider="jira" onAdd={addSource} />
        </div>
        {sources.length > 0 && (
          <ul className="mt-3 flex flex-wrap gap-1.5">
            {sources.map((spec, index) => {
              const Icon = SOURCE_ICONS[spec.kind];
              return (
                <li
                  key={`${spec.kind}-${index}`}
                  className="flex items-center gap-1.5 rounded-full border border-border bg-surface-2 px-2 py-1 text-xs text-foreground"
                >
                  <Icon size={11} className="text-muted" />
                  {sourceSpecLabel(spec)}
                  <button
                    type="button"
                    onClick={() => removeSource(index)}
                    className="text-faint hover:text-danger"
                    aria-label={t("TriagePanel.removeSourceButton")}
                  >
                    ×
                  </button>
                </li>
              );
            })}
          </ul>
        )}
        <div className="mt-3 flex justify-end">
          <Button size="sm" onClick={() => void handleRefresh()} disabled={sources.length === 0 || loading}>
            <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
            {loading ? t("TriagePanel.refreshingButton") : t("TriagePanel.refreshQueueButton")}
          </Button>
        </div>
      </section>

      <section className="grid gap-3 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <div className="rounded-lg border border-border bg-surface p-3">
          <div className="flex items-center justify-between gap-2">
            <h3 className="text-sm font-semibold text-foreground">{t("TriagePanel.queueHeading")}</h3>
            <span className="text-xs text-faint">{t("TriagePanel.itemCountLabel", { count: items.length })}</span>
          </div>
          <div className="mt-3 flex flex-col gap-2">
            {items.length === 0 ? (
              <p className="px-1 text-xs text-faint">{t("TriagePanel.emptyQueueState")}</p>
            ) : (
              items.map((item) => (
                <ItemRow
                  key={item.id}
                  item={item}
                  selected={item.id === selectedId}
                  onSelect={() => setSelectedId(item.id)}
                />
              ))
            )}
          </div>
        </div>

        <div className="rounded-lg border border-border bg-surface">
          {selected ? (
            <ItemDetail key={selected.id} item={selected} onDiscard={discard} />
          ) : (
            <p className="p-3 text-xs text-faint">{t("TriagePanel.noSelectionState")}</p>
          )}
        </div>
      </section>
    </div>
  );
}

export default TriagePanel;
