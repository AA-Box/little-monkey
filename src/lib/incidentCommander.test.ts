import { describe, expect, it } from "vitest";

import {
  INCIDENT_SAFETY_NOTICE,
  actionPolicy,
  addIncidentAlert,
  addIncidentDecision,
  addIncidentEvidence,
  addIncidentMitigation,
  addIncidentOwner,
  addIncidentRunbookStep,
  addIncidentStatusDraft,
  addIncidentTimelineNote,
  buildIncidentPostmortemMarkdown,
  buildIncidentRunbookMarkdown,
  createIncident,
  incidentCompleteness,
  isIncidentRecord,
  recordIncidentApproval,
  serializeIncidentBundle,
  transitionIncidentStatus,
  updateIncidentMitigationStatus,
  updateIncidentPostmortem,
  updateIncidentRunbookStep,
  updateIncidentStatusDraft,
} from "./incidentCommander";

function incident() {
  return createIncident({
    id: "incident-1",
    title: "Checkout elevated errors",
    severity: "sev1",
    service: "checkout-api",
    summary: "Error rate increased after release r42.",
    impact: "Some customers cannot complete checkout.",
    now: 1_000,
    actor: "Alice",
  });
}

describe("Incident Commander domain", () => {
  it("creates a local incident and redacts secrets from captured text", () => {
    const created = createIncident({
      id: "i-secret",
      title: "API incident",
      summary: "authorization: super-secret-value",
      now: 10,
    });

    expect(created.status).toBe("declared");
    expect(created.summary).toContain("[REDACTED]");
    expect(created.timeline).toMatchObject([{ kind: "incident_declared", occurredAtMs: 10 }]);
  });

  it("enforces valid incident lifecycle transitions and records them", () => {
    const investigating = transitionIncidentStatus(incident(), "investigating", "Alice", "Triage started", 2_000);
    const resolved = transitionIncidentStatus(investigating, "resolved", "Alice", "Error rate recovered", 3_000);

    expect(resolved.status).toBe("resolved");
    expect(resolved.resolvedAtMs).toBe(3_000);
    expect(resolved.timeline[resolved.timeline.length - 1]).toMatchObject({ kind: "status_changed", actor: "Alice" });
    expect(() => transitionIncidentStatus(incident(), "closed", "Alice", "", 2_000)).toThrow(/cannot move/i);
  });

  it("requires approval for customer-facing, destructive, infrastructure, and high-risk actions", () => {
    expect(actionPolicy("customer_facing", "low").requiresHumanApproval).toBe(true);
    expect(actionPolicy("destructive", "low").requiresHumanApproval).toBe(true);
    expect(actionPolicy("infrastructure_change", "medium").requiresHumanApproval).toBe(true);
    expect(actionPolicy("read_only", "high").requiresHumanApproval).toBe(true);
    expect(actionPolicy("read_only", "low")).toMatchObject({ requiresHumanApproval: false, executionMode: "manual_external_only" });
  });

  it("cannot record a gated mitigation as started until explicit approval is recorded", () => {
    const proposed = addIncidentMitigation(incident(), {
      id: "mit-1",
      title: "Roll back release",
      description: "Operator should roll back r42 in the deployment system.",
      ownerId: null,
      risk: "high",
      actionClass: "infrastructure_change",
      createdAtMs: 2_000,
      actor: "Alice",
    });

    expect(proposed.mitigations[0]).toMatchObject({
      approval: { state: "pending" },
      status: "proposed",
      executionMode: "manual_external_only",
    });
    expect(() => updateIncidentMitigationStatus(proposed, "mit-1", "in_progress_external", "Bob", "", 2_100)).toThrow(/approval/i);

    const approved = recordIncidentApproval(proposed, "mit-1", "approved", "Change manager", "Approved in incident call", 2_200);
    const started = updateIncidentMitigationStatus(approved, "mit-1", "in_progress_external", "Bob", "Started in deployment console", 2_300);
    expect(started.mitigations[0]).toMatchObject({
      approval: { state: "approved", decidedBy: "Change manager" },
      status: "in_progress_external",
    });
    expect(started.timeline.map((entry) => entry.kind)).toEqual(expect.arrayContaining(["approval_recorded", "mitigation_changed"]));
  });

  it("blocks rejected mitigations from being marked active", () => {
    const proposed = addIncidentMitigation(incident(), {
      id: "mit-rejected",
      title: "Disable checkout",
      description: "",
      ownerId: null,
      risk: "critical",
      actionClass: "customer_facing",
      createdAtMs: 2_000,
    });
    const rejected = recordIncidentApproval(proposed, "mit-rejected", "rejected", "Incident commander", "Too much impact", 2_100);

    expect(rejected.mitigations[0].status).toBe("rejected");
    expect(() => updateIncidentMitigationStatus(rejected, "mit-rejected", "monitoring", "Alice", "", 2_200)).toThrow(/rejected/i);
  });

  it("captures owners, alerts, evidence, decisions, notes, and external runbook state in one timeline", () => {
    let value = addIncidentOwner(incident(), {
      id: "owner-1",
      name: "Alice",
      role: "commander",
      responsibility: "Coordinate response",
      assignedAtMs: 1_100,
    });
    value = addIncidentAlert(value, {
      id: "alert-1",
      title: "Checkout 5xx",
      source: "local monitor",
      severity: "critical",
      description: "5xx above 10%",
      firedAtMs: 1_200,
    });
    value = addIncidentEvidence(value, {
      id: "evidence-1",
      kind: "log",
      title: "API error excerpt",
      sourceUri: "local://pasted-log",
      content: "timeout while calling inventory",
      observedAtMs: 1_190,
      addedAtMs: 1_300,
    });
    value = addIncidentDecision(value, {
      id: "decision-1",
      title: "Pause release",
      decision: "Do not continue the rollout.",
      rationale: "Errors correlate with r42.",
      ownerId: "owner-1",
      alternatives: ["Continue rollout"],
      evidenceIds: ["evidence-1", "missing"],
      decidedAtMs: 1_400,
    });
    value = addIncidentRunbookStep(value, {
      id: "step-1",
      title: "Inspect deployment health",
      instructions: "Open the deployment console and capture health.",
      ownerId: "owner-1",
      now: 1_500,
    });
    value = updateIncidentRunbookStep(value, "step-1", "completed_external", "Alice", "Screenshot attached", 1_600);
    value = addIncidentTimelineNote(value, "Traffic stable", "5xx below 1%", "Alice", 1_700);

    expect(value.decisions[0].evidenceIds).toEqual(["evidence-1"]);
    expect(value.runbook[0]).toMatchObject({ status: "completed_external", executionMode: "manual_external_only" });
    expect(value.timeline.map((entry) => entry.kind)).toEqual(expect.arrayContaining([
      "owner_assigned", "alert_added", "evidence_added", "decision_recorded", "runbook_changed", "note",
    ]));
  });

  it("keeps every status update permanently draft-only with no publish state", () => {
    const created = addIncidentStatusDraft(incident(), {
      id: "draft-1",
      audience: "customer",
      title: "Checkout disruption",
      body: "We are investigating elevated errors.",
      now: 2_000,
      actor: "Comms owner",
    });
    const updated = updateIncidentStatusDraft(created, "draft-1", {
      body: "We identified the cause and are monitoring recovery.",
    }, "Comms owner", 2_100);
    const superseded = updateIncidentStatusDraft(updated, "draft-1", { state: "superseded" }, "Comms owner", 2_200);

    expect(superseded.statusUpdateDrafts[0]).toMatchObject({ draftOnly: true, state: "superseded" });
    expect(Object.keys(superseded.statusUpdateDrafts[0])).not.toContain("publishedAtMs");
    expect(superseded.timeline[superseded.timeline.length - 1]?.detail).toContain("NOT SENT");
  });

  it("exports deterministic postmortem and runbook artifacts with safety boundaries", () => {
    let value = addIncidentRunbookStep(incident(), {
      id: "step-1",
      title: "Check error rate",
      instructions: "Read the local dashboard snapshot.",
      ownerId: null,
      now: 2_000,
    });
    value = updateIncidentPostmortem(value, {
      executiveSummary: "Release r42 caused elevated checkout errors.",
      rootCause: "A timeout regression.",
      resolution: "Operators rolled back r42 outside Little Monkey.",
    }, 3_000);
    const postmortem = buildIncidentPostmortemMarkdown(value);
    const runbook = buildIncidentRunbookMarkdown(value);
    const bundle = JSON.parse(serializeIncidentBundle(value, 4_000));

    expect(postmortem).toContain("# Postmortem draft");
    expect(postmortem).toContain("A timeout regression");
    expect(runbook).toContain("manual_external_only");
    expect(runbook).toContain(INCIDENT_SAFETY_NOTICE);
    expect(bundle).toMatchObject({
      schemaVersion: 1,
      kind: "little-monkey-incident-bundle",
      exportedAtMs: 4_000,
      safetyNotice: INCIDENT_SAFETY_NOTICE,
      incident: { id: "incident-1" },
    });
  });

  it("reports postmortem completeness and rejects forged unsafe persisted state", () => {
    const base = incident();
    expect(incidentCompleteness(base)).toMatchObject({ complete: false });
    expect(isIncidentRecord(base)).toBe(true);

    const gated = addIncidentMitigation(base, {
      id: "mit-1",
      title: "Restart cluster",
      description: "",
      ownerId: null,
      risk: "high",
      actionClass: "infrastructure_change",
      createdAtMs: 2_000,
    });
    const forged = structuredClone(gated);
    forged.mitigations[0].approval.state = "not_required";
    forged.mitigations[0].status = "verified";
    expect(isIncidentRecord(forged)).toBe(false);

    const draft = addIncidentStatusDraft(base, {
      id: "draft-1",
      audience: "customer",
      title: "Draft",
      body: "Not sent",
      now: 2_000,
    });
    const forgedDraft = structuredClone(draft) as unknown as { statusUpdateDrafts: Array<{ draftOnly: boolean }> };
    forgedDraft.statusUpdateDrafts[0].draftOnly = false;
    expect(isIncidentRecord(forgedDraft)).toBe(false);
  });
});
