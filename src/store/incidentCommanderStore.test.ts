import { beforeEach, describe, expect, it } from "vitest";

import { addIncidentMitigation, createIncident } from "../lib/incidentCommander";
import {
  INCIDENT_COMMANDER_STORAGE_KEY,
  __resetIncidentCommanderStoreForTests,
  useIncidentCommanderStore,
} from "./incidentCommanderStore";

class MemoryStorage implements Storage {
  private values = new Map<string, string>();
  get length() { return this.values.size; }
  clear() { this.values.clear(); }
  getItem(key: string) { return this.values.get(key) ?? null; }
  key(index: number) { return [...this.values.keys()][index] ?? null; }
  removeItem(key: string) { this.values.delete(key); }
  setItem(key: string, value: string) { this.values.set(key, String(value)); }
}

describe("incidentCommanderStore", () => {
  beforeEach(() => {
    Object.defineProperty(globalThis, "localStorage", { value: new MemoryStorage(), configurable: true });
    __resetIncidentCommanderStoreForTests();
  });

  it("persists and rehydrates a complete local incident coordination record", () => {
    const created = useIncidentCommanderStore.getState().createIncident({
      title: "Checkout errors",
      severity: "sev1",
      service: "checkout-api",
      summary: "authorization: secret-value",
    });
    const store = useIncidentCommanderStore.getState();
    store.addOwner(created.id, {
      name: "Alice",
      role: "commander",
      responsibility: "Coordinate the response",
    });
    const ownerId = useIncidentCommanderStore.getState().incidents[0].owners[0].id;
    store.addAlert(created.id, {
      title: "5xx rate",
      source: "Pasted monitor snapshot",
      severity: "critical",
      description: "Errors above threshold",
    });
    store.addEvidence(created.id, {
      kind: "log",
      title: "Timeout excerpt",
      sourceUri: "local://pasted-log",
      content: "inventory request timed out",
    });
    const evidenceId = useIncidentCommanderStore.getState().incidents[0].evidence[0].id;
    store.addDecision(created.id, {
      title: "Pause rollout",
      decision: "Do not continue the rollout.",
      rationale: "The release correlates with errors.",
      ownerId,
      alternatives: ["Continue rollout"],
      evidenceIds: [evidenceId],
    });
    store.addRunbookStep(created.id, {
      title: "Inspect release health",
      instructions: "Open the deployment console manually and capture its state.",
      ownerId,
    });
    store.addStatusDraft(created.id, {
      audience: "customer",
      title: "Checkout disruption",
      body: "We are investigating elevated errors.",
    });
    store.addTimelineNote(created.id, "Traffic update", "Errors are declining.", "Alice");

    const persisted = JSON.parse(localStorage.getItem(INCIDENT_COMMANDER_STORAGE_KEY) ?? "null");
    expect(persisted.version).toBe(1);
    expect(persisted.incidents[0]).toMatchObject({
      id: created.id,
      summary: expect.stringContaining("[REDACTED]"),
      owners: [{ name: "Alice" }],
      alerts: [{ title: "5xx rate" }],
      evidence: [{ title: "Timeout excerpt" }],
      decisions: [{ title: "Pause rollout", evidenceIds: [evidenceId] }],
      runbook: [{ executionMode: "manual_external_only" }],
      statusUpdateDrafts: [{ draftOnly: true, state: "draft" }],
    });

    useIncidentCommanderStore.setState({ incidents: [], selectedIncidentId: null, error: null });
    useIncidentCommanderStore.getState().init();
    expect(useIncidentCommanderStore.getState().incidents[0].id).toBe(created.id);
    expect(useIncidentCommanderStore.getState().selectedIncidentId).toBe(created.id);
  });

  it("will only record gated external mitigation progress after explicit approval", () => {
    const created = useIncidentCommanderStore.getState().createIncident({ title: "Database latency" });
    useIncidentCommanderStore.getState().addMitigation(created.id, {
      title: "Fail over the database",
      description: "An operator may perform failover in the infrastructure console.",
      ownerId: null,
      risk: "critical",
      actionClass: "infrastructure_change",
    });
    const mitigationId = useIncidentCommanderStore.getState().incidents[0].mitigations[0].id;

    useIncidentCommanderStore.getState().updateMitigationStatus(
      created.id,
      mitigationId,
      "in_progress_external",
      "Operator",
    );
    expect(useIncidentCommanderStore.getState().incidents[0].mitigations[0].status).toBe("proposed");
    expect(useIncidentCommanderStore.getState().error).toMatch(/approval/i);

    useIncidentCommanderStore.getState().recordApproval(
      created.id,
      mitigationId,
      "approved",
      "Change manager",
      "Approved on the incident bridge",
    );
    useIncidentCommanderStore.getState().updateMitigationStatus(
      created.id,
      mitigationId,
      "in_progress_external",
      "Operator",
      "Started outside Little Monkey",
    );
    expect(useIncidentCommanderStore.getState().incidents[0].mitigations[0]).toMatchObject({
      status: "in_progress_external",
      approval: { state: "approved", decidedBy: "Change manager" },
      executionMode: "manual_external_only",
    });
    expect(useIncidentCommanderStore.getState().error).toBeNull();
  });

  it("drops forged unsafe records during hydration", () => {
    const base = addIncidentMitigation(createIncident({ title: "Forged incident" }), {
      title: "Restart cluster",
      description: "",
      ownerId: null,
      risk: "high",
      actionClass: "infrastructure_change",
    });
    const forged = structuredClone(base);
    forged.mitigations[0].approval.state = "not_required";
    forged.mitigations[0].status = "verified";
    localStorage.setItem(INCIDENT_COMMANDER_STORAGE_KEY, JSON.stringify({ version: 1, incidents: [forged] }));

    useIncidentCommanderStore.getState().init();

    expect(useIncidentCommanderStore.getState().incidents).toEqual([]);
    expect(JSON.parse(localStorage.getItem(INCIDENT_COMMANDER_STORAGE_KEY) ?? "null").incidents).toEqual([]);
  });

  it("deletes an incident and clears its selection", () => {
    const created = useIncidentCommanderStore.getState().createIncident({ title: "Temporary incident" });
    useIncidentCommanderStore.getState().deleteIncident(created.id);

    expect(useIncidentCommanderStore.getState()).toMatchObject({
      incidents: [],
      selectedIncidentId: null,
      error: null,
    });
  });
});
