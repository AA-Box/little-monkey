import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { ChevronDown, Pencil, Plus, Trash2, UsersRound } from "lucide-react";
import { useShallow } from "zustand/react/shallow";

import {
  MAX_CREW_MEMBERS,
  MIN_CREW_MEMBERS,
  normalizeCrewDefinition,
  type CrewActorDefinition,
  type CrewDefinition,
} from "../../lib/crewTypes";
import {
  buildModelTargetInventory,
  findActiveModelTarget,
  type ModelTargetSnapshot,
} from "../../lib/modelTargets";
import { useT } from "../../lib/i18n";
import { useModelStore } from "../../store/modelStore";
import { usePromptStore } from "../../store/promptStore";
import { useSessionStore } from "../../store/sessionStore";
import { Button } from "../ui";

export interface CrewPickerProps {
  value: string | null;
  onChange: (crewId: string | null) => void;
  disabled?: boolean;
}

interface EditorState {
  originalId: string | null;
  name: string;
  coordinator: CrewActorDefinition;
  members: CrewActorDefinition[];
  createdAt: number;
}

function targetLabel(target: ModelTargetSnapshot): string {
  return `${target.label} · ${target.displayName}`;
}

function actorDraft(
  kind: "coordinator" | "member",
  index: number,
  target: ModelTargetSnapshot,
  personaId: string | null,
): CrewActorDefinition {
  return {
    id: crypto.randomUUID(),
    name: kind === "coordinator" ? "Coordinator" : `Member ${index + 1}`,
    role: kind === "coordinator"
      ? "Combine the strongest evidence into one clear answer."
      : "Develop an independent perspective and report only your strongest findings.",
    personaId,
    modelTarget: structuredClone(target),
    contextPolicy: kind === "coordinator" ? "shared_session" : "prompt_only",
    toolProfile: "read_only",
  };
}

export function CrewPicker({ value, onChange, disabled = false }: CrewPickerProps) {
  const { t } = useT();
  const panelId = useId();
  const titleId = useId();
  const containerRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const firstFieldRef = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [editor, setEditor] = useState<EditorState | null>(null);
  const [error, setError] = useState<string | null>(null);

  const crews = useSessionStore((state) => state.crews);
  const saveCrew = useSessionStore((state) => state.saveCrew);
  const removeCrew = useSessionStore((state) => state.removeCrew);
  // Select the store-owned array itself so useSyncExternalStore receives a
  // stable snapshot. Deriving a fresh filtered array inside the selector makes
  // React 19 treat every snapshot read as a store change and can recurse until
  // the nearest error boundary trips.
  const promptEntries = usePromptStore((state) => state.entries);
  const personas = useMemo(
    () => promptEntries.filter((entry) => entry.kind === "persona"),
    [promptEntries],
  );
  const modelState = useModelStore(
    useShallow((state) => ({
      installed: state.installed,
      active: state.active,
      llamaStatus: state.llamaStatus,
      ollamaModels: state.ollamaModels,
      ollamaReachable: state.ollamaReachable,
      providers: state.providers,
      providerModels: state.providerModels,
      effort: state.effort,
      activeProvider: state.activeProvider,
      activeOllamaModel: state.activeOllamaModel,
      activeProviderId: state.activeProviderId,
      activeProviderModel: state.activeProviderModel,
    })),
  );
  const inventory = useMemo(() => buildModelTargetInventory({
    installed: modelState.installed,
    active: modelState.active,
    llamaStatus: modelState.llamaStatus,
    ollamaModels: modelState.ollamaModels,
    ollamaReachable: modelState.ollamaReachable,
    providers: modelState.providers,
    providerModels: modelState.providerModels,
    effort: modelState.effort,
  }), [modelState]);
  const availableTargets = inventory.targets.filter((target) => target.availability.status === "available");
  const selectedCrew = crews.find((crew) => crew.id === value) ?? null;

  const close = useCallback((restoreFocus = false) => {
    setOpen(false);
    setEditor(null);
    setError(null);
    if (restoreFocus) requestAnimationFrame(() => triggerRef.current?.focus());
  }, []);

  useEffect(() => {
    if (!open) return;
    const frame = requestAnimationFrame(() => firstFieldRef.current?.focus());
    const pointer = (event: PointerEvent) => {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) close(false);
    };
    const key = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      close(true);
    };
    window.addEventListener("pointerdown", pointer);
    window.addEventListener("keydown", key);
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener("pointerdown", pointer);
      window.removeEventListener("keydown", key);
    };
  }, [close, open]);

  useEffect(() => {
    if (value !== null && !crews.some((crew) => crew.id === value)) onChange(null);
  }, [crews, onChange, value]);

  function preferredTarget(): ModelTargetSnapshot | null {
    return findActiveModelTarget(inventory, modelState) ?? availableTargets[0] ?? null;
  }

  function beginNew() {
    const target = preferredTarget();
    const personaId = personas[0]?.id ?? null;
    if (!target || !personaId) {
      setError(t("CrewPicker.setupRequired"));
      return;
    }
    setEditor({
      originalId: null,
      name: t("CrewPicker.defaultCrewName"),
      coordinator: actorDraft("coordinator", 0, target, personaId),
      members: [actorDraft("member", 0, target, personaId), actorDraft("member", 1, target, personaId)],
      createdAt: Date.now(),
    });
    setError(null);
  }

  function beginEdit(crew: CrewDefinition) {
    setEditor({
      originalId: crew.id,
      name: crew.name,
      coordinator: structuredClone(crew.coordinator),
      members: structuredClone(crew.members),
      createdAt: crew.createdAt,
    });
    setError(null);
  }

  function updateActor(
    kind: "coordinator" | "member",
    actorId: string,
    patch: Partial<CrewActorDefinition>,
  ) {
    setEditor((current) => {
      if (!current) return current;
      if (kind === "coordinator") {
        return { ...current, coordinator: { ...current.coordinator, ...patch } };
      }
      return {
        ...current,
        members: current.members.map((member) => member.id === actorId ? { ...member, ...patch } : member),
      };
    });
  }

  function addMember() {
    const target = preferredTarget();
    const personaId = personas[0]?.id ?? null;
    if (!target || !personaId) return;
    setEditor((current) => current && current.members.length < MAX_CREW_MEMBERS
      ? { ...current, members: [...current.members, actorDraft("member", current.members.length, target, personaId)] }
      : current);
  }

  function removeMember(actorId: string) {
    setEditor((current) => current && current.members.length > MIN_CREW_MEMBERS
      ? { ...current, members: current.members.filter((member) => member.id !== actorId) }
      : current);
  }

  function saveEditor() {
    if (!editor) return;
    const now = Date.now();
    const candidate: CrewDefinition = {
      version: 1,
      id: editor.originalId ?? crypto.randomUUID(),
      name: editor.name.trim(),
      coordinator: structuredClone(editor.coordinator),
      members: structuredClone(editor.members),
      createdAt: editor.createdAt,
      updatedAt: now,
    };
    const normalized = normalizeCrewDefinition(candidate);
    if (!normalized) {
      setError(t("CrewPicker.validationError"));
      return;
    }
    const id = saveCrew(normalized);
    onChange(id);
    close(true);
  }

  function chooseCrew(crewId: string) {
    onChange(crewId);
    close(true);
  }

  function deleteCrew(crewId: string) {
    removeCrew(crewId);
    if (value === crewId) onChange(null);
  }

  function actorEditor(kind: "coordinator" | "member", actor: CrewActorDefinition, index: number) {
    const targetOptions = inventory.targets.some((target) => target.key === actor.modelTarget.key)
      ? inventory.targets
      : [actor.modelTarget, ...inventory.targets];
    const member = kind === "member";
    return (
      <fieldset key={actor.id} className="rounded-lg border border-border bg-surface/50 p-2.5">
        <legend className="px-1 text-[11px] font-semibold uppercase tracking-wider text-faint">
          {member ? t("CrewPicker.memberLegend", { count: index + 1 }) : t("CrewPicker.coordinatorLegend")}
        </legend>
        <div className="grid grid-cols-2 gap-2">
          <label className="text-[11px] font-medium text-muted">
            {t("CrewPicker.actorNameLabel")}
            <input
              value={actor.name}
              onChange={(event) => updateActor(kind, actor.id, { name: event.target.value })}
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
            />
          </label>
          <label className="text-[11px] font-medium text-muted">
            {t("CrewPicker.personaLabel")}
            <select
              value={actor.personaId ?? ""}
              onChange={(event) => updateActor(kind, actor.id, { personaId: event.target.value || null })}
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
            >
              {!member && <option value="">{t("CrewPicker.noPersona")}</option>}
              {personas.map((persona) => <option key={persona.id} value={persona.id}>{persona.name}</option>)}
            </select>
          </label>
          <label className="col-span-2 text-[11px] font-medium text-muted">
            {t("CrewPicker.roleLabel")}
            <input
              value={actor.role}
              onChange={(event) => updateActor(kind, actor.id, { role: event.target.value })}
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
            />
          </label>
          <label className="text-[11px] font-medium text-muted">
            {t("CrewPicker.modelLabel")}
            <select
              value={actor.modelTarget.key}
              onChange={(event) => {
                const target = targetOptions.find((candidate) => candidate.key === event.target.value);
                if (target) updateActor(kind, actor.id, { modelTarget: structuredClone(target) });
              }}
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
            >
              {targetOptions.map((target) => (
                <option key={target.key} value={target.key} disabled={target.availability.status !== "available"}>
                  {targetLabel(target)}{target.availability.status !== "available" ? ` — ${t("CrewPicker.unavailable")}` : ""}
                </option>
              ))}
            </select>
          </label>
          <label className="text-[11px] font-medium text-muted">
            {t("CrewPicker.contextLabel")}
            <select
              value={actor.contextPolicy}
              onChange={(event) => updateActor(kind, actor.id, {
                contextPolicy: event.target.value as CrewActorDefinition["contextPolicy"],
              })}
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
            >
              <option value="prompt_only">{t("CrewPicker.contextPromptOnly")}</option>
              <option value="shared_session">{t("CrewPicker.contextShared")}</option>
            </select>
          </label>
        </div>
        <div className="mt-2 flex items-center justify-between gap-2">
          <span className="rounded-full border border-border bg-background px-2 py-0.5 text-[10px] font-medium text-muted">
            {t("CrewPicker.readOnlyProfile")}
          </span>
          {member && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={(editor?.members.length ?? 0) <= MIN_CREW_MEMBERS}
              onClick={() => removeMember(actor.id)}
            >
              <Trash2 size={12} aria-hidden="true" />
              {t("CrewPicker.removeMember")}
            </Button>
          )}
        </div>
      </fieldset>
    );
  }

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        ref={triggerRef}
        type="button"
        disabled={disabled}
        onClick={() => open ? close(false) : setOpen(true)}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        className="inline-flex cursor-pointer items-center gap-1.5 rounded-full bg-surface-2 px-2.5 py-1 text-xs font-medium text-muted transition-colors hover:bg-surface hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
      >
        <UsersRound size={13} aria-hidden="true" />
        <span>{selectedCrew?.name ?? t("CrewPicker.trigger")}</span>
        <ChevronDown size={12} aria-hidden="true" className={`transition-transform ${open ? "rotate-180" : ""}`} />
      </button>

      {open && (
        <div
          id={panelId}
          role="dialog"
          aria-labelledby={titleId}
          className="absolute bottom-full left-0 z-40 mb-2 flex max-h-[min(42rem,80vh)] w-[32rem] max-w-[calc(100vw-2rem)] flex-col overflow-hidden rounded-xl border border-border bg-background shadow-2xl"
        >
          <header className="border-b border-border px-3.5 py-3">
            <h2 id={titleId} className="text-sm font-semibold text-foreground">{t("CrewPicker.title")}</h2>
            <p className="mt-0.5 text-xs leading-relaxed text-muted">{t("CrewPicker.description")}</p>
          </header>

          <div className="min-h-0 flex-1 overflow-y-auto p-3 [overscroll-behavior:contain]">
            {editor ? (
              <div className="space-y-2.5">
                <label className="block text-[11px] font-medium text-muted">
                  {t("CrewPicker.crewNameLabel")}
                  <input
                    ref={firstFieldRef}
                    value={editor.name}
                    onChange={(event) => setEditor((current) => current ? { ...current, name: event.target.value } : current)}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
                  />
                </label>
                {actorEditor("coordinator", editor.coordinator, 0)}
                {editor.members.map((member, index) => actorEditor("member", member, index))}
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  onClick={addMember}
                  disabled={editor.members.length >= MAX_CREW_MEMBERS}
                >
                  <Plus size={13} aria-hidden="true" />
                  {t("CrewPicker.addMember")}
                </Button>
              </div>
            ) : crews.length === 0 ? (
              <div className="rounded-lg border border-dashed border-border px-4 py-8 text-center">
                <UsersRound size={24} aria-hidden="true" className="mx-auto text-faint" />
                <p className="mt-2 text-sm font-medium text-foreground">{t("CrewPicker.emptyTitle")}</p>
                <p className="mt-1 text-xs text-muted">{t("CrewPicker.emptyDescription")}</p>
              </div>
            ) : (
              <div className="space-y-1.5">
                {crews.map((crew) => (
                  <div
                    key={crew.id}
                    className={`flex items-center gap-2 rounded-lg border p-2 ${
                      crew.id === value ? "border-accent bg-accent-soft/60" : "border-border bg-surface"
                    }`}
                  >
                    <button
                      type="button"
                      onClick={() => chooseCrew(crew.id)}
                      className="min-w-0 flex-1 cursor-pointer rounded-md px-1 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
                    >
                      <span className="block truncate text-sm font-medium text-foreground">{crew.name}</span>
                      <span className="block truncate text-[11px] text-faint">
                        {t("CrewPicker.crewSummary", { count: crew.members.length, model: crew.coordinator.modelTarget.displayName })}
                      </span>
                    </button>
                    <Button type="button" variant="ghost" size="sm" onClick={() => beginEdit(crew)} aria-label={t("CrewPicker.editCrew", { name: crew.name })}>
                      <Pencil size={13} aria-hidden="true" />
                    </Button>
                    <Button type="button" variant="ghost" size="sm" onClick={() => deleteCrew(crew.id)} aria-label={t("CrewPicker.deleteCrew", { name: crew.name })}>
                      <Trash2 size={13} aria-hidden="true" />
                    </Button>
                  </div>
                ))}
              </div>
            )}
            {error && <p role="alert" className="mt-2 rounded-md border border-danger/40 bg-danger-soft px-2.5 py-2 text-xs text-danger">{error}</p>}
          </div>

          <footer className="flex items-center justify-between gap-2 border-t border-border bg-surface/30 px-3.5 py-3">
            {editor ? (
              <>
                <Button type="button" variant="ghost" size="sm" onClick={() => { setEditor(null); setError(null); }}>
                  {t("CrewPicker.back")}
                </Button>
                <Button type="button" variant="primary" size="sm" onClick={saveEditor}>
                  {t("CrewPicker.save")}
                </Button>
              </>
            ) : (
              <>
                <Button type="button" variant="ghost" size="sm" onClick={() => { onChange(null); close(true); }}>
                  {t("CrewPicker.normalChat")}
                </Button>
                <Button type="button" variant="primary" size="sm" onClick={beginNew}>
                  <Plus size={13} aria-hidden="true" />
                  {t("CrewPicker.newCrew")}
                </Button>
              </>
            )}
          </footer>
        </div>
      )}
    </div>
  );
}

export default CrewPicker;
