import { useCallback, useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen, GitBranch, Loader2, Package, Pin, RefreshCw, RotateCcw, ShieldCheck, Trash2 } from "lucide-react";

import {
  nativeSkillsClient,
  type GitSkillCandidate,
  type GitSkillRequest,
  type NativeSkillDescriptor,
  type NativeSkillInstallPreview,
  type NativeSkillScope,
} from "../../lib/nativeSkillsClient";
import { skillLearningClient } from "../../lib/skillLearningClient";
import { useNativeSkillsStore } from "../../store/nativeSkillsStore";
import {
  skillActivationPolicyKey,
  useSkillActivationPolicyStore,
  type SkillActivationPolicy,
} from "../../store/skillActivationPolicyStore";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";

function descriptorScope(skill: NativeSkillDescriptor): NativeSkillScope | null {
  return skill.source.kind === "global" || skill.source.kind === "workspace" ? skill.source.kind : null;
}

function descriptorPolicyIdentity(skill: NativeSkillDescriptor): string {
  if (skill.source.kind === "global") return "global";
  if (skill.source.kind === "workspace") return skill.source.path;
  return `signed-package:${skill.source.package_id}`;
}

/** Strips the scheme/`.git` suffix so a card header reads `org/repo` instead of the full clone URL. */
function repoDisplayName(url: string): string {
  return url.replace(/^https?:\/\//, "").replace(/\.git$/, "");
}

const ACTIVATION_POLICIES: Array<{ value: SkillActivationPolicy; label: string }> = [
  { value: "automatic", label: "Automatic" },
  { value: "ask", label: "Ask" },
  { value: "manual", label: "Manual" },
];

function SkillPolicySelect({ command, identity, defaultPolicy = "automatic" }: { command: string; identity: string; defaultPolicy?: SkillActivationPolicy }) {
  const key = skillActivationPolicyKey("native", command, identity);
  const policy = useSkillActivationPolicyStore((state) => state.getPolicy(key, defaultPolicy));
  const pinned = useSkillActivationPolicyStore((state) => state.isPinned(key));
  const setPolicy = useSkillActivationPolicyStore((state) => state.setPolicy);
  const setPinned = useSkillActivationPolicyStore((state) => state.setPinned);
  const bumpNativeSkills = useNativeSkillsStore((state) => state.bump);
  return (
    <div className="flex items-center gap-1 text-[10px] text-faint">
      <label className="flex items-center gap-1">
        Policy
        <select
          aria-label={`/${command} activation policy`}
          value={policy}
          onChange={(event) => {
            void setPolicy(key, event.target.value as SkillActivationPolicy);
            bumpNativeSkills();
          }}
          className="h-6 rounded border border-border bg-background px-1 text-[10px] text-foreground"
        >
          {ACTIVATION_POLICIES.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
        </select>
      </label>
      <label className="ml-1 inline-flex items-center gap-0.5" title="Pin this skill higher in ranked discovery">
        <input aria-label={`Pin /${command}`} type="checkbox" checked={pinned} onChange={(event) => void setPinned(key, event.target.checked)} />
        <Pin size={10} />
      </label>
    </div>
  );
}

interface RepoGroup {
  key: string;
  repository: string;
  scope: NativeSkillScope;
  skills: NativeSkillDescriptor[];
}

/** Skills installed from the same Git repository (and scope) collapse into one card — install/enable/disable/uninstall/rollback act on the whole group together, matching how they were installed. */
function groupByRepository(skills: NativeSkillDescriptor[]): { groups: RepoGroup[]; standalone: NativeSkillDescriptor[] } {
  const groups = new Map<string, RepoGroup>();
  const standalone: NativeSkillDescriptor[] = [];
  for (const skill of skills) {
    const scope = descriptorScope(skill);
    if (!scope || !skill.git_repository || !skill.managed) {
      if (scope) standalone.push(skill);
      continue;
    }
    const key = `${scope}:${skill.git_repository}`;
    const existing = groups.get(key);
    if (existing) {
      existing.skills.push(skill);
    } else {
      groups.set(key, { key, repository: skill.git_repository, scope, skills: [skill] });
    }
  }
  for (const group of groups.values()) {
    group.skills.sort((a, b) => a.command.localeCompare(b.command));
  }
  return { groups: [...groups.values()].sort((a, b) => a.repository.localeCompare(b.repository)), standalone };
}

export function NativeSkillsManager() {
  const [skills, setSkills] = useState<NativeSkillDescriptor[]>([]);
  const [scope, setScope] = useState<NativeSkillScope>("global");
  const [localPath, setLocalPath] = useState("");
  const [gitUrl, setGitUrl] = useState("");
  const [preview, setPreview] = useState<NativeSkillInstallPreview | null>(null);
  const [previewSource, setPreviewSource] = useState<{ kind: "local"; path: string } | { kind: "git"; request: GitSkillRequest } | null>(null);
  const [gitCandidates, setGitCandidates] = useState<{ pinnedCommit: string; list: GitSkillCandidate[] } | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const bumpNativeSkills = useNativeSkillsStore((state) => state.bump);

  const refresh = useCallback(async () => {
    setError(null);
    try {
      // The learning-aware discovery: identical descriptors plus `learned`
      // provenance for whichever active content hashes this app's learning
      // loop installed, so a learned skill is visibly one here rather than
      // only in the learning panel.
      setSkills(await skillLearningClient.discover());
      // Refresh is also the explicit invalidation point for Chat's frozen
      // native-skill snapshots, including edits made outside the app.
      bumpNativeSkills();
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);

  useEffect(() => { void refresh(); }, [refresh]);

  const run = async (key: string, operation: () => Promise<unknown>) => {
    setBusy(key);
    setError(null);
    try {
      await operation();
      setPreview(null);
      setPreviewSource(null);
      setGitCandidates(null);
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  };

  const previewLocal = async () => {
    if (!localPath) return;
    setBusy("preview");
    setError(null);
    try {
      const next = await nativeSkillsClient.previewLocal(localPath, scope);
      setPreview(next);
      setPreviewSource({ kind: "local", path: localPath });
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  };

  const previewGit = async () => {
    // Commit is resolved by the backend (default branch → pinned SHA);
    // subdirectories come back as discovered candidates.
    const request: GitSkillRequest = { repository_url: gitUrl.trim(), commit: "" };
    setBusy("preview");
    setError(null);
    try {
      const outcome = await nativeSkillsClient.previewGit(request, scope);
      if (outcome.kind === "candidates") {
        setPreview(null);
        setPreviewSource(null);
        setGitCandidates({ pinnedCommit: outcome.pinned_commit, list: outcome.candidates });
        return;
      }
      setGitCandidates(null);
      setPreview(outcome.preview);
      // Install with the resolved commit so the approval digest verifies
      // against the exact snapshot that was previewed.
      setPreviewSource({ kind: "git", request: { ...request, commit: outcome.pinned_commit } });
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  };

  const installCandidates = async (selected: GitSkillCandidate[]) => {
    if (!gitCandidates || selected.length === 0) return;
    const commands = selected.map((candidate) => `/${candidate.preview.command}`).join(", ");
    const accepted = window.confirm(
      `Install ${selected.length} skill${selected.length === 1 ? "" : "s"} (${commands}) from ${gitUrl.trim()} at commit ${gitCandidates.pinnedCommit}?\n\nEach skill's content digest was verified during preview and is re-verified on install.`,
    );
    if (!accepted) return;
    const request: GitSkillRequest = { repository_url: gitUrl.trim(), commit: gitCandidates.pinnedCommit };
    await run("install", () =>
      nativeSkillsClient.installGitBulk(
        request,
        scope,
        selected.map((candidate) => ({
          subdirectory: candidate.subdirectory,
          approval_digest: candidate.preview.approval_digest,
        })),
      ));
  };

  const installPreview = async () => {
    if (!preview || !previewSource) return;
    const accepted = window.confirm(
      `Install /${preview.command} from ${preview.origin}?\n\nApproval digest:\n${preview.approval_digest}`,
    );
    if (!accepted) return;
    await run("install", () => previewSource.kind === "local"
      ? nativeSkillsClient.installLocal(previewSource.path, preview.scope, preview.approval_digest)
      : nativeSkillsClient.installGit(previewSource.request, preview.scope, preview.approval_digest));
  };

  const native = skills.filter((skill) => skill.source.kind !== "signed_package");
  const packaged = skills.filter((skill) => skill.source.kind === "signed_package");
  const { groups, standalone } = useMemo(() => groupByRepository(native), [native]);

  return (
    <section className="flex flex-col gap-3 rounded-lg border border-border bg-surface p-3">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h3 className="text-sm font-medium text-foreground">Native SKILL.md runtime</h3>
          <p className="text-xs text-faint">
            Data-only skills from global/workspace folders or a Git repository. Branches and tags resolve to a pinned commit at preview; installs verify that exact snapshot. Symlinks, executables, collisions, and unmet eligibility gates fail closed.
          </p>
        </div>
        <Button variant="ghost" size="sm" onClick={() => void refresh()} disabled={busy !== null}>
          <RefreshCw size={12} /> Refresh
        </Button>
      </div>

      {error && <p className="rounded border border-danger bg-danger-soft px-2 py-1 text-xs text-danger">{error}</p>}

      <div className="grid gap-2 sm:grid-cols-[9rem_1fr_auto]">
        <select
          value={scope}
          onChange={(event) => { setScope(event.target.value as NativeSkillScope); setPreview(null); }}
          className="h-8 rounded-md border border-border bg-background px-2 text-xs text-foreground"
        >
          <option value="global">Global scope</option>
          <option value="workspace">Workspace scope</option>
        </select>
        <div className="flex min-w-0 gap-1.5">
          <input
            value={localPath}
            readOnly
            placeholder="Choose a folder containing SKILL.md"
            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-background px-2 text-xs text-foreground"
          />
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void open({ directory: true, multiple: false }).then((path) => {
              if (typeof path === "string") { setLocalPath(path); setPreview(null); }
            })}
          >
            <FolderOpen size={12} /> Choose
          </Button>
        </div>
        <Button variant="secondary" size="sm" disabled={!localPath || busy !== null} onClick={() => void previewLocal()}>
          Preview
        </Button>
      </div>

      <div className="grid gap-2 sm:grid-cols-[1fr_auto]">
        <input value={gitUrl} onChange={(event) => setGitUrl(event.target.value)} placeholder="https://github.com/org/repo — skills are discovered automatically" className="h-8 rounded-md border border-border bg-background px-2 text-xs text-foreground" />
        <Button variant="secondary" size="sm" disabled={!gitUrl.trim() || busy !== null} onClick={() => void previewGit()}>
          <GitBranch size={12} /> Preview
        </Button>
      </div>

      {gitCandidates && (
        <div className="rounded-md border border-border bg-background p-2.5 text-xs">
          <div className="flex flex-wrap items-center gap-2">
            <p className="text-muted">
              {gitCandidates.list.length} skill{gitCandidates.list.length === 1 ? "" : "s"} found at commit{" "}
              <span className="font-mono">{gitCandidates.pinnedCommit.slice(0, 12)}…</span> — installs as one package.
            </p>
            <Button
              variant="secondary"
              size="sm"
              className="ml-auto"
              disabled={busy !== null}
              onClick={() => void installCandidates(gitCandidates.list)}
            >
              {busy === "install" && <Loader2 size={12} className="animate-spin" />} Install all ({gitCandidates.list.length})
            </Button>
          </div>
          <div className="mt-1.5 flex flex-wrap gap-1.5">
            {gitCandidates.list.map((candidate) => (
              <span
                key={candidate.subdirectory}
                className="rounded-md border border-border px-2 py-1 font-mono text-foreground"
                title={candidate.subdirectory}
              >
                /{candidate.preview.command}
              </span>
            ))}
          </div>
        </div>
      )}

      {preview && (
        <div className="rounded-md border border-warning bg-warning-soft p-2.5 text-xs">
          <div className="flex flex-wrap items-center gap-2 text-foreground">
            <ShieldCheck size={14} className="text-warning" />
            <strong>/{preview.command}</strong>
            <span>{preview.name} · {preview.version}</span>
            <span className="ml-auto">{preview.file_count} files · {preview.total_bytes.toLocaleString()} bytes</span>
          </div>
          <p className="mt-1 text-muted">{preview.description}</p>
          <p className="mt-1 break-all font-mono text-[10px] text-faint">sha256:{preview.sha256}</p>
          <p className="break-all font-mono text-[10px] text-faint">approval:{preview.approval_digest}</p>
          {!preview.eligibility.eligible && (
            <p className="mt-1 text-danger">
              Not eligible: {[
                preview.eligibility.unsupported_os ? `unsupported on ${preview.eligibility.current_os}` : "",
                preview.eligibility.missing_bins.length ? `missing bins ${preview.eligibility.missing_bins.join(", ")}` : "",
                preview.eligibility.missing_env.length ? `missing env ${preview.eligibility.missing_env.join(", ")}` : "",
              ].filter(Boolean).join("; ")}
            </p>
          )}
          <div className="mt-2 flex justify-end">
            <Button variant="secondary" size="sm" disabled={busy !== null} onClick={() => void installPreview()}>
              {busy === "install" && <Loader2 size={12} className="animate-spin" />} Approve digest & install
            </Button>
          </div>
        </div>
      )}

      <div className="flex flex-col gap-1.5">
        {groups.length === 0 && standalone.length === 0 ? (
          <p className="text-xs text-faint">No native skills discovered.</p>
        ) : (
          <>
            {groups.map((group) => {
              const commands = group.skills.map((skill) => skill.command);
              const anyEnabled = group.skills.some((skill) => skill.enabled);
              const allEligible = group.skills.every((skill) => skill.eligibility.eligible);
              const busyKey = `group:${group.key}`;
              return (
                <div key={group.key} className="rounded-md border border-border bg-background px-2.5 py-2 text-xs">
                  <div className="flex flex-wrap items-center gap-2">
                    <Package size={13} className="text-muted" />
                    <span className="font-medium text-foreground">{repoDisplayName(group.repository)}</span>
                    <span className={`rounded px-1 py-0.5 text-[10px] ${anyEnabled && allEligible ? "bg-success-soft text-success" : "bg-warning-soft text-warning"}`}>
                      {!anyEnabled ? "disabled" : allEligible ? group.scope : "ineligible"}
                    </span>
                    <span className="text-faint">{group.skills.length} skill{group.skills.length === 1 ? "" : "s"}</span>
                    <div className="ml-auto flex items-center gap-1">
                      <Button
                        variant="ghost"
                        size="sm"
                        disabled={busy !== null}
                        onClick={() => void run(`${busyKey}:toggle`, () => nativeSkillsClient.setEnabledMany(group.scope, commands, !anyEnabled))}
                      >
                        {anyEnabled ? "Disable all" : "Enable all"}
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        disabled={busy !== null}
                        onClick={() => void run(`${busyKey}:rollback`, () => nativeSkillsClient.rollbackMany(group.scope, commands))}
                      >
                        <RotateCcw size={12} /> Rollback all
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        disabled={busy !== null}
                        onClick={() => {
                          if (window.confirm(`Uninstall all ${group.skills.length} skills from ${repoDisplayName(group.repository)}? Their active versions are archived for rollback.`)) {
                            void run(`${busyKey}:uninstall`, () => nativeSkillsClient.uninstallMany(group.scope, commands));
                          }
                        }}
                      >
                        <Trash2 size={12} /> Uninstall all
                      </Button>
                    </div>
                  </div>
                  <div className="mt-1.5 flex flex-wrap gap-1.5">
                    {group.skills.map((skill) => (
                      <span key={skill.command} className="flex items-center gap-1 rounded-md border border-border px-2 py-1">
                        <span className={`font-mono ${skill.enabled ? "text-foreground" : "text-faint line-through"}`} title={`${skill.name} · ${skill.version}`}>/{skill.command}</span>
                        <SkillPolicySelect
                          command={skill.command}
                          identity={descriptorPolicyIdentity(skill)}
                          defaultPolicy={skill.managed ? "automatic" : "ask"}
                        />
                      </span>
                    ))}
                  </div>
                </div>
              );
            })}

            {standalone.map((skill) => {
              const skillScope = descriptorScope(skill);
              if (!skillScope) return null;
              const managed = skill.managed;
              return (
                <div key={`${skillScope}:${skill.command}:${skill.sha256}`} className="flex flex-wrap items-center gap-2 rounded-md border border-border bg-background px-2.5 py-2 text-xs">
                  <span className="font-mono text-foreground">/{skill.command}</span>
                  <span className="text-muted">{skill.name} · {skill.version}</span>
                  <span className={`rounded px-1 py-0.5 text-[10px] ${skill.enabled && skill.eligibility.eligible ? "bg-success-soft text-success" : "bg-warning-soft text-warning"}`}>
                    {!skill.enabled ? "disabled" : skill.eligibility.eligible ? managed ? skillScope : "external" : "ineligible"}
                  </span>
                  {!managed && <span className="text-faint">Read-only `.agents/skills`</span>}
                  {skill.learned && (
                    <span
                      className="rounded border border-border px-1 py-0.5 text-[10px] text-muted"
                      title={`Learned from runs ${skill.learned.source_run_ids.join(", ")} (${skill.learned.source_kind}), promoted by ${skill.learned.promotion_policy}`}
                    >
                      learned
                    </span>
                  )}
                  <span className="ml-auto font-mono text-[10px] text-faint">{skill.sha256.slice(0, 12)}…</span>
                  <SkillPolicySelect
                    command={skill.command}
                    identity={descriptorPolicyIdentity(skill)}
                    defaultPolicy={skill.managed ? "automatic" : "ask"}
                  />
                  {managed ? (
                    <>
                      <Button variant="ghost" size="sm" disabled={busy !== null} onClick={() => void run(`toggle:${skill.command}`, () => nativeSkillsClient.setEnabled(skillScope, skill.command, !skill.enabled))}>
                        {skill.enabled ? "Disable" : "Enable"}
                      </Button>
                      <Button variant="ghost" size="sm" disabled={busy !== null} onClick={() => void run(`rollback:${skill.command}`, () => nativeSkillsClient.rollback(skillScope, skill.command))}>
                        <RotateCcw size={12} /> Rollback
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        disabled={busy !== null}
                        onClick={() => {
                          if (window.confirm(`Uninstall /${skill.command}? Its active version is archived for rollback.`)) {
                            void run(`uninstall:${skill.command}`, () => nativeSkillsClient.uninstall(skillScope, skill.command));
                          }
                        }}
                      >
                        <Trash2 size={12} /> Uninstall
                      </Button>
                    </>
                  ) : null}
                </div>
              );
            })}
          </>
        )}
      </div>

      {packaged.length > 0 && (
        <p className="text-[11px] text-faint">
          {packaged.length} signed package skill{packaged.length === 1 ? "" : "s"} also passed the merged collision/eligibility check; manage them in Settings → Ecosystem.
        </p>
      )}
    </section>
  );
}

export default NativeSkillsManager;
