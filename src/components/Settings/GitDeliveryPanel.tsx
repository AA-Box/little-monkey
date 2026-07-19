import { isTauri } from "@tauri-apps/api/core";
import { useEffect, useMemo, useState } from "react";
import {
  Archive,
  CheckCircle2,
  GitBranch,
  GitCommitHorizontal,
  GitPullRequest,
  Loader2,
  Lock,
  LockOpen,
  Play,
  RefreshCw,
  Search,
  Send,
  ShieldAlert,
  Trash2,
  Unlock,
} from "lucide-react";

import type { DeliveryMutation, WorktreeCreateRequest } from "../../lib/gitDelivery";
import { validateCreateRequest } from "../../lib/gitDelivery";
import { useGitDeliveryStore } from "../../store/gitDeliveryStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { Button, Tabs } from "../ui";

type Section = "worktrees" | "git" | "github" | "review" | "audit";
type DiffKind = "head" | "staged" | "unstaged";

const CONTROL = "mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent";
const CARD = "rounded-lg border border-border bg-surface p-3";

function run(action: Promise<unknown>) {
  void action.catch(() => undefined);
}

function commaList(value: string): string[] {
  return [...new Set(value.split(",").map((item) => item.trim()).filter(Boolean))];
}

function asNumber(value: string): number {
  const number = Number(value);
  return Number.isInteger(number) && number > 0 ? number : 0;
}

function formatDate(value: number): string {
  return new Date(value).toLocaleString();
}

export function GitDeliveryPanel() {
  const [section, setSection] = useState<Section>("worktrees");
  const [diffKind, setDiffKind] = useState<DiffKind>("head");
  const [confirmation, setConfirmation] = useState("");
  const [selectedPaths, setSelectedPaths] = useState<string[]>([]);
  const [commitMessage, setCommitMessage] = useState("");
  const [remote, setRemote] = useState("origin");
  const [githubNumber, setGithubNumber] = useState("1");
  const [prBase, setPrBase] = useState("main");
  const [prTitle, setPrTitle] = useState("");
  const [prBody, setPrBody] = useState("");
  const [reviewModel, setReviewModel] = useState("qwen2.5-coder:14b");
  const [commentId, setCommentId] = useState("");
  const [reconciliationNotes, setReconciliationNotes] = useState<Record<string, string>>({});
  const [create, setCreate] = useState({
    repositorySlug: "",
    baseRef: "main",
    label: "issue",
    branchPrefix: "codex/delivery/",
    remotes: "origin",
    protectedBranches: "main,master,develop,release",
    allowPush: false,
    allowCreatePullRequest: false,
    allowReviewComment: false,
    allowForkWrites: false,
  });

  const roots = useWorkspaceStore((state) => state.roots);
  const refreshRoots = useWorkspaceStore((state) => state.refreshRoots);
  const workspace = primaryRoot(roots);
  const store = useGitDeliveryStore();
  const busy = Object.values(store.busy).some(Boolean);
  const selected = useMemo(
    () => store.worktrees.find((item) => item.marker.worktreeId === store.selectedWorktreeId) ?? null,
    [store.selectedWorktreeId, store.worktrees],
  );
  const worktree = store.inspection?.worktree ?? selected;
  const prNumber = asNumber(githubNumber);

  const createRequest: WorktreeCreateRequest = useMemo(() => ({
    repositoryRoot: workspace?.path ?? "",
    repositorySlug: create.repositorySlug.trim(),
    baseRef: create.baseRef.trim(),
    label: create.label.trim(),
    allowedRemotes: commaList(create.remotes),
    branchPrefix: create.branchPrefix.trim(),
    protectedBranches: commaList(create.protectedBranches),
    allowPush: create.allowPush,
    allowCreatePullRequest: create.allowCreatePullRequest,
    allowReviewComment: create.allowReviewComment,
    allowForkWrites: create.allowForkWrites,
  }), [create, workspace?.path]);
  const createErrors = useMemo(() => validateCreateRequest(createRequest), [createRequest]);

  useEffect(() => {
    if (!isTauri()) return;
    run(refreshRoots());
    run(store.refresh());
    run(store.refreshAuth());
    run(store.refreshAudit());
  }, []); // Stores expose stable Zustand actions.

  useEffect(() => {
    const paths = store.inspection?.files
      .filter((file) => !file.ignored)
      .map((file) => file.path) ?? [];
    setSelectedPaths(paths);
    if (worktree?.marker.policy.allowedRemotes[0]) {
      setRemote(worktree.marker.policy.allowedRemotes[0]);
    }
  }, [store.inspection?.headOid, worktree?.marker.worktreeId]);

  function prepare(mutation: DeliveryMutation) {
    setConfirmation("");
    run(store.prepare(mutation));
  }

  function selectedMutation<T extends DeliveryMutation["kind"]>(
    kind: T,
    payload: Record<string, unknown> = {},
  ): DeliveryMutation | null {
    if (!worktree) return null;
    return { kind, payload: { worktreeId: worktree.marker.worktreeId, ...payload } } as DeliveryMutation;
  }

  function togglePath(path: string) {
    setSelectedPaths((current) => current.includes(path)
      ? current.filter((item) => item !== path)
      : [...current, path]);
  }

  async function executeConfirmation() {
    try {
      await store.executePrepared(confirmation);
      setConfirmation("");
      setCommitMessage("");
    } catch {
      // Store exposes the error in the panel alert.
    }
  }

  const diff = store.inspection?.diffs[diffKind];

  if (!isTauri()) {
    return (
      <section className="flex min-h-0 flex-col gap-4" aria-labelledby="git-delivery-title">
        <h3 id="git-delivery-title" className="text-sm font-semibold text-foreground">Git delivery and local PR review</h3>
        <div className="rounded-lg border border-dashed border-border p-8 text-center">
          <GitBranch className="mx-auto text-faint" size={24} />
          <p className="mt-2 text-xs text-muted">Git delivery is unavailable in the browser.</p>
          <p className="mt-1 text-[11px] text-faint">Launch Little Monkey as a desktop app (<code>pnpm tauri dev</code> or the installed build) to manage worktrees, pushes, and PR reviews.</p>
        </div>
      </section>
    );
  }

  return (
    <section className="flex min-h-0 flex-col gap-4" aria-labelledby="git-delivery-title">
      <header>
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h3 id="git-delivery-title" className="text-sm font-semibold text-foreground">Git delivery and local PR review</h3>
            <p className="mt-1 max-w-3xl text-xs leading-5 text-muted">
              Work only in Little Monkey-owned branches and worktrees. Every mutation uses an exact, expiring digest; GitHub writes are limited to owned-branch push, draft PR metadata, and one deduplicated review report.
            </p>
          </div>
          <div className={`rounded-full border px-2.5 py-1 text-[11px] ${store.auth?.authenticated ? "border-success/40 text-success" : "border-warning/40 text-warning"}`}>
            {store.auth?.authenticated ? `gh · ${store.auth.account}` : store.auth?.available ? "gh auth required" : "gh unavailable"}
          </div>
        </div>
      </header>

      <Tabs
        tabs={[
          { id: "worktrees", label: "Worktrees" },
          { id: "git", label: "Git & diffs" },
          { id: "github", label: "GitHub" },
          { id: "review", label: "Local review" },
          { id: "audit", label: "Audit" },
        ]}
        active={section}
        onChange={(id) => setSection(id as Section)}
      />

      {store.error && <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">{store.error}</div>}
      {store.notice && <div role="status" className="rounded-md border border-success/40 bg-success/10 p-3 text-xs text-success">{store.notice}</div>}

      {section === "worktrees" && (
        <div className="grid gap-4 xl:grid-cols-[minmax(0,1.1fr)_minmax(20rem,.9fr)]">
          <div className={CARD}>
            <div className="flex items-center justify-between gap-2">
              <div>
                <h4 className="text-xs font-semibold text-foreground">Owned worktrees</h4>
                <p className="mt-1 text-[11px] text-muted">Recovered worktrees start locked. Cleaned records remain as audit evidence.</p>
              </div>
              <Button size="sm" disabled={busy} onClick={() => run(store.refresh())}><RefreshCw size={13} /> Refresh</Button>
            </div>
            <div className="mt-3 space-y-2">
              {store.worktrees.length === 0 && <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">No owned worktrees yet.</p>}
              {store.worktrees.map((item) => {
                const active = item.marker.worktreeId === store.selectedWorktreeId;
                return (
                  <button
                    type="button"
                    key={item.marker.worktreeId}
                    disabled={item.state === "cleaned"}
                    onClick={() => run(store.selectWorktree(item.marker.worktreeId))}
                    className={`w-full rounded-md border p-3 text-left transition-colors disabled:opacity-60 ${active ? "border-accent bg-accent/10" : "border-border bg-background hover:border-border-strong"}`}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div className="min-w-0">
                        <p className="truncate font-mono text-xs text-foreground">{item.marker.branch}</p>
                        <p className="mt-1 truncate text-[10px] text-faint">{item.marker.canonicalPath}</p>
                      </div>
                      <span className="rounded-full border border-border px-2 py-0.5 text-[10px] text-muted">{item.state}{item.locked ? " · locked" : ""}</span>
                    </div>
                  </button>
                );
              })}
            </div>
            {worktree && worktree.state !== "cleaned" && (
              <div className="mt-3 flex flex-wrap gap-2 border-t border-border pt-3">
                <Button size="sm" disabled={busy} onClick={() => {
                  const mutation = selectedMutation("set_lock", { locked: !worktree.locked, reason: worktree.locked ? null : "Locked from Git delivery panel" });
                  if (mutation) prepare(mutation);
                }}>
                  {worktree.locked ? <Unlock size={13} /> : <Lock size={13} />}{worktree.locked ? "Unlock" : "Lock"}
                </Button>
                <Button size="sm" disabled={busy || worktree.locked || worktree.state === "archived"} onClick={() => {
                  const mutation = selectedMutation("archive_worktree"); if (mutation) prepare(mutation);
                }}><Archive size={13} /> Archive clean worktree</Button>
                <Button variant="danger" size="sm" disabled={busy || worktree.locked || worktree.state !== "archived"} onClick={() => {
                  const mutation = selectedMutation("cleanup_worktree"); if (mutation) prepare(mutation);
                }}><Trash2 size={13} /> Safe cleanup</Button>
              </div>
            )}
          </div>

          <form className={CARD} onSubmit={(event) => { event.preventDefault(); prepare({ kind: "create_worktree", payload: createRequest }); }}>
            <h4 className="text-xs font-semibold text-foreground">Create from primary workspace</h4>
            <p className="mt-1 truncate text-[10px] text-faint">{workspace?.path ?? "Open a primary workspace first"}</p>
            <div className="mt-3 grid gap-3 sm:grid-cols-2">
              <label className="text-xs text-muted">GitHub repository<input className={CONTROL} placeholder="owner/repository" value={create.repositorySlug} onChange={(event) => setCreate({ ...create, repositorySlug: event.target.value })} /></label>
              <label className="text-xs text-muted">Base ref<input className={CONTROL} value={create.baseRef} onChange={(event) => setCreate({ ...create, baseRef: event.target.value })} /></label>
              <label className="text-xs text-muted">Task label<input className={CONTROL} value={create.label} onChange={(event) => setCreate({ ...create, label: event.target.value })} /></label>
              <label className="text-xs text-muted">Owned branch prefix<input className={CONTROL} value={create.branchPrefix} onChange={(event) => setCreate({ ...create, branchPrefix: event.target.value })} /></label>
              <label className="text-xs text-muted">Allowed remotes<input className={CONTROL} value={create.remotes} onChange={(event) => setCreate({ ...create, remotes: event.target.value })} /></label>
              <label className="text-xs text-muted">Protected branches<input className={CONTROL} value={create.protectedBranches} onChange={(event) => setCreate({ ...create, protectedBranches: event.target.value })} /></label>
            </div>
            <fieldset className="mt-3 rounded-md border border-border p-3">
              <legend className="px-1 text-[11px] font-medium text-foreground">Frozen remote-write policy</legend>
              <div className="grid gap-2 text-xs text-muted sm:grid-cols-2">
                <label className="flex items-center gap-2"><input type="checkbox" checked={create.allowPush} onChange={(event) => setCreate({ ...create, allowPush: event.target.checked, allowCreatePullRequest: event.target.checked ? create.allowCreatePullRequest : false, allowReviewComment: event.target.checked ? create.allowReviewComment : false })} /> Push owned branch</label>
                <label className="flex items-center gap-2"><input type="checkbox" disabled={!create.allowPush} checked={create.allowCreatePullRequest} onChange={(event) => setCreate({ ...create, allowCreatePullRequest: event.target.checked })} /> Create/update draft PR</label>
                <label className="flex items-center gap-2"><input type="checkbox" disabled={!create.allowPush} checked={create.allowReviewComment} onChange={(event) => setCreate({ ...create, allowReviewComment: event.target.checked })} /> Publish review report</label>
                <label className="flex items-center gap-2 text-warning"><input type="checkbox" checked={create.allowForkWrites} onChange={(event) => setCreate({ ...create, allowForkWrites: event.target.checked })} /> Allow fork writes (advanced)</label>
              </div>
              <p className="mt-2 text-[10px] leading-4 text-faint">Merge, force-push, branch deletion, and review-thread resolution are never exposed.</p>
            </fieldset>
            {createErrors.map((error) => <p key={error} className="mt-2 text-[11px] text-warning">{error}</p>)}
            <Button type="submit" variant="primary" className="mt-3" disabled={busy || createErrors.length > 0 || !workspace}><GitBranch size={14} /> Preview owned worktree</Button>
          </form>
        </div>
      )}

      {section === "git" && (
        !store.inspection ? <EmptySelection /> : (
          <div className="grid gap-4 xl:grid-cols-[minmax(18rem,.75fr)_minmax(0,1.25fr)]">
            <div className={CARD}>
              <div className="flex items-start justify-between gap-2">
                <div>
                  <h4 className="font-mono text-xs text-foreground">{store.inspection.worktree.marker.branch}</h4>
                  <p className="mt-1 text-[10px] text-faint">HEAD {store.inspection.headOid.slice(0, 12)} · ahead {store.inspection.ahead} · behind {store.inspection.behind}</p>
                </div>
                <Button size="sm" disabled={busy} onClick={() => run(store.refreshInspection())}><RefreshCw size={13} /> Refresh</Button>
              </div>
              <div className="mt-3 max-h-72 space-y-1 overflow-auto rounded-md border border-border bg-background p-2">
                {store.inspection.files.length === 0 && <p className="p-3 text-center text-xs text-faint">Owned worktree is clean.</p>}
                {store.inspection.files.map((file) => (
                  <label key={`${file.path}:${file.oldPath ?? ""}`} className="flex items-start gap-2 rounded px-1.5 py-1 text-xs text-muted hover:bg-surface">
                    <input type="checkbox" className="mt-0.5" disabled={file.ignored} checked={selectedPaths.includes(file.path)} onChange={() => togglePath(file.path)} />
                    <span className="w-7 shrink-0 font-mono text-[10px] text-faint">{file.indexStatus}{file.worktreeStatus}</span>
                    <span className="min-w-0 break-all font-mono text-[11px] text-foreground">{file.path}{file.oldPath ? ` ← ${file.oldPath}` : ""}</span>
                  </label>
                ))}
              </div>
              <div className="mt-3 flex flex-wrap gap-2">
                <Button size="sm" disabled={busy || selectedPaths.length === 0 || worktree?.state === "archived"} onClick={() => {
                  const mutation = selectedMutation("stage", { paths: selectedPaths }); if (mutation) prepare(mutation);
                }}>Stage selected</Button>
                <Button size="sm" onClick={() => setSelectedPaths(store.inspection?.files.filter((file) => !file.ignored).map((file) => file.path) ?? [])}>Select all</Button>
                <Button size="sm" onClick={() => setSelectedPaths([])}>Clear</Button>
              </div>
              <label className="mt-3 block text-xs text-muted">Commit message<textarea className={`${CONTROL} min-h-20 resize-y`} value={commitMessage} onChange={(event) => setCommitMessage(event.target.value)} /></label>
              <Button variant="primary" className="mt-2" disabled={busy || selectedPaths.length === 0 || !commitMessage.trim() || worktree?.state === "archived"} onClick={() => {
                const mutation = selectedMutation("commit", { paths: selectedPaths, message: commitMessage }); if (mutation) prepare(mutation);
              }}><GitCommitHorizontal size={14} /> Commit selected only</Button>
              {worktree?.marker.policy.allowPush && (
                <div className="mt-4 border-t border-border pt-3">
                  <label className="text-xs text-muted">Declared remote<select className={CONTROL} value={remote} onChange={(event) => setRemote(event.target.value)}>{worktree.marker.policy.allowedRemotes.map((item) => <option key={item}>{item}</option>)}</select></label>
                  <Button className="mt-2" disabled={busy || worktree.state === "archived"} onClick={() => {
                    const mutation = selectedMutation("push", { remote }); if (mutation) prepare(mutation);
                  }}><Send size={14} /> Preview push</Button>
                </div>
              )}
            </div>
            <div className={`${CARD} min-w-0`}>
              <Tabs tabs={[{ id: "head", label: "HEAD" }, { id: "staged", label: "Staged" }, { id: "unstaged", label: "Unstaged" }]} active={diffKind} onChange={(id) => setDiffKind(id as DiffKind)} />
              <div className="mt-3 flex items-center justify-between text-[10px] text-faint"><span>{diffKind} diff</span>{diff?.truncated && <span className="text-warning">Truncated at 8 MiB</span>}</div>
              <pre tabIndex={0} className="mt-2 max-h-[34rem] overflow-auto whitespace-pre rounded-md border border-border bg-background p-3 font-mono text-[11px] leading-5 text-muted">{diff?.text || "No diff in this view."}</pre>
            </div>
          </div>
        )
      )}

      {section === "github" && (
        !worktree ? <EmptySelection /> : (
          <div className="space-y-4">
            <div className={`${CARD} flex flex-wrap items-end gap-3`}>
              <label className="min-w-40 flex-1 text-xs text-muted">Issue or PR number<input type="number" min={1} className={CONTROL} value={githubNumber} onChange={(event) => setGithubNumber(event.target.value)} /></label>
              <Button disabled={busy || !prNumber} onClick={() => run(store.loadGitHub(prNumber))}><Search size={14} /> Read issue, PR, threads & checks</Button>
              <Button disabled={busy} onClick={() => run(store.refreshAuth())}><GitPullRequest size={14} /> Refresh gh auth</Button>
            </div>
            <div className="grid gap-4 lg:grid-cols-2">
              {([["Issue", store.issue], ["Pull request", store.pullRequest], ["Unresolved review threads", store.reviewThreads], ["Checks", store.checks]] as const).map(([title, value]) => (
                <div key={title} className={CARD}>
                  <h4 className="text-xs font-semibold text-foreground">{title}</h4>
                  <pre tabIndex={0} className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md bg-background p-2 text-[10px] leading-4 text-muted">{value ? JSON.stringify(value, null, 2) : "Not loaded or unavailable for this number."}</pre>
                </div>
              ))}
            </div>
            {worktree.marker.policy.allowCreatePullRequest && (
              <div className={CARD}>
                <h4 className="text-xs font-semibold text-foreground">Draft pull request</h4>
                <p className="mt-1 text-[11px] text-muted">Creation requires the current owned HEAD to be pushed. Updates are refused unless this exact branch still owns an open draft.</p>
                <div className="mt-3 grid gap-3 sm:grid-cols-2">
                  <label className="text-xs text-muted">Base branch<input className={CONTROL} value={prBase} onChange={(event) => setPrBase(event.target.value)} /></label>
                  <label className="text-xs text-muted">Draft PR number (for update)<input type="number" min={1} className={CONTROL} value={githubNumber} onChange={(event) => setGithubNumber(event.target.value)} /></label>
                  <label className="text-xs text-muted sm:col-span-2">Title<input className={CONTROL} value={prTitle} onChange={(event) => setPrTitle(event.target.value)} /></label>
                  <label className="text-xs text-muted sm:col-span-2">Body<textarea className={`${CONTROL} min-h-28 resize-y`} value={prBody} onChange={(event) => setPrBody(event.target.value)} /></label>
                </div>
                <div className="mt-3 flex flex-wrap gap-2">
                  <Button variant="primary" disabled={busy || !prTitle.trim() || !prBase.trim()} onClick={() => {
                    const mutation = selectedMutation("create_draft_pr", { base: prBase, title: prTitle, body: prBody }); if (mutation) prepare(mutation);
                  }}>Create draft</Button>
                  <Button disabled={busy || !prNumber || !prTitle.trim()} onClick={() => {
                    const mutation = selectedMutation("update_draft_pr", { prNumber, title: prTitle, body: prBody }); if (mutation) prepare(mutation);
                  }}>Update exact draft</Button>
                </div>
              </div>
            )}
          </div>
        )
      )}

      {section === "review" && (
        !worktree ? <EmptySelection /> : (
          <div className="space-y-4">
            <div className={`${CARD} grid gap-3 md:grid-cols-[10rem_minmax(12rem,1fr)_auto_auto] md:items-end`}>
              <label className="text-xs text-muted">PR number<input type="number" min={1} className={CONTROL} value={githubNumber} onChange={(event) => setGithubNumber(event.target.value)} /></label>
              <label className="text-xs text-muted">Local Ollama reviewer model<input className={CONTROL} value={reviewModel} onChange={(event) => setReviewModel(event.target.value)} /></label>
              <Button variant="primary" disabled={busy || !prNumber || !reviewModel.trim()} onClick={() => run(store.runReview(prNumber, reviewModel))}>{store.busy.review ? <Loader2 className="animate-spin" size={14} /> : <Play size={14} />} Review locally</Button>
              <Button disabled={busy || !prNumber} onClick={() => run(store.refreshReports(prNumber))}><RefreshCw size={14} /> Reports</Button>
            </div>
            <div className="rounded-md border border-border bg-background p-3 text-[11px] leading-5 text-muted">
              PR title and diff are sent only to <code>127.0.0.1:11434</code>. Findings must map to a new-side diff line or the entire model response is rejected.
            </div>
            {store.reports.length === 0 && <p className="rounded-md border border-dashed border-border p-6 text-center text-xs text-faint">No stored report for this PR.</p>}
            {store.reports.map((report) => (
              <article key={report.reportId} className={CARD}>
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div>
                    <h4 className="text-sm font-medium text-foreground">{report.summary}</h4>
                    <p className="mt-1 font-mono text-[10px] text-faint">{report.model} · {report.headOid.slice(0, 12)} · {report.reportDigest.slice(0, 12)}</p>
                  </div>
                  <div className="flex items-center gap-2">
                    {report.publishedCommentId && <span className="text-[10px] text-success"><CheckCircle2 size={12} className="inline" /> comment {report.publishedCommentId}</span>}
                    {worktree.marker.policy.allowReviewComment && <Button size="sm" disabled={busy} onClick={() => {
                      const mutation = selectedMutation("publish_review", { reportId: report.reportId }); if (mutation) prepare(mutation);
                    }}><Send size={13} /> {report.publishedCommentId ? "Update report" : "Publish report"}</Button>}
                  </div>
                </div>
                <div className="mt-3 space-y-2">
                  {report.findings.length === 0 && <p className="text-xs text-muted">No line-mapped findings.</p>}
                  {report.findings.map((finding) => (
                    <div key={finding.findingId} className="rounded-md border border-border bg-background p-3">
                      <div className="flex flex-wrap gap-2 text-[10px]"><span className={finding.severity === "blocking" ? "text-danger" : finding.severity === "warning" ? "text-warning" : "text-muted"}>{finding.severity}</span><span className="font-mono text-accent">{finding.path}:{finding.line}</span></div>
                      <p className="mt-1 text-xs font-medium text-foreground">{finding.title}</p>
                      <p className="mt-1 whitespace-pre-wrap text-xs leading-5 text-muted">{finding.body}</p>
                    </div>
                  ))}
                </div>
              </article>
            ))}
            <div className={CARD}>
              <h4 className="text-xs font-semibold text-foreground">Apply one explicitly selected comment</h4>
              <p className="mt-1 text-[11px] leading-5 text-muted">Queues a daemon-owned isolated patch task with commit permission only. Push, PR/comment writes, merge, force-push, and thread resolution stay disabled.</p>
              <div className="mt-3 grid gap-3 sm:grid-cols-[10rem_minmax(12rem,1fr)_auto] sm:items-end">
                <label className="text-xs text-muted">Comment database ID<input type="number" min={1} className={CONTROL} value={commentId} onChange={(event) => setCommentId(event.target.value)} /></label>
                <label className="text-xs text-muted">Patch model<input className={CONTROL} value={reviewModel} onChange={(event) => setReviewModel(event.target.value)} /></label>
                <Button disabled={busy || !prNumber || !asNumber(commentId) || !reviewModel.trim()} onClick={() => {
                  const mutation = selectedMutation("queue_patch_task", { prNumber, commentId: asNumber(commentId), model: reviewModel }); if (mutation) prepare(mutation);
                }}><GitBranch size={14} /> Queue isolated patch</Button>
              </div>
            </div>
          </div>
        )
      )}

      {section === "audit" && (
        <div className="space-y-4">
          <div className={CARD}>
            <div className="flex items-center justify-between gap-2"><div><h4 className="text-xs font-semibold text-foreground">Reconciliation queue</h4><p className="mt-1 text-[11px] text-muted">Interrupted or ambiguous operations are never retried automatically. Verify the target state outside Little Monkey, then record the observed outcome with a note.</p></div><Button size="sm" disabled={busy} onClick={() => run(store.refreshAudit())}><RefreshCw size={13} /> Refresh</Button></div>
            <div className="mt-3 space-y-3">
              {store.reconciliations.map((execution) => {
                const note = reconciliationNotes[execution.requestDigest] ?? "";
                return (
                  <article key={execution.requestDigest} className="rounded-md border border-warning/40 bg-warning/5 p-3">
                    <div className="flex flex-wrap items-start justify-between gap-2">
                      <div>
                        <p className="text-xs font-semibold text-warning">Manual verification required</p>
                        <p className="mt-1 font-mono text-[10px] text-foreground">{execution.action} · {execution.target}</p>
                        <p className="mt-1 font-mono text-[10px] text-faint">{execution.requestDigest}</p>
                      </div>
                      <span className="text-[10px] text-faint">{formatDate(execution.updatedAtMs)}</span>
                    </div>
                    {execution.error && <p className="mt-2 whitespace-pre-wrap break-words text-[11px] leading-5 text-muted">{execution.error}</p>}
                    <label className="mt-3 block text-xs text-muted">Verification note<textarea className={`${CONTROL} min-h-20 resize-y`} maxLength={4096} placeholder="What did you inspect, and what exact state did you observe?" value={note} onChange={(event) => setReconciliationNotes((current) => ({ ...current, [execution.requestDigest]: event.target.value }))} /></label>
                    <div className="mt-2 flex flex-wrap gap-2">
                      <Button size="sm" variant="primary" disabled={busy || !note.trim()} onClick={() => prepare({ kind: "resolve_reconciliation", payload: { requestDigest: execution.requestDigest, resolution: "completed", note: note.trim() } })}><CheckCircle2 size={13} /> Verified completed</Button>
                      <Button size="sm" disabled={busy || !note.trim()} onClick={() => prepare({ kind: "resolve_reconciliation", payload: { requestDigest: execution.requestDigest, resolution: "not_applied", note: note.trim() } })}><ShieldAlert size={13} /> Verified not applied</Button>
                    </div>
                  </article>
                );
              })}
              {store.reconciliations.length === 0 && <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">No operations need reconciliation.</p>}
            </div>
          </div>
          <div className={CARD}>
            <div><h4 className="text-xs font-semibold text-foreground">Append-only delivery audit</h4><p className="mt-1 text-[11px] text-muted">Pending, completion, failure, and reconciliation records retain the exact request digest and target.</p></div>
            <div className="mt-3 overflow-x-auto">
              <table className="w-full min-w-[46rem] text-left text-[11px]">
                <thead className="border-b border-border text-faint"><tr><th className="p-2 font-medium">Time</th><th className="p-2 font-medium">Action</th><th className="p-2 font-medium">Target</th><th className="p-2 font-medium">Outcome</th><th className="p-2 font-medium">Digest / detail</th></tr></thead>
                <tbody>{store.audit.map((entry) => <tr key={entry.auditId} className="border-b border-border/60 align-top"><td className="p-2 text-muted">{formatDate(entry.occurredAtMs)}</td><td className="p-2 font-mono text-foreground">{entry.action}</td><td className="max-w-56 break-all p-2 text-muted">{entry.target ?? "—"}</td><td className={`p-2 ${entry.outcome === "success" || entry.outcome === "reconciled_completed" ? "text-success" : entry.outcome === "pending" || entry.outcome === "needs_reconciliation" ? "text-warning" : "text-danger"}`}>{entry.outcome}</td><td className="max-w-sm p-2"><span className="font-mono text-faint">{entry.requestDigest.slice(0, 16)}</span>{entry.detail && <p className="mt-1 break-words text-muted">{entry.detail}</p>}</td></tr>)}</tbody>
              </table>
              {store.audit.length === 0 && <p className="p-6 text-center text-xs text-faint">No delivery mutations recorded.</p>}
            </div>
          </div>
        </div>
      )}

      {store.preview && store.pendingMutation && (
        <div className="fixed inset-0 z-[80] flex items-center justify-center bg-black/60 p-4" role="dialog" aria-modal="true" aria-labelledby="delivery-confirm-title" onMouseDown={(event) => { if (event.target === event.currentTarget) store.cancelPreview(); }}>
          <div className="w-full max-w-lg rounded-xl border border-border bg-surface p-5 shadow-2xl">
            <div className="flex items-start gap-3">
              <div className={`rounded-full p-2 ${store.preview.external ? "bg-warning/15 text-warning" : "bg-accent/15 text-accent"}`}><ShieldAlert size={18} /></div>
              <div className="min-w-0">
                <h4 id="delivery-confirm-title" className="text-sm font-semibold text-foreground">{store.preview.summary}</h4>
                <p className="mt-1 text-xs leading-5 text-muted">{store.preview.impact}</p>
              </div>
            </div>
            <dl className="mt-4 grid grid-cols-[7rem_minmax(0,1fr)] gap-x-3 gap-y-2 rounded-md border border-border bg-background p-3 text-[11px]">
              <dt className="text-faint">Repository</dt><dd className="break-all font-mono text-foreground">{store.preview.repositorySlug}</dd>
              <dt className="text-faint">Owned branch</dt><dd className="break-all font-mono text-foreground">{store.preview.branch ?? "created after confirmation"}</dd>
              <dt className="text-faint">Scope</dt><dd className={store.preview.external ? "text-warning" : "text-muted"}>{store.preview.external ? "External mutation" : "Local mutation"}</dd>
              <dt className="text-faint">Digest</dt><dd className="break-all font-mono text-faint">{store.preview.digest}</dd>
              <dt className="text-faint">Expires</dt><dd className="text-muted">{formatDate(store.preview.expiresAtMs)}</dd>
            </dl>
            <label className="mt-4 block text-xs text-muted">Type <code className="select-all text-foreground">{store.preview.confirmationPhrase}</code><input autoFocus autoComplete="off" spellCheck={false} className={`${CONTROL} font-mono`} value={confirmation} onChange={(event) => setConfirmation(event.target.value)} /></label>
            <div className="mt-4 flex justify-end gap-2">
              <Button disabled={store.busy.execute} onClick={() => { store.cancelPreview(); setConfirmation(""); }}>Cancel</Button>
              <Button variant={store.preview.external ? "danger" : "primary"} disabled={store.busy.execute || confirmation !== store.preview.confirmationPhrase || Date.now() > store.preview.expiresAtMs} onClick={() => run(executeConfirmation())}>
                {store.busy.execute && <Loader2 className="animate-spin" size={14} />}{store.preview.external ? "Execute external mutation" : "Execute mutation"}
              </Button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}

function EmptySelection() {
  return <div className="rounded-lg border border-dashed border-border p-8 text-center"><LockOpen className="mx-auto text-faint" size={24} /><p className="mt-2 text-xs text-muted">Create or select an active owned worktree first.</p></div>;
}
