import { useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import {
  Activity,
  AlertTriangle,
  Ban,
  Check,
  ClipboardCheck,
  Download,
  FileText,
  MessageSquareText,
  Plus,
  Radio,
  ShieldAlert,
  Siren,
  Trash2,
  Users,
  X,
} from "lucide-react";

import {
  INCIDENT_SAFETY_NOTICE,
  actionPolicy,
  availableIncidentStatuses,
  buildIncidentPostmortemMarkdown,
  buildIncidentRunbookMarkdown,
  incidentCompleteness,
  serializeIncidentBundle,
  type IncidentActionClass,
  type IncidentAlertSeverity,
  type IncidentDraftAudience,
  type IncidentEvidenceKind,
  type IncidentMitigation,
  type IncidentMitigationStatus,
  type IncidentOwnerRole,
  type IncidentPostmortemDraft,
  type IncidentRecord,
  type IncidentRisk,
  type IncidentRunbookStep,
  type IncidentRunbookStepStatus,
  type IncidentSeverity,
  type IncidentStatusUpdateDraft,
} from "../../lib/incidentCommander";
import { useT } from "../../lib/i18n";
import { useIncidentCommanderStore } from "../../store/incidentCommanderStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";
import { statusTone as sharedStatusTone } from "../../lib/statusTone";

interface IncidentCommanderPanelProps {
  onClose: () => void;
}

type IncidentTab = "overview" | "response" | "timeline" | "communications" | "postmortem";
type ExportKind = "postmortem" | "runbook" | "bundle";

const FIELD_CLASS = "mt-1 min-h-11 w-full rounded-md border border-border bg-background px-3 py-2 text-sm text-foreground outline-none placeholder:text-faint focus:border-accent focus:ring-2 focus:ring-accent/30";
const LABEL_CLASS = "block text-xs font-medium text-muted";
const SEVERITIES: readonly IncidentSeverity[] = ["sev0", "sev1", "sev2", "sev3"];
const OWNER_ROLES: readonly IncidentOwnerRole[] = ["commander", "technical", "communications", "operations", "observer"];
const ALERT_SEVERITIES: readonly IncidentAlertSeverity[] = ["critical", "warning", "info"];
const EVIDENCE_KINDS: readonly IncidentEvidenceKind[] = ["alert", "log", "trace", "dashboard", "ticket", "runbook", "release", "note"];
const RISKS: readonly IncidentRisk[] = ["low", "medium", "high", "critical"];
const ACTION_CLASSES: readonly IncidentActionClass[] = ["read_only", "customer_facing", "destructive", "infrastructure_change"];
const MITIGATION_STATUSES: readonly IncidentMitigationStatus[] = ["proposed", "in_progress_external", "monitoring", "verified", "failed", "rejected", "cancelled"];
const RUNBOOK_STATUSES: readonly IncidentRunbookStepStatus[] = ["pending", "in_progress_external", "completed_external", "skipped"];
const AUDIENCES: readonly IncidentDraftAudience[] = ["internal", "customer", "executive", "engineering"];

function severityTone(severity: IncidentSeverity): PillTone {
  if (severity === "sev0") return "danger";
  if (severity === "sev1") return "warning";
  if (severity === "sev2") return "neutral";
  return "success";
}

function statusTone(status: IncidentRecord["status"]): PillTone {
  // A freshly declared incident is the loudest state here, not a neutral one.
  return sharedStatusTone(status, { declared: "danger", mitigating: "warning", monitoring: "warning" });
}

function approvalTone(state: IncidentMitigation["approval"]["state"]): PillTone {
  if (state === "approved" || state === "not_required") return "success";
  if (state === "rejected") return "danger";
  return "warning";
}

function time(value: number): string {
  return new Date(value).toLocaleString();
}

function fileStem(title: string): string {
  return title.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 60) || "incident";
}

function Card({ title, hint, icon, children }: { title: string; hint?: string; icon?: ReactNode; children: ReactNode }) {
  return (
    <section className="rounded-lg border border-border bg-surface p-4">
      <div className="flex items-start gap-2">
        {icon && <span className="mt-0.5 text-accent" aria-hidden="true">{icon}</span>}
        <div>
          <h3 className="text-sm font-semibold text-foreground">{title}</h3>
          {hint && <p className="mt-1 max-w-3xl text-xs leading-5 text-muted">{hint}</p>}
        </div>
      </div>
      <div className="mt-4">{children}</div>
    </section>
  );
}

function Empty({ children }: { children: ReactNode }) {
  return <p className="rounded-md border border-dashed border-border p-4 text-center text-xs text-faint">{children}</p>;
}

function OverviewTab({ incident }: { incident: IncidentRecord }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [ownerName, setOwnerName] = useState("");
  const [ownerRole, setOwnerRole] = useState<IncidentOwnerRole>("commander");
  const [responsibility, setResponsibility] = useState("");
  const [alertTitle, setAlertTitle] = useState("");
  const [alertSource, setAlertSource] = useState("");
  const [alertSeverity, setAlertSeverity] = useState<IncidentAlertSeverity>("critical");
  const [alertDescription, setAlertDescription] = useState("");
  const [evidenceKind, setEvidenceKind] = useState<IncidentEvidenceKind>("log");
  const [evidenceTitle, setEvidenceTitle] = useState("");
  const [evidenceSource, setEvidenceSource] = useState("");
  const [evidenceContent, setEvidenceContent] = useState("");

  const submitOwner = (event: FormEvent) => {
    event.preventDefault();
    store.addOwner(incident.id, { name: ownerName, role: ownerRole, responsibility });
    if (!useIncidentCommanderStore.getState().error) {
      setOwnerName("");
      setResponsibility("");
    }
  };
  const submitAlert = (event: FormEvent) => {
    event.preventDefault();
    store.addAlert(incident.id, { title: alertTitle, source: alertSource, severity: alertSeverity, description: alertDescription });
    if (!useIncidentCommanderStore.getState().error) {
      setAlertTitle("");
      setAlertSource("");
      setAlertDescription("");
    }
  };
  const submitEvidence = (event: FormEvent) => {
    event.preventDefault();
    store.addEvidence(incident.id, { kind: evidenceKind, title: evidenceTitle, sourceUri: evidenceSource, content: evidenceContent });
    if (!useIncidentCommanderStore.getState().error) {
      setEvidenceTitle("");
      setEvidenceSource("");
      setEvidenceContent("");
    }
  };

  return (
    <div className="space-y-4">
      <Card title={t("IncidentCommander.tab.overview")} icon={<Activity size={16} />}>
        <div className="grid gap-3 lg:grid-cols-2">
          <label className={LABEL_CLASS}>
            {t("IncidentCommander.titleLabel")}
            <input className={FIELD_CLASS} value={incident.title} onChange={(event) => store.updateIncident(incident.id, { title: event.target.value })} />
          </label>
          <label className={LABEL_CLASS}>
            {t("IncidentCommander.serviceLabel")}
            <input className={FIELD_CLASS} value={incident.service} placeholder={t("IncidentCommander.servicePlaceholder")} onChange={(event) => store.updateIncident(incident.id, { service: event.target.value })} />
          </label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>
            {t("IncidentCommander.summaryLabel")}
            <textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={incident.summary} placeholder={t("IncidentCommander.summaryPlaceholder")} onChange={(event) => store.updateIncident(incident.id, { summary: event.target.value })} />
          </label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>
            {t("IncidentCommander.impactLabel")}
            <textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={incident.impact} placeholder={t("IncidentCommander.impactPlaceholder")} onChange={(event) => store.updateIncident(incident.id, { impact: event.target.value })} />
          </label>
        </div>
      </Card>

      <div className="grid gap-4 2xl:grid-cols-2">
        <Card title={t("IncidentCommander.owners")} icon={<Users size={16} />}>
          <form className="grid gap-3 sm:grid-cols-2" onSubmit={submitOwner}>
            <label className={LABEL_CLASS}>{t("IncidentCommander.ownerName")}<input required className={FIELD_CLASS} value={ownerName} placeholder={t("IncidentCommander.ownerNamePlaceholder")} onChange={(event) => setOwnerName(event.target.value)} /></label>
            <label className={LABEL_CLASS}>{t("IncidentCommander.ownerRole")}<select className={FIELD_CLASS} value={ownerRole} onChange={(event) => setOwnerRole(event.target.value as IncidentOwnerRole)}>{OWNER_ROLES.map((role) => <option key={role} value={role}>{t(`IncidentCommander.ownerRole.${role}`)}</option>)}</select></label>
            <label className={`${LABEL_CLASS} sm:col-span-2`}>{t("IncidentCommander.ownerResponsibility")}<input className={FIELD_CLASS} value={responsibility} placeholder={t("IncidentCommander.ownerResponsibilityPlaceholder")} onChange={(event) => setResponsibility(event.target.value)} /></label>
            <Button type="submit" size="sm" variant="primary" className="sm:col-span-2"><Plus size={14} />{t("IncidentCommander.addOwner")}</Button>
          </form>
          <div className="mt-4 space-y-2">
            {incident.owners.length === 0 ? <Empty>{t("IncidentCommander.noOwners")}</Empty> : incident.owners.map((owner) => (
              <div key={owner.id} className="rounded-md border border-border bg-background p-3">
                <div className="flex flex-wrap items-center justify-between gap-2"><p className="text-sm font-medium text-foreground">{owner.name}</p><StatusPill>{t(`IncidentCommander.ownerRole.${owner.role}`)}</StatusPill></div>
                {owner.responsibility && <p className="mt-2 text-xs leading-5 text-muted">{owner.responsibility}</p>}
              </div>
            ))}
          </div>
        </Card>

        <Card title={t("IncidentCommander.alerts")} icon={<Radio size={16} />}>
          <form className="grid gap-3 sm:grid-cols-2" onSubmit={submitAlert}>
            <label className={LABEL_CLASS}>{t("IncidentCommander.alertTitle")}<input required className={FIELD_CLASS} value={alertTitle} onChange={(event) => setAlertTitle(event.target.value)} /></label>
            <label className={LABEL_CLASS}>{t("IncidentCommander.alertSource")}<input className={FIELD_CLASS} value={alertSource} onChange={(event) => setAlertSource(event.target.value)} /></label>
            <label className={LABEL_CLASS}>{t("IncidentCommander.severityLabel")}<select className={FIELD_CLASS} value={alertSeverity} onChange={(event) => setAlertSeverity(event.target.value as IncidentAlertSeverity)}>{ALERT_SEVERITIES.map((severity) => <option key={severity} value={severity}>{t(`IncidentCommander.alertSeverity.${severity}`)}</option>)}</select></label>
            <label className={`${LABEL_CLASS} sm:col-span-2`}>{t("IncidentCommander.alertDescription")}<textarea rows={2} className={`${FIELD_CLASS} resize-y`} value={alertDescription} onChange={(event) => setAlertDescription(event.target.value)} /></label>
            <Button type="submit" size="sm" variant="primary" className="sm:col-span-2"><Plus size={14} />{t("IncidentCommander.addAlert")}</Button>
          </form>
          <div className="mt-4 space-y-2">
            {incident.alerts.length === 0 ? <Empty>{t("IncidentCommander.noAlerts")}</Empty> : incident.alerts.map((alert) => (
              <div key={alert.id} className="rounded-md border border-border bg-background p-3">
                <div className="flex flex-wrap items-center justify-between gap-2"><p className="text-sm font-medium text-foreground">{alert.title}</p><StatusPill tone={alert.severity === "critical" ? "danger" : alert.severity === "warning" ? "warning" : "neutral"}>{t(`IncidentCommander.alertSeverity.${alert.severity}`)}</StatusPill></div>
                <p className="mt-1 text-xs text-faint">{alert.source} · {time(alert.firedAtMs)}</p>
                {alert.description && <p className="mt-2 text-xs leading-5 text-muted">{alert.description}</p>}
                <label className={`${LABEL_CLASS} mt-2`}>{t("IncidentCommander.recordExternalState")}<select className={FIELD_CLASS} value={alert.status} onChange={(event) => store.updateAlertStatus(incident.id, alert.id, event.target.value as IncidentRecord["alerts"][number]["status"], "Local user")}>{["firing", "acknowledged", "resolved"].map((status) => <option key={status} value={status}>{t(`IncidentCommander.alertStatus.${status}`)}</option>)}</select></label>
              </div>
            ))}
          </div>
        </Card>
      </div>

      <Card title={t("IncidentCommander.evidence")} icon={<FileText size={16} />}>
        <form className="grid gap-3 lg:grid-cols-2" onSubmit={submitEvidence}>
          <label className={LABEL_CLASS}>{t("IncidentCommander.evidenceKind")}<select className={FIELD_CLASS} value={evidenceKind} onChange={(event) => setEvidenceKind(event.target.value as IncidentEvidenceKind)}>{EVIDENCE_KINDS.map((kind) => <option key={kind} value={kind}>{t(`IncidentCommander.evidenceKind.${kind}`)}</option>)}</select></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.evidenceTitle")}<input required className={FIELD_CLASS} value={evidenceTitle} onChange={(event) => setEvidenceTitle(event.target.value)} /></label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.evidenceSource")}<input className={FIELD_CLASS} value={evidenceSource} placeholder="local://pasted-evidence" onChange={(event) => setEvidenceSource(event.target.value)} /></label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.evidenceContent")}<textarea required rows={4} className={`${FIELD_CLASS} resize-y font-mono text-xs`} value={evidenceContent} placeholder={t("IncidentCommander.evidenceContentPlaceholder")} onChange={(event) => setEvidenceContent(event.target.value)} /></label>
          <Button type="submit" size="sm" variant="primary" className="lg:col-span-2"><Plus size={14} />{t("IncidentCommander.addEvidence")}</Button>
        </form>
        <div className="mt-4 grid gap-2 lg:grid-cols-2">
          {incident.evidence.length === 0 ? <div className="lg:col-span-2"><Empty>{t("IncidentCommander.noEvidence")}</Empty></div> : incident.evidence.map((evidence) => (
            <article key={evidence.id} className="min-w-0 rounded-md border border-border bg-background p-3">
              <div className="flex items-center justify-between gap-2"><p className="truncate text-sm font-medium text-foreground">{evidence.title}</p><StatusPill>{evidence.kind}</StatusPill></div>
              <p className="mt-1 truncate text-[11px] text-faint">{evidence.sourceUri} · {time(evidence.observedAtMs)}</p>
              <pre className="mt-2 max-h-36 overflow-auto whitespace-pre-wrap break-words rounded bg-surface-2 p-2 text-[11px] text-muted">{evidence.content}</pre>
            </article>
          ))}
        </div>
      </Card>
    </div>
  );
}

function MitigationCard({ incident, mitigation }: { incident: IncidentRecord; mitigation: IncidentMitigation }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [approver, setApprover] = useState("");
  const [note, setNote] = useState("");
  const [actor, setActor] = useState("Local user");
  const [verification, setVerification] = useState(mitigation.verification);
  const policy = actionPolicy(mitigation.actionClass, mitigation.risk);

  return (
    <article className="rounded-md border border-border bg-background p-3">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div><h4 className="text-sm font-medium text-foreground">{mitigation.title}</h4><p className="mt-1 text-[11px] text-faint">{t(`IncidentCommander.actionClass.${mitigation.actionClass}`)} · {t(`IncidentCommander.risk.${mitigation.risk}`)}</p></div>
        <StatusPill tone={approvalTone(mitigation.approval.state)}>{t(`IncidentCommander.approval.${mitigation.approval.state}`)}</StatusPill>
      </div>
      {mitigation.description && <p className="mt-2 whitespace-pre-wrap text-xs leading-5 text-muted">{mitigation.description}</p>}
      <div className="mt-3 rounded-md border border-warning/30 bg-warning-soft p-2.5 text-xs text-warning">
        <p className="font-medium">{t("IncidentCommander.executionBoundary")}</p>
        <p className="mt-1 leading-5">{policy.reason}</p>
      </div>
      {mitigation.approval.state === "pending" && (
        <div className="mt-3 grid gap-2 sm:grid-cols-2">
          <label className={LABEL_CLASS}>{t("IncidentCommander.approver")}<input className={FIELD_CLASS} value={approver} onChange={(event) => setApprover(event.target.value)} /></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.approvalNote")}<input className={FIELD_CLASS} value={note} onChange={(event) => setNote(event.target.value)} /></label>
          <div className="flex flex-wrap gap-2 sm:col-span-2">
            <Button size="sm" variant="primary" disabled={!approver.trim()} onClick={() => store.recordApproval(incident.id, mitigation.id, "approved", approver, note)}><Check size={14} />{t("IncidentCommander.approve")}</Button>
            <Button size="sm" variant="danger" disabled={!approver.trim()} onClick={() => store.recordApproval(incident.id, mitigation.id, "rejected", approver, note)}><Ban size={14} />{t("IncidentCommander.reject")}</Button>
          </div>
        </div>
      )}
      <div className="mt-3 grid gap-2 sm:grid-cols-2">
        <label className={LABEL_CLASS}>{t("IncidentCommander.recordExternalState")}<select className={FIELD_CLASS} disabled={mitigation.approval.state === "pending"} value={mitigation.status} onChange={(event) => store.updateMitigationStatus(incident.id, mitigation.id, event.target.value as IncidentMitigationStatus, actor, verification)}>{MITIGATION_STATUSES.map((status) => <option key={status} value={status}>{t(`IncidentCommander.mitigationStatus.${status}`)}</option>)}</select></label>
        <label className={LABEL_CLASS}>{t("IncidentCommander.actor")}<input className={FIELD_CLASS} value={actor} onChange={(event) => setActor(event.target.value)} /></label>
        <label className={`${LABEL_CLASS} sm:col-span-2`}>{t("IncidentCommander.verification")}<textarea rows={2} className={`${FIELD_CLASS} resize-y`} value={verification} onChange={(event) => setVerification(event.target.value)} /></label>
        <Button size="sm" className="sm:col-span-2" onClick={() => store.updateMitigationStatus(incident.id, mitigation.id, mitigation.status, actor, verification)}><Check size={14} />{t("IncidentCommander.saveExternalRecord")}</Button>
      </div>
    </article>
  );
}

function RunbookCard({ incident, step }: { incident: IncidentRecord; step: IncidentRunbookStep }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [verification, setVerification] = useState(step.verification);
  return (
    <article className="rounded-md border border-border bg-background p-3">
      <div className="flex flex-wrap items-center justify-between gap-2"><h4 className="text-sm font-medium text-foreground">{step.title}</h4><StatusPill tone={step.status === "completed_external" ? "success" : step.status === "in_progress_external" ? "warning" : "neutral"}>{t(`IncidentCommander.runbookStatus.${step.status}`)}</StatusPill></div>
      <p className="mt-2 whitespace-pre-wrap text-xs leading-5 text-muted">{step.instructions}</p>
      <p className="mt-2 text-[11px] font-medium text-warning">{t("IncidentCommander.executionBoundary")}</p>
      <div className="mt-3 grid gap-2 sm:grid-cols-2">
        <label className={LABEL_CLASS}>{t("IncidentCommander.recordExternalState")}<select className={FIELD_CLASS} value={step.status} onChange={(event) => store.updateRunbookStep(incident.id, step.id, event.target.value as IncidentRunbookStepStatus, "Local user", verification)}>{RUNBOOK_STATUSES.map((status) => <option key={status} value={status}>{t(`IncidentCommander.runbookStatus.${status}`)}</option>)}</select></label>
        <label className={LABEL_CLASS}>{t("IncidentCommander.verification")}<input className={FIELD_CLASS} value={verification} onChange={(event) => setVerification(event.target.value)} /></label>
        <Button size="sm" className="sm:col-span-2" onClick={() => store.updateRunbookStep(incident.id, step.id, step.status, "Local user", verification)}><Check size={14} />{t("IncidentCommander.saveExternalRecord")}</Button>
      </div>
    </article>
  );
}

function ResponseTab({ incident }: { incident: IncidentRecord }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [mitigationTitle, setMitigationTitle] = useState("");
  const [mitigationDescription, setMitigationDescription] = useState("");
  const [ownerId, setOwnerId] = useState<string>("");
  const [risk, setRisk] = useState<IncidentRisk>("medium");
  const [actionClass, setActionClass] = useState<IncidentActionClass>("read_only");
  const [stepTitle, setStepTitle] = useState("");
  const [stepInstructions, setStepInstructions] = useState("");
  const [decisionTitle, setDecisionTitle] = useState("");
  const [decision, setDecision] = useState("");
  const [rationale, setRationale] = useState("");
  const [alternatives, setAlternatives] = useState("");
  const [evidenceIds, setEvidenceIds] = useState<string[]>([]);

  const submitMitigation = (event: FormEvent) => {
    event.preventDefault();
    store.addMitigation(incident.id, { title: mitigationTitle, description: mitigationDescription, ownerId: ownerId || null, risk, actionClass });
    if (!useIncidentCommanderStore.getState().error) { setMitigationTitle(""); setMitigationDescription(""); }
  };
  const submitStep = (event: FormEvent) => {
    event.preventDefault();
    store.addRunbookStep(incident.id, { title: stepTitle, instructions: stepInstructions, ownerId: ownerId || null });
    if (!useIncidentCommanderStore.getState().error) { setStepTitle(""); setStepInstructions(""); }
  };
  const submitDecision = (event: FormEvent) => {
    event.preventDefault();
    store.addDecision(incident.id, {
      title: decisionTitle,
      decision,
      rationale,
      ownerId: ownerId || null,
      alternatives: alternatives.split("\n").map((entry) => entry.trim()).filter(Boolean),
      evidenceIds,
    });
    if (!useIncidentCommanderStore.getState().error) { setDecisionTitle(""); setDecision(""); setRationale(""); setAlternatives(""); setEvidenceIds([]); }
  };

  return (
    <div className="space-y-4">
      <Card title={t("IncidentCommander.mitigations")} hint={t("IncidentCommander.mitigationHint")} icon={<ShieldAlert size={16} />}>
        <form className="grid gap-3 lg:grid-cols-2" onSubmit={submitMitigation}>
          <label className={LABEL_CLASS}>{t("IncidentCommander.mitigationTitle")}<input required className={FIELD_CLASS} value={mitigationTitle} onChange={(event) => setMitigationTitle(event.target.value)} /></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.owner")}<select className={FIELD_CLASS} value={ownerId} onChange={(event) => setOwnerId(event.target.value)}><option value="">{t("IncidentCommander.unassigned")}</option>{incident.owners.map((owner) => <option key={owner.id} value={owner.id}>{owner.name}</option>)}</select></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.risk")}<select className={FIELD_CLASS} value={risk} onChange={(event) => setRisk(event.target.value as IncidentRisk)}>{RISKS.map((value) => <option key={value} value={value}>{t(`IncidentCommander.risk.${value}`)}</option>)}</select></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.actionClass")}<select className={FIELD_CLASS} value={actionClass} onChange={(event) => setActionClass(event.target.value as IncidentActionClass)}>{ACTION_CLASSES.map((value) => <option key={value} value={value}>{t(`IncidentCommander.actionClass.${value}`)}</option>)}</select></label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.mitigationDescription")}<textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={mitigationDescription} onChange={(event) => setMitigationDescription(event.target.value)} /></label>
          <div className="rounded-md border border-warning/30 bg-warning-soft p-3 text-xs text-warning lg:col-span-2"><p className="font-medium">{actionPolicy(actionClass, risk).requiresHumanApproval ? t("IncidentCommander.approvalRequired") : t("IncidentCommander.approvalNotRequired")}</p><p className="mt-1 leading-5">{actionPolicy(actionClass, risk).reason}</p></div>
          <Button type="submit" size="sm" variant="primary" className="lg:col-span-2"><Plus size={14} />{t("IncidentCommander.proposeMitigation")}</Button>
        </form>
        <div className="mt-4 space-y-3">{incident.mitigations.length === 0 ? <Empty>{t("IncidentCommander.noMitigations")}</Empty> : incident.mitigations.map((mitigation) => <MitigationCard key={mitigation.id} incident={incident} mitigation={mitigation} />)}</div>
      </Card>

      <Card title={t("IncidentCommander.runbook")} hint={t("IncidentCommander.runbookHint")} icon={<ClipboardCheck size={16} />}>
        <form className="grid gap-3 lg:grid-cols-2" onSubmit={submitStep}>
          <label className={LABEL_CLASS}>{t("IncidentCommander.stepTitle")}<input required className={FIELD_CLASS} value={stepTitle} onChange={(event) => setStepTitle(event.target.value)} /></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.owner")}<select className={FIELD_CLASS} value={ownerId} onChange={(event) => setOwnerId(event.target.value)}><option value="">{t("IncidentCommander.unassigned")}</option>{incident.owners.map((owner) => <option key={owner.id} value={owner.id}>{owner.name}</option>)}</select></label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.stepInstructions")}<textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={stepInstructions} onChange={(event) => setStepInstructions(event.target.value)} /></label>
          <Button type="submit" size="sm" variant="primary" className="lg:col-span-2"><Plus size={14} />{t("IncidentCommander.addStep")}</Button>
        </form>
        <div className="mt-4 space-y-3">{incident.runbook.length === 0 ? <Empty>{t("IncidentCommander.noSteps")}</Empty> : incident.runbook.map((step) => <RunbookCard key={step.id} incident={incident} step={step} />)}</div>
      </Card>

      <Card title={t("IncidentCommander.decisions")} icon={<ClipboardCheck size={16} />}>
        <form className="grid gap-3 lg:grid-cols-2" onSubmit={submitDecision}>
          <label className={LABEL_CLASS}>{t("IncidentCommander.decisionTitle")}<input required className={FIELD_CLASS} value={decisionTitle} onChange={(event) => setDecisionTitle(event.target.value)} /></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.owner")}<select className={FIELD_CLASS} value={ownerId} onChange={(event) => setOwnerId(event.target.value)}><option value="">{t("IncidentCommander.unassigned")}</option>{incident.owners.map((owner) => <option key={owner.id} value={owner.id}>{owner.name}</option>)}</select></label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.decision")}<textarea required rows={3} className={`${FIELD_CLASS} resize-y`} value={decision} onChange={(event) => setDecision(event.target.value)} /></label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.rationale")}<textarea rows={2} className={`${FIELD_CLASS} resize-y`} value={rationale} onChange={(event) => setRationale(event.target.value)} /></label>
          <label className={LABEL_CLASS}>{t("IncidentCommander.alternatives")}<textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={alternatives} onChange={(event) => setAlternatives(event.target.value)} /></label>
          <fieldset className="rounded-md border border-border p-3"><legend className="px-1 text-xs font-medium text-muted">{t("IncidentCommander.linkedEvidence")}</legend><div className="max-h-32 space-y-2 overflow-y-auto">{incident.evidence.length === 0 ? <p className="text-xs text-faint">{t("IncidentCommander.noEvidence")}</p> : incident.evidence.map((evidence) => <label key={evidence.id} className="flex cursor-pointer items-center gap-2 text-xs text-muted"><input type="checkbox" checked={evidenceIds.includes(evidence.id)} onChange={(event) => setEvidenceIds((current) => event.target.checked ? [...current, evidence.id] : current.filter((id) => id !== evidence.id))} />{evidence.title}</label>)}</div></fieldset>
          <Button type="submit" size="sm" variant="primary" className="lg:col-span-2"><Plus size={14} />{t("IncidentCommander.recordDecision")}</Button>
        </form>
        <div className="mt-4 space-y-2">{incident.decisions.length === 0 ? <Empty>{t("IncidentCommander.noDecisions")}</Empty> : incident.decisions.slice().reverse().map((entry) => <article key={entry.id} className="rounded-md border border-border bg-background p-3"><div className="flex flex-wrap items-center justify-between gap-2"><h4 className="text-sm font-medium text-foreground">{entry.title}</h4><span className="text-[11px] text-faint">{time(entry.decidedAtMs)}</span></div><p className="mt-2 whitespace-pre-wrap text-xs leading-5 text-foreground">{entry.decision}</p>{entry.rationale && <p className="mt-2 text-xs leading-5 text-muted">{entry.rationale}</p>}</article>)}</div>
      </Card>
    </div>
  );
}

function TimelineTab({ incident }: { incident: IncidentRecord }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [title, setTitle] = useState("");
  const [detail, setDetail] = useState("");
  const [actor, setActor] = useState("Local user");
  const submit = (event: FormEvent) => {
    event.preventDefault();
    store.addTimelineNote(incident.id, title, detail, actor);
    if (!useIncidentCommanderStore.getState().error) { setTitle(""); setDetail(""); }
  };
  const entries = useMemo(() => incident.timeline.slice().sort((left, right) => right.occurredAtMs - left.occurredAtMs), [incident.timeline]);
  return (
    <Card title={t("IncidentCommander.timeline")} icon={<Activity size={16} />}>
      <form className="grid gap-3 lg:grid-cols-2" onSubmit={submit}>
        <label className={LABEL_CLASS}>{t("IncidentCommander.timelineTitle")}<input required className={FIELD_CLASS} value={title} onChange={(event) => setTitle(event.target.value)} /></label>
        <label className={LABEL_CLASS}>{t("IncidentCommander.actor")}<input required className={FIELD_CLASS} value={actor} onChange={(event) => setActor(event.target.value)} /></label>
        <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.timelineDetail")}<textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={detail} onChange={(event) => setDetail(event.target.value)} /></label>
        <Button type="submit" size="sm" variant="primary" className="lg:col-span-2"><Plus size={14} />{t("IncidentCommander.addNote")}</Button>
      </form>
      <ol className="mt-5 space-y-3 border-l border-border pl-4">
        {entries.length === 0 ? <Empty>{t("IncidentCommander.noTimeline")}</Empty> : entries.map((entry) => <li key={entry.id} className="relative rounded-md border border-border bg-background p-3 before:absolute before:-left-[1.31rem] before:top-4 before:h-2 before:w-2 before:rounded-full before:bg-accent"><div className="flex flex-wrap items-start justify-between gap-2"><p className="text-sm font-medium text-foreground">{entry.title}</p><time className="text-[11px] text-faint">{time(entry.occurredAtMs)}</time></div><p className="mt-1 text-[11px] text-faint">{entry.actor} · {entry.kind.replace(/_/g, " ")}</p>{entry.detail && <p className="mt-2 whitespace-pre-wrap text-xs leading-5 text-muted">{entry.detail}</p>}</li>)}
      </ol>
    </Card>
  );
}

function DraftCard({ incident, draft }: { incident: IncidentRecord; draft: IncidentStatusUpdateDraft }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [title, setTitle] = useState(draft.title);
  const [body, setBody] = useState(draft.body);
  useEffect(() => { setTitle(draft.title); setBody(draft.body); }, [draft.id, draft.title, draft.body]);
  return (
    <article className="rounded-md border border-border bg-background p-3">
      <div className="flex flex-wrap items-center justify-between gap-2"><StatusPill tone={draft.state === "draft" ? "warning" : "neutral"}>{t(`IncidentCommander.draftState.${draft.state}`)}</StatusPill><span className="text-[11px] font-semibold uppercase tracking-wide text-warning">{t("IncidentCommander.notSentMarker")}</span></div>
      <label className={`${LABEL_CLASS} mt-3`}>{t("IncidentCommander.draftTitle")}<input disabled={draft.state === "superseded"} className={FIELD_CLASS} value={title} onChange={(event) => setTitle(event.target.value)} /></label>
      <label className={`${LABEL_CLASS} mt-3`}>{t("IncidentCommander.draftBody")}<textarea disabled={draft.state === "superseded"} rows={5} className={`${FIELD_CLASS} resize-y`} value={body} onChange={(event) => setBody(event.target.value)} /></label>
      {draft.state === "draft" && <div className="mt-3 flex flex-wrap gap-2"><Button size="sm" variant="secondary" onClick={() => store.updateStatusDraft(incident.id, draft.id, { title, body }, "Local user")}><Check size={14} />{t("IncidentCommander.saveDraft")}</Button><Button size="sm" variant="ghost" onClick={() => store.updateStatusDraft(incident.id, draft.id, { state: "superseded" }, "Local user")}><Ban size={14} />{t("IncidentCommander.supersedeDraft")}</Button></div>}
    </article>
  );
}

function CommunicationsTab({ incident }: { incident: IncidentRecord }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [audience, setAudience] = useState<IncidentDraftAudience>("internal");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const submit = (event: FormEvent) => {
    event.preventDefault();
    store.addStatusDraft(incident.id, { audience, title, body });
    if (!useIncidentCommanderStore.getState().error) { setTitle(""); setBody(""); }
  };
  return (
    <Card title={t("IncidentCommander.draftOnlyTitle")} hint={t("IncidentCommander.draftOnlyBody")} icon={<MessageSquareText size={16} />}>
      <div className="mb-4 rounded-md border border-warning/40 bg-warning-soft p-3 text-xs leading-5 text-warning"><strong>{t("IncidentCommander.draftOnlyTitle")}</strong><br />{INCIDENT_SAFETY_NOTICE}</div>
      <form className="grid gap-3 lg:grid-cols-2" onSubmit={submit}>
        <label className={LABEL_CLASS}>{t("IncidentCommander.audience")}<select className={FIELD_CLASS} value={audience} onChange={(event) => setAudience(event.target.value as IncidentDraftAudience)}>{AUDIENCES.map((value) => <option key={value} value={value}>{t(`IncidentCommander.audience.${value}`)}</option>)}</select></label>
        <label className={LABEL_CLASS}>{t("IncidentCommander.draftTitle")}<input required className={FIELD_CLASS} value={title} onChange={(event) => setTitle(event.target.value)} /></label>
        <label className={`${LABEL_CLASS} lg:col-span-2`}>{t("IncidentCommander.draftBody")}<textarea required rows={5} className={`${FIELD_CLASS} resize-y`} value={body} onChange={(event) => setBody(event.target.value)} /></label>
        <Button type="submit" size="sm" variant="primary" className="lg:col-span-2"><Plus size={14} />{t("IncidentCommander.addDraft")}</Button>
      </form>
      <div className="mt-4 space-y-3">{incident.statusUpdateDrafts.length === 0 ? <Empty>{t("IncidentCommander.noDrafts")}</Empty> : incident.statusUpdateDrafts.slice().reverse().map((draft) => <DraftCard key={draft.id} incident={incident} draft={draft} />)}</div>
    </Card>
  );
}

const POSTMORTEM_FIELDS: ReadonlyArray<keyof Omit<IncidentPostmortemDraft, "updatedAtMs">> = [
  "executiveSummary", "impact", "detection", "rootCause", "contributingFactors", "resolution", "whatWentWell", "whatWentPoorly", "followUpActions",
];

function PostmortemTab({ incident, onExport }: { incident: IncidentRecord; onExport: (kind: ExportKind) => void }) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const completeness = incidentCompleteness(incident);
  return (
    <div className="space-y-4">
      <Card title={t("IncidentCommander.postmortemDraft")} hint={t("IncidentCommander.postmortemHint")} icon={<FileText size={16} />}>
        <div className={`rounded-md border p-3 text-xs ${completeness.complete ? "border-success/40 bg-success-soft text-success" : "border-warning/40 bg-warning-soft text-warning"}`}>{completeness.complete ? t("IncidentCommander.completenessComplete") : t("IncidentCommander.completenessMissing", { items: completeness.missing.join(", ") })}</div>
        <div className="mt-4 grid gap-3 lg:grid-cols-2">
          {POSTMORTEM_FIELDS.map((field) => <label key={field} className={`${LABEL_CLASS} ${field === "executiveSummary" || field === "rootCause" || field === "resolution" ? "lg:col-span-2" : ""}`}>{t(`IncidentCommander.postmortem.${field}`)}<textarea rows={4} className={`${FIELD_CLASS} resize-y`} value={incident.postmortem[field]} onChange={(event) => store.updatePostmortem(incident.id, { [field]: event.target.value })} /></label>)}
        </div>
        <div className="mt-4 flex flex-wrap gap-2"><Button size="sm" variant="primary" onClick={() => onExport("postmortem")}><Download size={14} />{t("IncidentCommander.exportPostmortem")}</Button><Button size="sm" onClick={() => onExport("runbook")}><Download size={14} />{t("IncidentCommander.exportRunbook")}</Button><Button size="sm" onClick={() => onExport("bundle")}><Download size={14} />{t("IncidentCommander.exportBundle")}</Button></div>
      </Card>
      <Card title={t("IncidentCommander.preview")} icon={<FileText size={16} />}><pre className="max-h-[36rem] overflow-auto whitespace-pre-wrap break-words rounded-md bg-background p-4 text-xs leading-5 text-muted">{buildIncidentPostmortemMarkdown(incident)}</pre></Card>
    </div>
  );
}

export function IncidentCommanderPanel({ onClose }: IncidentCommanderPanelProps) {
  const { t } = useT();
  const store = useIncidentCommanderStore();
  const [tab, setTab] = useState<IncidentTab>("overview");
  const [newTitle, setNewTitle] = useState("");
  const [newService, setNewService] = useState("");
  const [newSeverity, setNewSeverity] = useState<IncidentSeverity>("sev2");
  const [newSummary, setNewSummary] = useState("");
  const [exportError, setExportError] = useState<string | null>(null);

  useEffect(() => { useIncidentCommanderStore.getState().init(); }, []);
  const selected = useMemo(() => store.incidents.find((incident) => incident.id === store.selectedIncidentId) ?? null, [store.incidents, store.selectedIncidentId]);

  const create = (event: FormEvent) => {
    event.preventDefault();
    try {
      store.createIncident({ title: newTitle, service: newService, severity: newSeverity, summary: newSummary });
      setNewTitle(""); setNewService(""); setNewSummary(""); setTab("overview");
    } catch { /* The store exposes the redacted validation message inline. */ }
  };

  const exportArtifact = async (kind: ExportKind) => {
    if (!selected) return;
    setExportError(null);
    const markdown = kind === "postmortem" ? buildIncidentPostmortemMarkdown(selected) : buildIncidentRunbookMarkdown(selected);
    const content = kind === "bundle" ? serializeIncidentBundle(selected) : markdown;
    const extension = kind === "bundle" ? "json" : "md";
    try {
      const destination = await save({
        defaultPath: `${fileStem(selected.title)}-${kind}.${extension}`,
        filters: [{ name: kind === "bundle" ? "JSON" : "Markdown", extensions: [extension] }],
      });
      if (destination) await writeTextFile(destination, content);
    } catch (error) {
      setExportError(errorMessage(error));
    }
  };

  const tabs: Array<{ id: IncidentTab; icon: ReactNode }> = [
    { id: "overview", icon: <Activity size={14} /> },
    { id: "response", icon: <ShieldAlert size={14} /> },
    { id: "timeline", icon: <Radio size={14} /> },
    { id: "communications", icon: <MessageSquareText size={14} /> },
    { id: "postmortem", icon: <FileText size={14} /> },
  ];

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="incident-commander-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <div className="flex items-center gap-2"><Siren size={17} className="text-accent" /><h2 id="incident-commander-title" className="text-sm font-semibold text-foreground">{t("IncidentCommander.title")}</h2></div>
          <p className="mt-1 max-w-3xl text-xs leading-5 text-muted">{t("IncidentCommander.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("IncidentCommander.close")} title={t("IncidentCommander.close")} onClick={onClose}><X size={15} /></IconButton>
      </header>

      <div className="mx-5 mt-4 shrink-0 rounded-lg border border-warning/40 bg-warning-soft p-3 text-warning" role="note">
        <p className="flex items-center gap-2 text-xs font-semibold"><AlertTriangle size={14} />{t("IncidentCommander.safetyTitle")}</p>
        <p className="mt-1 text-xs leading-5">{t("IncidentCommander.safetyBody")}</p>
      </div>

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(17rem,.72fr)_minmax(0,2fr)]">
        <aside className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
          <h3 className="text-xs font-semibold text-foreground">{t("IncidentCommander.newIncident")}</h3>
          <form className="mt-3 space-y-3" onSubmit={create}>
            <label className={LABEL_CLASS}>{t("IncidentCommander.titleLabel")}<input required autoFocus className={FIELD_CLASS} value={newTitle} placeholder={t("IncidentCommander.titlePlaceholder")} onChange={(event) => setNewTitle(event.target.value)} /></label>
            <label className={LABEL_CLASS}>{t("IncidentCommander.serviceLabel")}<input className={FIELD_CLASS} value={newService} placeholder={t("IncidentCommander.servicePlaceholder")} onChange={(event) => setNewService(event.target.value)} /></label>
            <label className={LABEL_CLASS}>{t("IncidentCommander.severityLabel")}<select className={FIELD_CLASS} value={newSeverity} onChange={(event) => setNewSeverity(event.target.value as IncidentSeverity)}>{SEVERITIES.map((severity) => <option key={severity} value={severity}>{t(`IncidentCommander.severity.${severity}`)}</option>)}</select></label>
            <label className={LABEL_CLASS}>{t("IncidentCommander.summaryLabel")}<textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={newSummary} placeholder={t("IncidentCommander.summaryPlaceholder")} onChange={(event) => setNewSummary(event.target.value)} /></label>
            <Button type="submit" className="w-full" variant="primary" disabled={!newTitle.trim()}><Siren size={14} />{t("IncidentCommander.create")}</Button>
          </form>
          <h3 className="mt-5 text-xs font-semibold text-foreground">{t("IncidentCommander.savedIncidents")}</h3>
          <div className="mt-2 space-y-2">
            {store.incidents.length === 0 ? <Empty>{t("IncidentCommander.empty")}</Empty> : store.incidents.map((incident) => (
              <div key={incident.id} className={`rounded-md border transition-colors ${incident.id === selected?.id ? "border-accent bg-accent/10" : "border-border bg-background hover:border-border-strong"}`}>
                <button type="button" className="min-h-11 w-full cursor-pointer p-3 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent" onClick={() => { store.selectIncident(incident.id); setTab("overview"); }}>
                  <p className="truncate text-xs font-medium text-foreground">{incident.title}</p>
                  <div className="mt-2 flex flex-wrap gap-1.5"><StatusPill tone={severityTone(incident.severity)}>{t(`IncidentCommander.severity.${incident.severity}`)}</StatusPill><StatusPill tone={statusTone(incident.status)}>{t(`IncidentCommander.status.${incident.status}`)}</StatusPill></div>
                </button>
                <button type="button" className="flex min-h-11 w-full cursor-pointer items-center justify-center gap-1 border-t border-border px-2 text-[11px] text-faint transition-colors hover:text-danger focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-danger" onClick={() => { if (window.confirm(t("IncidentCommander.deleteConfirm"))) store.deleteIncident(incident.id); }}><Trash2 size={12} />{t("IncidentCommander.delete")}</button>
              </div>
            ))}
          </div>
        </aside>

        <main className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface-2 p-4">
          {!selected ? (
            <div className="grid min-h-full place-items-center p-10 text-center"><div><Siren size={32} className="mx-auto text-faint" /><p className="mt-3 text-sm font-medium text-foreground">{t("IncidentCommander.empty")}</p><p className="mt-1 text-xs text-muted">{t("IncidentCommander.emptyDetail")}</p></div></div>
          ) : (
            <div className="space-y-4">
              <div className="rounded-lg border border-border bg-surface p-4">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div><h3 className="text-base font-semibold text-foreground">{selected.title}</h3><p className="mt-1 text-[11px] text-faint">{t("IncidentCommander.updated", { time: time(selected.updatedAtMs) })}</p></div>
                  <div className="flex flex-wrap gap-2"><StatusPill tone={severityTone(selected.severity)}>{t(`IncidentCommander.severity.${selected.severity}`)}</StatusPill><StatusPill tone={statusTone(selected.status)}>{t(`IncidentCommander.status.${selected.status}`)}</StatusPill></div>
                </div>
                <div className="mt-3 grid gap-3 sm:grid-cols-2">
                  <label className={LABEL_CLASS}>{t("IncidentCommander.severityLabel")}<select className={FIELD_CLASS} value={selected.severity} onChange={(event) => store.updateIncident(selected.id, { severity: event.target.value as IncidentSeverity })}>{SEVERITIES.map((severity) => <option key={severity} value={severity}>{t(`IncidentCommander.severity.${severity}`)}</option>)}</select></label>
                  <label className={LABEL_CLASS}>{t("IncidentCommander.statusLabel")}<select className={FIELD_CLASS} value={selected.status} onChange={(event) => store.transitionStatus(selected.id, event.target.value as IncidentRecord["status"], "Local user")}>{availableIncidentStatuses(selected.status).map((status) => <option key={status} value={status}>{t(`IncidentCommander.status.${status}`)}</option>)}</select></label>
                </div>
              </div>

              {(store.error || exportError) && <div role="alert" className="flex items-start justify-between gap-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger"><div><p className="font-semibold">{exportError ? t("IncidentCommander.exportFailed") : t("IncidentCommander.error")}</p><p className="mt-1">{exportError ?? store.error}</p></div><IconButton size="sm" aria-label={t("IncidentCommander.dismiss")} onClick={() => { store.clearError(); setExportError(null); }}><X size={14} /></IconButton></div>}

              <nav className="grid grid-cols-2 gap-1 rounded-lg border border-border bg-surface p-1 sm:grid-cols-5" aria-label={t("IncidentCommander.title")}>
                {tabs.map(({ id, icon }) => <button key={id} type="button" aria-current={tab === id ? "page" : undefined} className={`flex min-h-11 cursor-pointer items-center justify-center gap-1.5 rounded-md px-2 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${tab === id ? "bg-accent text-accent-foreground" : "text-muted hover:bg-surface-2 hover:text-foreground"}`} onClick={() => setTab(id)}>{icon}{t(`IncidentCommander.tab.${id}`)}</button>)}
              </nav>

              {tab === "overview" && <OverviewTab incident={selected} />}
              {tab === "response" && <ResponseTab incident={selected} />}
              {tab === "timeline" && <TimelineTab incident={selected} />}
              {tab === "communications" && <CommunicationsTab incident={selected} />}
              {tab === "postmortem" && <PostmortemTab incident={selected} onExport={(kind) => { void exportArtifact(kind); }} />}
            </div>
          )}
        </main>
      </div>
    </section>
  );
}
