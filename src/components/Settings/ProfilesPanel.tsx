import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Plus, Trash2, UserCheck } from "lucide-react";
import { Button, StatusPill } from "../ui";
import { useT } from "../../lib/i18n";

/** Mirrors `profiles::ProfileQuota` (serde camelCase). `null` is unbounded. */
export type ProfileQuota = {
  maxConcurrentRuns: number | null;
  maxMemoryBytes: number | null;
  maxRuntimeMs: number | null;
};

/** Mirrors `profiles::ProfileSummary`, which flattens `Profile` into itself. */
export type ProfileSummary = {
  id: string;
  name: string;
  createdAtMs: number;
  fairShareWeight: number;
  quota: ProfileQuota;
  active: boolean;
  root: string;
  share: number;
};

const DEFAULT_PROFILE_ID = "default";

function megabytes(bytes: number | null): string {
  return bytes === null ? "" : String(Math.round(bytes / (1024 * 1024)));
}

function parseOptionalNumber(raw: string, scale = 1): number | null {
  const trimmed = raw.trim();
  if (trimmed === "") return null;
  const value = Number(trimmed);
  if (!Number.isFinite(value) || value <= 0) return null;
  return Math.round(value * scale);
}

/**
 * Per-profile limits (K4 quota + K8 share). Edited as a draft and applied on
 * submit rather than on every keystroke: each save is a registry write the
 * daemon reads at startup, and a write per character would be a write per
 * character.
 */
function LimitsForm({
  profile,
  busy,
  onApply,
}: {
  profile: ProfileSummary;
  busy: boolean;
  onApply: (quota: ProfileQuota, weight: number) => void;
}) {
  const { t } = useT();
  const [weight, setWeight] = useState(String(profile.fairShareWeight));
  const [runs, setRuns] = useState(profile.quota.maxConcurrentRuns?.toString() ?? "");
  const [memory, setMemory] = useState(megabytes(profile.quota.maxMemoryBytes));
  const [runtime, setRuntime] = useState(
    profile.quota.maxRuntimeMs === null ? "" : String(Math.round(profile.quota.maxRuntimeMs / 1000)),
  );

  return (
    <form
      className="mt-2 flex flex-wrap items-end gap-2 border-t border-border pt-2"
      onSubmit={(event) => {
        event.preventDefault();
        onApply(
          {
            maxConcurrentRuns: parseOptionalNumber(runs),
            maxMemoryBytes: parseOptionalNumber(memory, 1024 * 1024),
            maxRuntimeMs: parseOptionalNumber(runtime, 1000),
          },
          Number(weight) || 1,
        );
      }}
    >
      <label className="flex flex-col gap-1 text-[11px] text-muted">
        {t("ProfilesPanel.weightLabel")}
        <input
          type="number"
          step="0.05"
          min="0.05"
          max="20"
          value={weight}
          onChange={(event) => setWeight(event.target.value)}
          className="h-7 w-20 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
        />
      </label>
      <label className="flex flex-col gap-1 text-[11px] text-muted">
        {t("ProfilesPanel.maxRunsLabel")}
        <input
          type="number"
          min="1"
          value={runs}
          placeholder={t("ProfilesPanel.unbounded")}
          onChange={(event) => setRuns(event.target.value)}
          className="h-7 w-24 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
        />
      </label>
      <label className="flex flex-col gap-1 text-[11px] text-muted">
        {t("ProfilesPanel.maxMemoryLabel")}
        <input
          type="number"
          min="1"
          value={memory}
          placeholder={t("ProfilesPanel.unbounded")}
          onChange={(event) => setMemory(event.target.value)}
          className="h-7 w-28 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
        />
      </label>
      <label className="flex flex-col gap-1 text-[11px] text-muted">
        {t("ProfilesPanel.maxRuntimeLabel")}
        <input
          type="number"
          min="1"
          value={runtime}
          placeholder={t("ProfilesPanel.unbounded")}
          onChange={(event) => setRuntime(event.target.value)}
          className="h-7 w-28 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
        />
      </label>
      <Button size="sm" variant="ghost" type="submit" disabled={busy}>
        {t("ProfilesPanel.applyLimits")}
      </Button>
    </form>
  );
}

/**
 * Settings "Profiles" tab: local multi-profile identity (K23).
 *
 * Each profile is a separate data root — its own sessions, run history,
 * artifacts, packages and keychain items — so switching is a restart, not a
 * re-render: everything the running process holds open belongs to the profile
 * it started under. The confirm below says so, because a silent restart in the
 * middle of a chat is the kind of surprise a settings toggle must not spring.
 */
export function ProfilesPanel() {
  const { t } = useT();
  const [profiles, setProfiles] = useState<ProfileSummary[]>([]);
  const [newName, setNewName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = useCallback(async (operation: () => Promise<ProfileSummary[]>) => {
    setBusy(true);
    try {
      setProfiles(await operation());
      setError(null);
    } catch (caught) {
      setError(String(caught));
    } finally {
      setBusy(false);
    }
  }, []);

  const refresh = useCallback(() => {
    void run(() => invoke<ProfileSummary[]>("profiles_list"));
  }, [run]);

  useEffect(refresh, [refresh]);

  function handleSwitch(profile: ProfileSummary) {
    if (!window.confirm(t("ProfilesPanel.switchConfirm", { name: profile.name }))) return;
    // Never resolves on success — the backend restarts the app.
    void invoke("profiles_switch", { id: profile.id }).catch((caught) => setError(String(caught)));
  }

  function handleDelete(profile: ProfileSummary) {
    if (!window.confirm(t("ProfilesPanel.deleteConfirm", { name: profile.name }))) return;
    void run(() => invoke<ProfileSummary[]>("profiles_delete", { id: profile.id }));
  }

  return (
    <div className="flex flex-col gap-4 py-2">
      <section>
        <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
          {t("ProfilesPanel.heading")}
        </h3>
        <p className="mb-3 text-xs text-muted">{t("ProfilesPanel.description")}</p>

        {error && (
          <div className="mb-3 flex items-center justify-between gap-2 rounded-md bg-danger-soft px-2.5 py-1.5 text-xs text-danger">
            <span>{error}</span>
            <button type="button" onClick={() => setError(null)} className="shrink-0 underline">
              {t("ProfilesPanel.dismissError")}
            </button>
          </div>
        )}

        <div className="flex flex-col gap-2">
          {profiles.map((profile) => (
            <div key={profile.id} className="rounded-lg border border-border bg-background p-3">
              <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
                <div className="flex min-w-0 items-center gap-2">
                  {profile.active && (
                    <UserCheck size={14} className="shrink-0 text-accent" aria-label={t("ProfilesPanel.activeBadge")} />
                  )}
                  <div className="min-w-0">
                    <p className="truncate text-sm font-medium text-foreground">{profile.name}</p>
                    <p className="truncate text-[11px] text-faint" title={profile.root}>
                      {profile.id} · {profile.root}
                    </p>
                  </div>
                  <StatusPill tone={profile.active ? "success" : "neutral"}>
                    {t("ProfilesPanel.share", { percent: Math.round(profile.share * 100) })}
                  </StatusPill>
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  {!profile.active && (
                    <Button size="sm" variant="ghost" onClick={() => handleSwitch(profile)} disabled={busy}>
                      {t("ProfilesPanel.switchButton")}
                    </Button>
                  )}
                  {!profile.active && profile.id !== DEFAULT_PROFILE_ID && (
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => handleDelete(profile)}
                      disabled={busy}
                      className="text-muted hover:text-danger"
                      aria-label={t("ProfilesPanel.deleteButton", { name: profile.name })}
                    >
                      <Trash2 size={12} />
                    </Button>
                  )}
                </div>
              </div>
              <LimitsForm
                profile={profile}
                busy={busy}
                onApply={(quota, fairShareWeight) =>
                  void run(() =>
                    invoke<ProfileSummary[]>("profiles_set_limits", {
                      id: profile.id,
                      quota,
                      fairShareWeight,
                    }),
                  )
                }
              />
            </div>
          ))}
        </div>

        <form
          className="mt-3 flex items-end gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (newName.trim() === "") return;
            void run(async () => {
              const next = await invoke<ProfileSummary[]>("profiles_create", { name: newName });
              setNewName("");
              return next;
            });
          }}
        >
          <label className="flex flex-col gap-1 text-[11px] text-muted">
            {t("ProfilesPanel.newNameLabel")}
            <input
              value={newName}
              onChange={(event) => setNewName(event.target.value)}
              placeholder={t("ProfilesPanel.newNamePlaceholder")}
              className="h-8 w-56 rounded-md border border-border bg-surface px-2 text-sm text-foreground"
            />
          </label>
          <Button size="sm" type="submit" disabled={busy || newName.trim() === ""}>
            <Plus size={12} />
            {t("ProfilesPanel.createButton")}
          </Button>
        </form>
      </section>
    </div>
  );
}
