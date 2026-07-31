import { create } from "zustand";

import {
  addIncidentAlert,
  addIncidentDecision,
  addIncidentEvidence,
  addIncidentMitigation,
  addIncidentOwner,
  addIncidentRunbookStep,
  addIncidentStatusDraft,
  addIncidentTimelineNote,
  createIncident,
  isIncidentRecord,
  recordIncidentApproval,
  transitionIncidentStatus,
  updateIncidentAlertStatus,
  updateIncidentDetails,
  updateIncidentMitigationStatus,
  updateIncidentPostmortem,
  updateIncidentRunbookStep,
  updateIncidentStatusDraft,
  type IncidentActionClass,
  type IncidentAlertSeverity,
  type IncidentAlertStatus,
  type IncidentDecision,
  type IncidentDraftAudience,
  type IncidentEvidenceKind,
  type IncidentMitigationStatus,
  type IncidentOwnerRole,
  type IncidentPostmortemDraft,
  type IncidentRecord,
  type IncidentRisk,
  type IncidentRunbookStepStatus,
  type IncidentSeverity,
  type IncidentStatus,
} from "../lib/incidentCommander";
import { errorMessage } from "../lib/errors";
import { hydrateState, persistState } from "../lib/persistedState";

export const INCIDENT_COMMANDER_STORAGE_KEY = "little-monkey-incident-commander-v1";
const STORAGE_VERSION = 1;

interface IncidentCommanderState {
  incidents: IncidentRecord[];
  selectedIncidentId: string | null;
  error: string | null;
  init: () => void;
  clearError: () => void;
  selectIncident: (incidentId: string | null) => void;
  createIncident: (input: { title: string; severity?: IncidentSeverity; summary?: string; impact?: string; service?: string }) => IncidentRecord;
  deleteIncident: (incidentId: string) => void;
  updateIncident: (incidentId: string, patch: Partial<Pick<IncidentRecord, "title" | "severity" | "summary" | "impact" | "service" | "startedAtMs">>) => void;
  transitionStatus: (incidentId: string, status: IncidentStatus, actor: string, note?: string) => void;
  addOwner: (incidentId: string, input: { name: string; role: IncidentOwnerRole; responsibility: string; actor?: string }) => void;
  addAlert: (incidentId: string, input: { title: string; source: string; severity: IncidentAlertSeverity; description: string; firedAtMs?: number; actor?: string }) => void;
  updateAlertStatus: (incidentId: string, alertId: string, status: IncidentAlertStatus, actor: string) => void;
  addEvidence: (incidentId: string, input: { kind: IncidentEvidenceKind; title: string; sourceUri: string; content: string; observedAtMs?: number; actor?: string }) => void;
  addMitigation: (incidentId: string, input: { title: string; description: string; ownerId: string | null; risk: IncidentRisk; actionClass: IncidentActionClass; actor?: string }) => void;
  recordApproval: (incidentId: string, mitigationId: string, decision: "approved" | "rejected", decidedBy: string, note?: string) => void;
  updateMitigationStatus: (incidentId: string, mitigationId: string, status: IncidentMitigationStatus, actor: string, verification?: string) => void;
  addDecision: (incidentId: string, input: Omit<IncidentDecision, "id" | "decidedAtMs"> & { alternatives: string[]; evidenceIds: string[]; decidedAtMs?: number; actor?: string }) => void;
  addRunbookStep: (incidentId: string, input: { title: string; instructions: string; ownerId: string | null; actor?: string }) => void;
  updateRunbookStep: (incidentId: string, stepId: string, status: IncidentRunbookStepStatus, actor: string, verification?: string) => void;
  addStatusDraft: (incidentId: string, input: { audience: IncidentDraftAudience; title: string; body: string; actor?: string }) => void;
  updateStatusDraft: (incidentId: string, draftId: string, patch: { audience?: IncidentDraftAudience; title?: string; body?: string; state?: "draft" | "superseded" }, actor: string) => void;
  updatePostmortem: (incidentId: string, patch: Partial<Omit<IncidentPostmortemDraft, "updatedAtMs">>) => void;
  addTimelineNote: (incidentId: string, title: string, detail: string, actor: string, occurredAtMs?: number) => void;
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

function persist(incidents: readonly IncidentRecord[]): void {
  persistState(INCIDENT_COMMANDER_STORAGE_KEY, STORAGE_VERSION, { incidents });
}

function hydrate(): IncidentRecord[] {
  const raw = hydrateState(INCIDENT_COMMANDER_STORAGE_KEY, STORAGE_VERSION);
  if (!raw || !Array.isArray(raw.incidents)) return [];
  return raw.incidents.filter(isIncidentRecord).sort((left, right) => right.updatedAtMs - left.updatedAtMs);
}

const initiallyHydrated = hydrate();

export const useIncidentCommanderStore = create<IncidentCommanderState>((set, get) => {
  const replace = (next: IncidentRecord) => {
    const incidents = [next, ...get().incidents.filter((incident) => incident.id !== next.id)]
      .sort((left, right) => right.updatedAtMs - left.updatedAtMs);
    persist(incidents);
    set({ incidents, error: null });
  };

  const mutate = (incidentId: string, operation: (incident: IncidentRecord) => IncidentRecord) => {
    const incident = get().incidents.find((candidate) => candidate.id === incidentId);
    if (!incident) {
      set({ error: "This incident no longer exists." });
      return;
    }
    try {
      replace(operation(incident));
    } catch (error) {
      set({ error: errorText(error) });
    }
  };

  return {
    incidents: initiallyHydrated,
    selectedIncidentId: initiallyHydrated[0]?.id ?? null,
    error: null,

    init: () => {
      const incidents = hydrate();
      persist(incidents);
      set((state) => ({
        incidents,
        selectedIncidentId: state.selectedIncidentId && incidents.some((incident) => incident.id === state.selectedIncidentId)
          ? state.selectedIncidentId
          : incidents[0]?.id ?? null,
        error: null,
      }));
    },

    clearError: () => set({ error: null }),
    selectIncident: (selectedIncidentId) => set({ selectedIncidentId, error: null }),

    createIncident: (input) => {
      try {
        const incident = createIncident(input);
        const incidents = [incident, ...get().incidents];
        persist(incidents);
        set({ incidents, selectedIncidentId: incident.id, error: null });
        return incident;
      } catch (error) {
        set({ error: errorText(error) });
        throw error;
      }
    },

    deleteIncident: (incidentId) => {
      const incidents = get().incidents.filter((incident) => incident.id !== incidentId);
      persist(incidents);
      set((state) => ({
        incidents,
        selectedIncidentId: state.selectedIncidentId === incidentId ? incidents[0]?.id ?? null : state.selectedIncidentId,
        error: null,
      }));
    },

    updateIncident: (incidentId, patch) => mutate(incidentId, (incident) => updateIncidentDetails(incident, patch)),
    transitionStatus: (incidentId, status, actor, note) => mutate(incidentId, (incident) => transitionIncidentStatus(incident, status, actor, note)),
    addOwner: (incidentId, input) => mutate(incidentId, (incident) => addIncidentOwner(incident, {
      name: input.name,
      role: input.role,
      responsibility: input.responsibility,
      actor: input.actor,
    })),
    addAlert: (incidentId, input) => mutate(incidentId, (incident) => addIncidentAlert(incident, {
      title: input.title,
      source: input.source,
      severity: input.severity,
      description: input.description,
      firedAtMs: input.firedAtMs ?? Date.now(),
      actor: input.actor,
    })),
    updateAlertStatus: (incidentId, alertId, status, actor) => mutate(incidentId, (incident) => updateIncidentAlertStatus(incident, alertId, status, actor)),
    addEvidence: (incidentId, input) => mutate(incidentId, (incident) => addIncidentEvidence(incident, {
      kind: input.kind,
      title: input.title,
      sourceUri: input.sourceUri,
      content: input.content,
      observedAtMs: input.observedAtMs ?? Date.now(),
      actor: input.actor,
    })),
    addMitigation: (incidentId, input) => mutate(incidentId, (incident) => addIncidentMitigation(incident, input)),
    recordApproval: (incidentId, mitigationId, decision, decidedBy, note) => mutate(incidentId, (incident) => recordIncidentApproval(incident, mitigationId, decision, decidedBy, note)),
    updateMitigationStatus: (incidentId, mitigationId, status, actor, verification) => mutate(incidentId, (incident) => updateIncidentMitigationStatus(incident, mitigationId, status, actor, verification)),
    addDecision: (incidentId, input) => mutate(incidentId, (incident) => addIncidentDecision(incident, {
      ...input,
      decidedAtMs: input.decidedAtMs ?? Date.now(),
    })),
    addRunbookStep: (incidentId, input) => mutate(incidentId, (incident) => addIncidentRunbookStep(incident, input)),
    updateRunbookStep: (incidentId, stepId, status, actor, verification) => mutate(incidentId, (incident) => updateIncidentRunbookStep(incident, stepId, status, actor, verification)),
    addStatusDraft: (incidentId, input) => mutate(incidentId, (incident) => addIncidentStatusDraft(incident, input)),
    updateStatusDraft: (incidentId, draftId, patch, actor) => mutate(incidentId, (incident) => updateIncidentStatusDraft(incident, draftId, patch, actor)),
    updatePostmortem: (incidentId, patch) => mutate(incidentId, (incident) => updateIncidentPostmortem(incident, patch)),
    addTimelineNote: (incidentId, title, detail, actor, occurredAtMs) => mutate(incidentId, (incident) => addIncidentTimelineNote(incident, title, detail, actor, occurredAtMs)),
  };
});

export function __resetIncidentCommanderStoreForTests(): void {
  try {
    localStorage.removeItem(INCIDENT_COMMANDER_STORAGE_KEY);
  } catch {
    // no-op
  }
  useIncidentCommanderStore.setState({ incidents: [], selectedIncidentId: null, error: null });
}

