import { redactSensitiveText } from "./durableRun";

export const INCIDENT_SCHEMA_VERSION = 1 as const;
export const MAX_INCIDENT_ITEMS = 200;
export const MAX_INCIDENT_EVIDENCE_CHARS = 16_000;
export const INCIDENT_SAFETY_NOTICE =
  "Coordination record only. Little Monkey does not send status updates or execute customer-facing, destructive, or infrastructure-changing actions from Incident Commander.";

export type IncidentSeverity = "sev0" | "sev1" | "sev2" | "sev3";
export type IncidentStatus = "declared" | "investigating" | "mitigating" | "monitoring" | "resolved" | "closed";
export type IncidentOwnerRole = "commander" | "technical" | "communications" | "operations" | "observer";
export type IncidentAlertSeverity = "critical" | "warning" | "info";
export type IncidentAlertStatus = "firing" | "acknowledged" | "resolved";
export type IncidentEvidenceKind = "alert" | "log" | "trace" | "dashboard" | "ticket" | "runbook" | "release" | "note";
export type IncidentRisk = "low" | "medium" | "high" | "critical";
export type IncidentActionClass = "read_only" | "customer_facing" | "destructive" | "infrastructure_change";
export type IncidentApprovalState = "not_required" | "pending" | "approved" | "rejected";
export type IncidentMitigationStatus = "proposed" | "in_progress_external" | "monitoring" | "verified" | "failed" | "rejected" | "cancelled";
export type IncidentRunbookStepStatus = "pending" | "in_progress_external" | "completed_external" | "skipped";
export type IncidentDraftAudience = "internal" | "customer" | "executive" | "engineering";
export type IncidentTimelineKind =
  | "incident_declared"
  | "status_changed"
  | "owner_assigned"
  | "alert_added"
  | "alert_changed"
  | "evidence_added"
  | "mitigation_proposed"
  | "approval_recorded"
  | "mitigation_changed"
  | "decision_recorded"
  | "runbook_changed"
  | "status_draft_changed"
  | "note";

export interface IncidentOwner {
  id: string;
  name: string;
  role: IncidentOwnerRole;
  responsibility: string;
  active: boolean;
  assignedAtMs: number;
}

export interface IncidentAlert {
  id: string;
  title: string;
  source: string;
  severity: IncidentAlertSeverity;
  status: IncidentAlertStatus;
  description: string;
  firedAtMs: number;
  updatedAtMs: number;
}

export interface IncidentEvidence {
  id: string;
  kind: IncidentEvidenceKind;
  title: string;
  sourceUri: string;
  content: string;
  observedAtMs: number;
  addedAtMs: number;
}

export interface IncidentApproval {
  state: IncidentApprovalState;
  requestedAtMs: number | null;
  decidedAtMs: number | null;
  decidedBy: string | null;
  note: string;
}

export interface IncidentMitigation {
  id: string;
  title: string;
  description: string;
  ownerId: string | null;
  risk: IncidentRisk;
  actionClass: IncidentActionClass;
  status: IncidentMitigationStatus;
  approval: IncidentApproval;
  verification: string;
  createdAtMs: number;
  updatedAtMs: number;
  executionMode: "manual_external_only";
}

export interface IncidentDecision {
  id: string;
  title: string;
  decision: string;
  rationale: string;
  ownerId: string | null;
  alternatives: string[];
  evidenceIds: string[];
  decidedAtMs: number;
}

export interface IncidentRunbookStep {
  id: string;
  title: string;
  instructions: string;
  ownerId: string | null;
  status: IncidentRunbookStepStatus;
  verification: string;
  updatedAtMs: number;
  executionMode: "manual_external_only";
}

export interface IncidentStatusUpdateDraft {
  id: string;
  audience: IncidentDraftAudience;
  title: string;
  body: string;
  state: "draft" | "superseded";
  draftOnly: true;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface IncidentTimelineEntry {
  id: string;
  kind: IncidentTimelineKind;
  title: string;
  detail: string;
  actor: string;
  relatedId: string | null;
  occurredAtMs: number;
}

export interface IncidentPostmortemDraft {
  executiveSummary: string;
  impact: string;
  detection: string;
  rootCause: string;
  contributingFactors: string;
  resolution: string;
  whatWentWell: string;
  whatWentPoorly: string;
  followUpActions: string;
  updatedAtMs: number;
}

export interface IncidentRecord {
  schemaVersion: typeof INCIDENT_SCHEMA_VERSION;
  id: string;
  revision: number;
  title: string;
  severity: IncidentSeverity;
  status: IncidentStatus;
  summary: string;
  impact: string;
  service: string;
  startedAtMs: number;
  resolvedAtMs: number | null;
  createdAtMs: number;
  updatedAtMs: number;
  owners: IncidentOwner[];
  alerts: IncidentAlert[];
  evidence: IncidentEvidence[];
  mitigations: IncidentMitigation[];
  decisions: IncidentDecision[];
  runbook: IncidentRunbookStep[];
  statusUpdateDrafts: IncidentStatusUpdateDraft[];
  timeline: IncidentTimelineEntry[];
  postmortem: IncidentPostmortemDraft;
}

export interface IncidentActionPolicy {
  requiresHumanApproval: boolean;
  executionMode: "manual_external_only";
  reason: string;
}

export interface IncidentCompleteness {
  complete: boolean;
  missing: string[];
}

function id(prefix: string): string {
  return `${prefix}-${crypto.randomUUID()}`;
}

function text(value: string, max: number, fallback = ""): string {
  return redactSensitiveText(value.trim()).slice(0, max) || fallback;
}

function list(value: readonly string[], maxItems = 20, maxChars = 1_000): string[] {
  return [...new Set(value.map((item) => text(item, maxChars)).filter(Boolean))].slice(0, maxItems);
}

function ensureRoom(items: readonly unknown[], label: string): void {
  if (items.length >= MAX_INCIDENT_ITEMS) throw new Error(`${label} reached the local limit of ${MAX_INCIDENT_ITEMS} entries.`);
}

function timelineEntry(input: Omit<IncidentTimelineEntry, "id"> & { id?: string }): IncidentTimelineEntry {
  return {
    id: input.id ?? id("incident-event"),
    kind: input.kind,
    title: text(input.title, 300, "Incident update"),
    detail: text(input.detail, 4_000),
    actor: text(input.actor, 200, "Local user"),
    relatedId: input.relatedId,
    occurredAtMs: input.occurredAtMs,
  };
}

function changed(
  incident: IncidentRecord,
  patch: Partial<IncidentRecord>,
  event: Omit<IncidentTimelineEntry, "id">,
  now: number,
): IncidentRecord {
  ensureRoom(incident.timeline, "Incident timeline");
  return {
    ...incident,
    ...patch,
    revision: incident.revision + 1,
    updatedAtMs: now,
    timeline: [...incident.timeline, timelineEntry(event)],
  };
}

export function actionPolicy(actionClass: IncidentActionClass, risk: IncidentRisk): IncidentActionPolicy {
  const requiresHumanApproval = actionClass !== "read_only" || risk === "high" || risk === "critical";
  const reason = actionClass === "customer_facing"
    ? "Customer-facing actions require explicit human approval and manual execution outside this feature."
    : actionClass === "destructive"
      ? "Destructive actions require explicit human approval and manual execution outside this feature."
      : actionClass === "infrastructure_change"
        ? "Infrastructure changes require explicit human approval and manual execution outside this feature."
        : requiresHumanApproval
          ? `${risk} risk requires explicit human approval and manual execution outside this feature.`
          : "Read-only, low/medium-risk coordination does not require an approval record, but execution remains manual and external.";
  return { requiresHumanApproval, executionMode: "manual_external_only", reason };
}

export function createIncident(input: {
  title: string;
  severity?: IncidentSeverity;
  summary?: string;
  impact?: string;
  service?: string;
  startedAtMs?: number;
  now?: number;
  id?: string;
  actor?: string;
}): IncidentRecord {
  const now = input.now ?? Date.now();
  const title = text(input.title, 300);
  if (!title) throw new Error("Enter an incident title.");
  const incidentId = input.id ?? id("incident");
  return {
    schemaVersion: INCIDENT_SCHEMA_VERSION,
    id: incidentId,
    revision: 1,
    title,
    severity: input.severity ?? "sev2",
    status: "declared",
    summary: text(input.summary ?? "", 8_000),
    impact: text(input.impact ?? "", 8_000),
    service: text(input.service ?? "", 300),
    startedAtMs: input.startedAtMs ?? now,
    resolvedAtMs: null,
    createdAtMs: now,
    updatedAtMs: now,
    owners: [],
    alerts: [],
    evidence: [],
    mitigations: [],
    decisions: [],
    runbook: [],
    statusUpdateDrafts: [],
    timeline: [timelineEntry({
      kind: "incident_declared",
      title: `Incident declared: ${title}`,
      detail: text(input.summary ?? "", 4_000),
      actor: input.actor ?? "Local user",
      relatedId: incidentId,
      occurredAtMs: now,
    })],
    postmortem: {
      executiveSummary: "",
      impact: text(input.impact ?? "", 8_000),
      detection: "",
      rootCause: "",
      contributingFactors: "",
      resolution: "",
      whatWentWell: "",
      whatWentPoorly: "",
      followUpActions: "",
      updatedAtMs: now,
    },
  };
}

export function updateIncidentDetails(
  incident: IncidentRecord,
  patch: Partial<Pick<IncidentRecord, "title" | "severity" | "summary" | "impact" | "service" | "startedAtMs">>,
  now = Date.now(),
): IncidentRecord {
  const title = patch.title === undefined ? incident.title : text(patch.title, 300);
  if (!title) throw new Error("Incident title cannot be empty.");
  return {
    ...incident,
    ...patch,
    title,
    summary: patch.summary === undefined ? incident.summary : text(patch.summary, 8_000),
    impact: patch.impact === undefined ? incident.impact : text(patch.impact, 8_000),
    service: patch.service === undefined ? incident.service : text(patch.service, 300),
    revision: incident.revision + 1,
    updatedAtMs: now,
  };
}

const STATUS_TRANSITIONS: Record<IncidentStatus, readonly IncidentStatus[]> = {
  declared: ["investigating", "mitigating", "resolved"],
  investigating: ["mitigating", "monitoring", "resolved"],
  mitigating: ["investigating", "monitoring", "resolved"],
  monitoring: ["investigating", "mitigating", "resolved"],
  resolved: ["investigating", "closed"],
  closed: ["investigating"],
};

export function availableIncidentStatuses(status: IncidentStatus): readonly IncidentStatus[] {
  return [status, ...STATUS_TRANSITIONS[status]];
}

export function transitionIncidentStatus(
  incident: IncidentRecord,
  status: IncidentStatus,
  actor: string,
  note = "",
  now = Date.now(),
): IncidentRecord {
  if (status === incident.status) return incident;
  if (!STATUS_TRANSITIONS[incident.status].includes(status)) {
    throw new Error(`Cannot move an incident from ${incident.status} to ${status}.`);
  }
  return changed(incident, {
    status,
    resolvedAtMs: status === "resolved" || status === "closed" ? incident.resolvedAtMs ?? now : null,
  }, {
    kind: "status_changed",
    title: `Incident status changed to ${status}`,
    detail: text(note, 4_000),
    actor,
    relatedId: incident.id,
    occurredAtMs: now,
  }, now);
}

export function addIncidentOwner(
  incident: IncidentRecord,
  input: Omit<IncidentOwner, "id" | "assignedAtMs" | "active"> & { id?: string; assignedAtMs?: number; active?: boolean; actor?: string },
): IncidentRecord {
  ensureRoom(incident.owners, "Incident owners");
  const now = input.assignedAtMs ?? Date.now();
  const name = text(input.name, 200);
  if (!name) throw new Error("Enter an owner name.");
  const owner: IncidentOwner = {
    id: input.id ?? id("incident-owner"),
    name,
    role: input.role,
    responsibility: text(input.responsibility, 1_000),
    active: input.active ?? true,
    assignedAtMs: now,
  };
  return changed(incident, { owners: [...incident.owners, owner] }, {
    kind: "owner_assigned",
    title: `${owner.name} assigned as ${owner.role}`,
    detail: owner.responsibility,
    actor: input.actor ?? owner.name,
    relatedId: owner.id,
    occurredAtMs: now,
  }, now);
}

export function addIncidentAlert(
  incident: IncidentRecord,
  input: Omit<IncidentAlert, "id" | "status" | "updatedAtMs"> & { id?: string; status?: IncidentAlertStatus; actor?: string },
): IncidentRecord {
  ensureRoom(incident.alerts, "Incident alerts");
  const title = text(input.title, 300);
  if (!title) throw new Error("Enter an alert title.");
  const alert: IncidentAlert = {
    id: input.id ?? id("incident-alert"),
    title,
    source: text(input.source, 500, "manual"),
    severity: input.severity,
    status: input.status ?? "firing",
    description: text(input.description, 4_000),
    firedAtMs: input.firedAtMs,
    updatedAtMs: input.firedAtMs,
  };
  return changed(incident, { alerts: [...incident.alerts, alert] }, {
    kind: "alert_added",
    title: `Alert added: ${alert.title}`,
    detail: `${alert.source} · ${alert.severity} · ${alert.status}`,
    actor: input.actor ?? "Local user",
    relatedId: alert.id,
    occurredAtMs: alert.firedAtMs,
  }, alert.firedAtMs);
}

export function updateIncidentAlertStatus(
  incident: IncidentRecord,
  alertId: string,
  status: IncidentAlertStatus,
  actor: string,
  now = Date.now(),
): IncidentRecord {
  const alert = incident.alerts.find((candidate) => candidate.id === alertId);
  if (!alert) throw new Error("Incident alert was not found.");
  if (alert.status === status) return incident;
  return changed(incident, {
    alerts: incident.alerts.map((candidate) => candidate.id === alertId ? { ...candidate, status, updatedAtMs: now } : candidate),
  }, {
    kind: "alert_changed",
    title: `Alert ${alert.title} changed to ${status}`,
    detail: alert.source,
    actor,
    relatedId: alertId,
    occurredAtMs: now,
  }, now);
}

export function addIncidentEvidence(
  incident: IncidentRecord,
  input: Omit<IncidentEvidence, "id" | "addedAtMs"> & { id?: string; addedAtMs?: number; actor?: string },
): IncidentRecord {
  ensureRoom(incident.evidence, "Incident evidence");
  const now = input.addedAtMs ?? Date.now();
  const title = text(input.title, 300);
  if (!title) throw new Error("Enter an evidence title.");
  const evidence: IncidentEvidence = {
    id: input.id ?? id("incident-evidence"),
    kind: input.kind,
    title,
    sourceUri: text(input.sourceUri, 2_000, "local://incident-evidence"),
    content: text(input.content, MAX_INCIDENT_EVIDENCE_CHARS),
    observedAtMs: input.observedAtMs,
    addedAtMs: now,
  };
  if (!evidence.content) throw new Error("Enter evidence content.");
  return changed(incident, { evidence: [...incident.evidence, evidence] }, {
    kind: "evidence_added",
    title: `Evidence added: ${evidence.title}`,
    detail: `${evidence.kind} · ${evidence.sourceUri}`,
    actor: input.actor ?? "Local user",
    relatedId: evidence.id,
    occurredAtMs: now,
  }, now);
}

export function addIncidentMitigation(
  incident: IncidentRecord,
  input: Omit<IncidentMitigation, "id" | "status" | "approval" | "verification" | "createdAtMs" | "updatedAtMs" | "executionMode"> & {
    id?: string;
    createdAtMs?: number;
    actor?: string;
  },
): IncidentRecord {
  ensureRoom(incident.mitigations, "Incident mitigations");
  const now = input.createdAtMs ?? Date.now();
  const title = text(input.title, 300);
  if (!title) throw new Error("Enter a mitigation title.");
  const policy = actionPolicy(input.actionClass, input.risk);
  const mitigation: IncidentMitigation = {
    id: input.id ?? id("incident-mitigation"),
    title,
    description: text(input.description, 6_000),
    ownerId: input.ownerId && incident.owners.some((owner) => owner.id === input.ownerId) ? input.ownerId : null,
    risk: input.risk,
    actionClass: input.actionClass,
    status: "proposed",
    approval: {
      state: policy.requiresHumanApproval ? "pending" : "not_required",
      requestedAtMs: policy.requiresHumanApproval ? now : null,
      decidedAtMs: null,
      decidedBy: null,
      note: policy.reason,
    },
    verification: "",
    createdAtMs: now,
    updatedAtMs: now,
    executionMode: "manual_external_only",
  };
  return changed(incident, { mitigations: [...incident.mitigations, mitigation] }, {
    kind: "mitigation_proposed",
    title: `Mitigation proposed: ${mitigation.title}`,
    detail: `${mitigation.actionClass} · ${mitigation.risk} risk · ${mitigation.approval.state}`,
    actor: input.actor ?? "Local user",
    relatedId: mitigation.id,
    occurredAtMs: now,
  }, now);
}

export function recordIncidentApproval(
  incident: IncidentRecord,
  mitigationId: string,
  decision: "approved" | "rejected",
  decidedBy: string,
  note = "",
  now = Date.now(),
): IncidentRecord {
  const mitigation = incident.mitigations.find((candidate) => candidate.id === mitigationId);
  if (!mitigation) throw new Error("Incident mitigation was not found.");
  const actor = text(decidedBy, 200);
  if (!actor) throw new Error("Record who made the approval decision.");
  const policy = actionPolicy(mitigation.actionClass, mitigation.risk);
  if (!policy.requiresHumanApproval) throw new Error("This mitigation does not require an approval record.");
  if (mitigation.approval.state !== "pending") throw new Error("This mitigation no longer has a pending approval request.");
  return changed(incident, {
    mitigations: incident.mitigations.map((candidate) => candidate.id === mitigationId ? {
      ...candidate,
      status: decision === "rejected" ? "rejected" : candidate.status,
      approval: {
        ...candidate.approval,
        state: decision,
        decidedAtMs: now,
        decidedBy: actor,
        note: text(note, 4_000, policy.reason),
      },
      updatedAtMs: now,
    } : candidate),
  }, {
    kind: "approval_recorded",
    title: `Mitigation ${decision}: ${mitigation.title}`,
    detail: text(note, 4_000, policy.reason),
    actor,
    relatedId: mitigationId,
    occurredAtMs: now,
  }, now);
}

const ACTIVE_MITIGATION_STATUSES: ReadonlySet<IncidentMitigationStatus> = new Set([
  "in_progress_external", "monitoring", "verified", "failed",
]);

export function updateIncidentMitigationStatus(
  incident: IncidentRecord,
  mitigationId: string,
  status: IncidentMitigationStatus,
  actor: string,
  verification = "",
  now = Date.now(),
): IncidentRecord {
  const mitigation = incident.mitigations.find((candidate) => candidate.id === mitigationId);
  if (!mitigation) throw new Error("Incident mitigation was not found.");
  const policy = actionPolicy(mitigation.actionClass, mitigation.risk);
  if (mitigation.approval.state === "rejected" && status !== "rejected" && status !== "cancelled") {
    throw new Error("A rejected mitigation can only remain rejected or be cancelled.");
  }
  if (policy.requiresHumanApproval && ACTIVE_MITIGATION_STATUSES.has(status) && mitigation.approval.state !== "approved") {
    throw new Error("Record explicit human approval before recording this external mitigation as started or completed.");
  }
  return changed(incident, {
    mitigations: incident.mitigations.map((candidate) => candidate.id === mitigationId ? {
      ...candidate,
      status,
      verification: text(verification, 6_000, candidate.verification),
      updatedAtMs: now,
    } : candidate),
  }, {
    kind: "mitigation_changed",
    title: `Mitigation recorded as ${status}: ${mitigation.title}`,
    detail: text(verification, 4_000, "State recorded from an action performed outside Incident Commander."),
    actor,
    relatedId: mitigationId,
    occurredAtMs: now,
  }, now);
}

export function addIncidentDecision(
  incident: IncidentRecord,
  input: Omit<IncidentDecision, "id"> & { id?: string; actor?: string },
): IncidentRecord {
  ensureRoom(incident.decisions, "Incident decisions");
  const title = text(input.title, 300);
  const decisionText = text(input.decision, 6_000);
  if (!title || !decisionText) throw new Error("Enter a decision title and decision.");
  const decision: IncidentDecision = {
    id: input.id ?? id("incident-decision"),
    title,
    decision: decisionText,
    rationale: text(input.rationale, 6_000),
    ownerId: input.ownerId && incident.owners.some((owner) => owner.id === input.ownerId) ? input.ownerId : null,
    alternatives: list(input.alternatives),
    evidenceIds: [...new Set(input.evidenceIds.filter((evidenceId) => incident.evidence.some((evidence) => evidence.id === evidenceId)))],
    decidedAtMs: input.decidedAtMs,
  };
  return changed(incident, { decisions: [...incident.decisions, decision] }, {
    kind: "decision_recorded",
    title: `Decision recorded: ${decision.title}`,
    detail: decision.decision,
    actor: input.actor ?? "Local user",
    relatedId: decision.id,
    occurredAtMs: decision.decidedAtMs,
  }, decision.decidedAtMs);
}

export function addIncidentRunbookStep(
  incident: IncidentRecord,
  input: Omit<IncidentRunbookStep, "id" | "status" | "verification" | "updatedAtMs" | "executionMode"> & { id?: string; now?: number; actor?: string },
): IncidentRecord {
  ensureRoom(incident.runbook, "Incident runbook");
  const now = input.now ?? Date.now();
  const title = text(input.title, 300);
  if (!title) throw new Error("Enter a runbook step title.");
  const step: IncidentRunbookStep = {
    id: input.id ?? id("incident-runbook-step"),
    title,
    instructions: text(input.instructions, 6_000),
    ownerId: input.ownerId && incident.owners.some((owner) => owner.id === input.ownerId) ? input.ownerId : null,
    status: "pending",
    verification: "",
    updatedAtMs: now,
    executionMode: "manual_external_only",
  };
  return changed(incident, { runbook: [...incident.runbook, step] }, {
    kind: "runbook_changed",
    title: `Runbook step added: ${step.title}`,
    detail: "Execution remains manual and external.",
    actor: input.actor ?? "Local user",
    relatedId: step.id,
    occurredAtMs: now,
  }, now);
}

export function updateIncidentRunbookStep(
  incident: IncidentRecord,
  stepId: string,
  status: IncidentRunbookStepStatus,
  actor: string,
  verification = "",
  now = Date.now(),
): IncidentRecord {
  const step = incident.runbook.find((candidate) => candidate.id === stepId);
  if (!step) throw new Error("Incident runbook step was not found.");
  return changed(incident, {
    runbook: incident.runbook.map((candidate) => candidate.id === stepId ? {
      ...candidate,
      status,
      verification: text(verification, 6_000, candidate.verification),
      updatedAtMs: now,
    } : candidate),
  }, {
    kind: "runbook_changed",
    title: `Runbook step recorded as ${status}: ${step.title}`,
    detail: text(verification, 4_000, "State recorded from work performed outside Incident Commander."),
    actor,
    relatedId: stepId,
    occurredAtMs: now,
  }, now);
}

export function addIncidentStatusDraft(
  incident: IncidentRecord,
  input: Pick<IncidentStatusUpdateDraft, "audience" | "title" | "body"> & { id?: string; now?: number; actor?: string },
): IncidentRecord {
  ensureRoom(incident.statusUpdateDrafts, "Status update drafts");
  const now = input.now ?? Date.now();
  const title = text(input.title, 300);
  const body = text(input.body, 12_000);
  if (!title || !body) throw new Error("Enter a status-update title and body.");
  const draft: IncidentStatusUpdateDraft = {
    id: input.id ?? id("incident-status-draft"),
    audience: input.audience,
    title,
    body,
    state: "draft",
    draftOnly: true,
    createdAtMs: now,
    updatedAtMs: now,
  };
  return changed(incident, { statusUpdateDrafts: [...incident.statusUpdateDrafts, draft] }, {
    kind: "status_draft_changed",
    title: `Draft status update created for ${draft.audience}`,
    detail: `${draft.title} · DRAFT ONLY, NOT SENT`,
    actor: input.actor ?? "Local user",
    relatedId: draft.id,
    occurredAtMs: now,
  }, now);
}

export function updateIncidentStatusDraft(
  incident: IncidentRecord,
  draftId: string,
  patch: Partial<Pick<IncidentStatusUpdateDraft, "audience" | "title" | "body" | "state">>,
  actor: string,
  now = Date.now(),
): IncidentRecord {
  const draft = incident.statusUpdateDrafts.find((candidate) => candidate.id === draftId);
  if (!draft) throw new Error("Status update draft was not found.");
  const next = {
    ...draft,
    ...patch,
    title: patch.title === undefined ? draft.title : text(patch.title, 300),
    body: patch.body === undefined ? draft.body : text(patch.body, 12_000),
    draftOnly: true as const,
    updatedAtMs: now,
  };
  if (!next.title || !next.body) throw new Error("A status update draft cannot have an empty title or body.");
  return changed(incident, {
    statusUpdateDrafts: incident.statusUpdateDrafts.map((candidate) => candidate.id === draftId ? next : candidate),
  }, {
    kind: "status_draft_changed",
    title: `Status update draft ${next.state}`,
    detail: `${next.title} · DRAFT ONLY, NOT SENT`,
    actor,
    relatedId: draftId,
    occurredAtMs: now,
  }, now);
}

export function updateIncidentPostmortem(
  incident: IncidentRecord,
  patch: Partial<Omit<IncidentPostmortemDraft, "updatedAtMs">>,
  now = Date.now(),
): IncidentRecord {
  const postmortem = { ...incident.postmortem, updatedAtMs: now };
  for (const [key, value] of Object.entries(patch) as Array<[keyof Omit<IncidentPostmortemDraft, "updatedAtMs">, string]>) {
    postmortem[key] = text(value, 12_000);
  }
  return { ...incident, postmortem, revision: incident.revision + 1, updatedAtMs: now };
}

export function addIncidentTimelineNote(
  incident: IncidentRecord,
  title: string,
  detail: string,
  actor: string,
  occurredAtMs = Date.now(),
): IncidentRecord {
  if (!text(title, 300)) throw new Error("Enter a timeline note title.");
  return changed(incident, {}, {
    kind: "note",
    title,
    detail,
    actor,
    relatedId: null,
    occurredAtMs,
  }, occurredAtMs);
}

function ownerName(incident: IncidentRecord, ownerId: string | null): string {
  if (!ownerId) return "Unassigned";
  return incident.owners.find((owner) => owner.id === ownerId)?.name ?? "Unknown owner";
}

function timestamp(value: number | null): string {
  return value === null ? "Not recorded" : new Date(value).toISOString();
}

function sectionValue(value: string): string {
  return value.trim() || "_Not yet documented._";
}

export function incidentCompleteness(incident: IncidentRecord): IncidentCompleteness {
  const missing: string[] = [];
  if (!incident.owners.some((owner) => owner.active && owner.role === "commander")) missing.push("active incident commander");
  if (incident.evidence.length === 0 && incident.alerts.length === 0) missing.push("alert or evidence");
  if (incident.decisions.length === 0) missing.push("decision log entry");
  if (incident.mitigations.length === 0) missing.push("mitigation");
  if (!incident.postmortem.rootCause.trim()) missing.push("postmortem root cause");
  if (!incident.postmortem.resolution.trim()) missing.push("postmortem resolution");
  return { complete: missing.length === 0, missing };
}

export function buildIncidentPostmortemMarkdown(incident: IncidentRecord): string {
  const completeness = incidentCompleteness(incident);
  const lines = [
    `# Postmortem draft: ${incident.title}`,
    "",
    "> DRAFT — local coordination artifact. Review before sharing.",
    `> ${INCIDENT_SAFETY_NOTICE}`,
    "",
    `- Incident ID: ${incident.id}`,
    `- Severity: ${incident.severity.toUpperCase()}`,
    `- Status: ${incident.status}`,
    `- Service: ${incident.service || "Not recorded"}`,
    `- Started: ${timestamp(incident.startedAtMs)}`,
    `- Resolved: ${timestamp(incident.resolvedAtMs)}`,
    `- Revision: ${incident.revision}`,
    `- Completeness: ${completeness.complete ? "complete" : `draft; missing ${completeness.missing.join(", ")}`}`,
    "",
    "## Executive summary",
    "",
    sectionValue(incident.postmortem.executiveSummary || incident.summary),
    "",
    "## Impact",
    "",
    sectionValue(incident.postmortem.impact || incident.impact),
    "",
    "## Detection",
    "",
    sectionValue(incident.postmortem.detection),
    "",
    "## Root cause",
    "",
    sectionValue(incident.postmortem.rootCause),
    "",
    "## Contributing factors",
    "",
    sectionValue(incident.postmortem.contributingFactors),
    "",
    "## Resolution",
    "",
    sectionValue(incident.postmortem.resolution),
    "",
    "## What went well",
    "",
    sectionValue(incident.postmortem.whatWentWell),
    "",
    "## What went poorly",
    "",
    sectionValue(incident.postmortem.whatWentPoorly),
    "",
    "## Follow-up actions",
    "",
    sectionValue(incident.postmortem.followUpActions),
    "",
    "## Owners",
    "",
    ...(incident.owners.length > 0
      ? incident.owners.map((owner) => `- ${owner.name} — ${owner.role}${owner.responsibility ? `: ${owner.responsibility}` : ""}${owner.active ? "" : " (inactive)"}`)
      : ["- _No owners assigned._"]),
    "",
    "## Timeline",
    "",
    ...incident.timeline.slice().sort((left, right) => left.occurredAtMs - right.occurredAtMs)
      .map((entry) => `- ${timestamp(entry.occurredAtMs)} — **${entry.title}** (${entry.actor})${entry.detail ? `: ${entry.detail}` : ""}`),
    "",
    "## Decisions",
    "",
    ...(incident.decisions.length > 0
      ? incident.decisions.map((decision) => `- ${timestamp(decision.decidedAtMs)} — **${decision.title}:** ${decision.decision}${decision.rationale ? ` Rationale: ${decision.rationale}` : ""}`)
      : ["- _No decisions recorded._"]),
    "",
    "## Mitigations and approvals",
    "",
    ...(incident.mitigations.length > 0
      ? incident.mitigations.map((mitigation) =>
        `- **${mitigation.title}** — ${mitigation.status}; ${mitigation.actionClass}; ${mitigation.risk} risk; approval ${mitigation.approval.state}; owner ${ownerName(incident, mitigation.ownerId)}.${mitigation.verification ? ` Verification: ${mitigation.verification}` : ""}`)
      : ["- _No mitigations recorded._"]),
    "",
    "## Evidence index",
    "",
    ...(incident.evidence.length > 0
      ? incident.evidence.map((evidence) => `- [${evidence.kind}] ${evidence.title} — ${evidence.sourceUri} (${timestamp(evidence.observedAtMs)})`)
      : ["- _No evidence recorded._"]),
    "",
  ];
  return `${lines.join("\n")}\n`;
}

export function buildIncidentRunbookMarkdown(incident: IncidentRecord): string {
  const lines = [
    `# Incident runbook: ${incident.title}`,
    "",
    `> ${INCIDENT_SAFETY_NOTICE}`,
    "> Every step is a record of manual work outside this feature; this artifact does not execute commands or changes.",
    "",
    ...(incident.runbook.length > 0 ? incident.runbook.map((step, index) => [
      `## ${index + 1}. ${step.title}`,
      "",
      `- Status: ${step.status}`,
      `- Owner: ${ownerName(incident, step.ownerId)}`,
      `- Execution: ${step.executionMode}`,
      "",
      sectionValue(step.instructions),
      "",
      `Verification: ${step.verification || "Not recorded"}`,
      "",
    ].join("\n")) : ["_No runbook steps recorded._", ""]),
  ];
  return `${lines.join("\n")}\n`;
}

export function serializeIncidentBundle(incident: IncidentRecord, exportedAtMs = Date.now()): string {
  return JSON.stringify({
    schemaVersion: INCIDENT_SCHEMA_VERSION,
    kind: "little-monkey-incident-bundle",
    exportedAtMs,
    safetyNotice: INCIDENT_SAFETY_NOTICE,
    incident,
    postmortemMarkdown: buildIncidentPostmortemMarkdown(incident),
    runbookMarkdown: buildIncidentRunbookMarkdown(incident),
  }, null, 2);
}

function object(value: unknown): value is Record<string, unknown> {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function strings(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((entry) => typeof entry === "string");
}

function number(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

const SEVERITIES = new Set<IncidentSeverity>(["sev0", "sev1", "sev2", "sev3"]);
const STATUSES = new Set<IncidentStatus>(["declared", "investigating", "mitigating", "monitoring", "resolved", "closed"]);
const OWNER_ROLES = new Set<IncidentOwnerRole>(["commander", "technical", "communications", "operations", "observer"]);
const ALERT_SEVERITIES = new Set<IncidentAlertSeverity>(["critical", "warning", "info"]);
const ALERT_STATUSES = new Set<IncidentAlertStatus>(["firing", "acknowledged", "resolved"]);
const EVIDENCE_KINDS = new Set<IncidentEvidenceKind>(["alert", "log", "trace", "dashboard", "ticket", "runbook", "release", "note"]);
const RISKS = new Set<IncidentRisk>(["low", "medium", "high", "critical"]);
const ACTION_CLASSES = new Set<IncidentActionClass>(["read_only", "customer_facing", "destructive", "infrastructure_change"]);
const APPROVAL_STATES = new Set<IncidentApprovalState>(["not_required", "pending", "approved", "rejected"]);
const MITIGATION_STATUSES = new Set<IncidentMitigationStatus>(["proposed", "in_progress_external", "monitoring", "verified", "failed", "rejected", "cancelled"]);
const RUNBOOK_STATUSES = new Set<IncidentRunbookStepStatus>(["pending", "in_progress_external", "completed_external", "skipped"]);
const AUDIENCES = new Set<IncidentDraftAudience>(["internal", "customer", "executive", "engineering"]);
const TIMELINE_KINDS = new Set<IncidentTimelineKind>(["incident_declared", "status_changed", "owner_assigned", "alert_added", "alert_changed", "evidence_added", "mitigation_proposed", "approval_recorded", "mitigation_changed", "decision_recorded", "runbook_changed", "status_draft_changed", "note"]);

export function isIncidentRecord(value: unknown): value is IncidentRecord {
  if (!object(value) || value.schemaVersion !== INCIDENT_SCHEMA_VERSION || typeof value.id !== "string" ||
    !number(value.revision) || typeof value.title !== "string" || !SEVERITIES.has(value.severity as IncidentSeverity) ||
    !STATUSES.has(value.status as IncidentStatus) || typeof value.summary !== "string" || typeof value.impact !== "string" ||
    typeof value.service !== "string" || !number(value.startedAtMs) || !(value.resolvedAtMs === null || number(value.resolvedAtMs)) ||
    !number(value.createdAtMs) || !number(value.updatedAtMs) || !object(value.postmortem)) return false;
  const owners = Array.isArray(value.owners) && value.owners.every((entry) => object(entry) && typeof entry.id === "string" && typeof entry.name === "string" && OWNER_ROLES.has(entry.role as IncidentOwnerRole) && typeof entry.responsibility === "string" && typeof entry.active === "boolean" && number(entry.assignedAtMs));
  const alerts = Array.isArray(value.alerts) && value.alerts.every((entry) => object(entry) && typeof entry.id === "string" && typeof entry.title === "string" && typeof entry.source === "string" && ALERT_SEVERITIES.has(entry.severity as IncidentAlertSeverity) && ALERT_STATUSES.has(entry.status as IncidentAlertStatus) && typeof entry.description === "string" && number(entry.firedAtMs) && number(entry.updatedAtMs));
  const evidence = Array.isArray(value.evidence) && value.evidence.every((entry) => object(entry) && typeof entry.id === "string" && EVIDENCE_KINDS.has(entry.kind as IncidentEvidenceKind) && typeof entry.title === "string" && typeof entry.sourceUri === "string" && typeof entry.content === "string" && number(entry.observedAtMs) && number(entry.addedAtMs));
  const mitigations = Array.isArray(value.mitigations) && value.mitigations.every((entry) => {
    if (!object(entry) || typeof entry.id !== "string" || typeof entry.title !== "string" || typeof entry.description !== "string" || !(entry.ownerId === null || typeof entry.ownerId === "string") || !RISKS.has(entry.risk as IncidentRisk) || !ACTION_CLASSES.has(entry.actionClass as IncidentActionClass) || !MITIGATION_STATUSES.has(entry.status as IncidentMitigationStatus) || !object(entry.approval) || entry.executionMode !== "manual_external_only" || typeof entry.verification !== "string" || !number(entry.createdAtMs) || !number(entry.updatedAtMs)) return false;
    if (!APPROVAL_STATES.has(entry.approval.state as IncidentApprovalState) || !(entry.approval.requestedAtMs === null || number(entry.approval.requestedAtMs)) || !(entry.approval.decidedAtMs === null || number(entry.approval.decidedAtMs)) || !(entry.approval.decidedBy === null || typeof entry.approval.decidedBy === "string") || typeof entry.approval.note !== "string") return false;
    const policy = actionPolicy(entry.actionClass as IncidentActionClass, entry.risk as IncidentRisk);
    if (policy.requiresHumanApproval && entry.approval.state === "not_required") return false;
    if (policy.requiresHumanApproval && ACTIVE_MITIGATION_STATUSES.has(entry.status as IncidentMitigationStatus) && entry.approval.state !== "approved") return false;
    return true;
  });
  const decisions = Array.isArray(value.decisions) && value.decisions.every((entry) => object(entry) && typeof entry.id === "string" && typeof entry.title === "string" && typeof entry.decision === "string" && typeof entry.rationale === "string" && !(entry.ownerId !== null && typeof entry.ownerId !== "string") && strings(entry.alternatives) && strings(entry.evidenceIds) && number(entry.decidedAtMs));
  const runbook = Array.isArray(value.runbook) && value.runbook.every((entry) => object(entry) && typeof entry.id === "string" && typeof entry.title === "string" && typeof entry.instructions === "string" && !(entry.ownerId !== null && typeof entry.ownerId !== "string") && RUNBOOK_STATUSES.has(entry.status as IncidentRunbookStepStatus) && typeof entry.verification === "string" && number(entry.updatedAtMs) && entry.executionMode === "manual_external_only");
  const drafts = Array.isArray(value.statusUpdateDrafts) && value.statusUpdateDrafts.every((entry) => object(entry) && typeof entry.id === "string" && AUDIENCES.has(entry.audience as IncidentDraftAudience) && typeof entry.title === "string" && typeof entry.body === "string" && (entry.state === "draft" || entry.state === "superseded") && entry.draftOnly === true && number(entry.createdAtMs) && number(entry.updatedAtMs));
  const timeline = Array.isArray(value.timeline) && value.timeline.every((entry) => object(entry) && typeof entry.id === "string" && TIMELINE_KINDS.has(entry.kind as IncidentTimelineKind) && typeof entry.title === "string" && typeof entry.detail === "string" && typeof entry.actor === "string" && !(entry.relatedId !== null && typeof entry.relatedId !== "string") && number(entry.occurredAtMs));
  const postmortem = value.postmortem;
  const postmortemValid = ["executiveSummary", "impact", "detection", "rootCause", "contributingFactors", "resolution", "whatWentWell", "whatWentPoorly", "followUpActions"].every((key) => typeof postmortem[key] === "string") && number(postmortem.updatedAtMs);
  return Boolean(owners && alerts && evidence && mitigations && decisions && runbook && drafts && timeline && postmortemValid);
}
