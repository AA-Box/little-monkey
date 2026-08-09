import { useState } from "react";
import { BadgeCheck, ShieldAlert } from "lucide-react";
import { StatusPill, type PillTone } from "../../ui";
import type {
  ConformanceCheckStatus,
  ConformanceSectionId,
  ConformanceSectionStatus,
} from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { BusyButton, CONTROL_CLASS, ErrorNotice, Field, SectionHeading } from "./RuntimeHubShared";

/** Mirrors `http_policy::DEFAULT_HTTP_PORT`. */
const DEFAULT_TARGET = "http://127.0.0.1:1234";

const SECTIONS: { id: ConformanceSectionId; label: string; covers: string }[] = [
  { id: "contract", label: "Contract", covers: "K19" },
  { id: "isolation", label: "Isolation", covers: "K3" },
  { id: "limits", label: "Limits", covers: "K4/K5" },
  { id: "ledger", label: "Ledger", covers: "K12" },
];

const SECTION_TONE: Record<ConformanceSectionStatus, PillTone> = {
  passed: "success",
  failed: "danger",
  incomplete: "warning",
  skipped: "neutral",
};

const SECTION_LABEL: Record<ConformanceSectionStatus, string> = {
  passed: "Pass",
  failed: "Fail",
  incomplete: "Incomplete",
  skipped: "Skipped",
};

const CHECK_TONE: Record<ConformanceCheckStatus, PillTone> = {
  passed: "success",
  failed: "danger",
  skipped: "neutral",
};

export function RuntimeHubConformance() {
  const report = useRuntimeHubStore((state) => state.conformanceReport);
  const run = useRuntimeHubStore((state) => state.runConformanceSuite);
  const busy = useRuntimeHubStore((state) => state.busy["conformance-suite"]);
  const error = useRuntimeHubStore((state) => state.errors["conformance-suite"]);

  const [baseUrl, setBaseUrl] = useState(DEFAULT_TARGET);
  const [token, setToken] = useState("");
  const [selected, setSelected] = useState<ConformanceSectionId[]>([]);

  const toggle = (id: ConformanceSectionId) =>
    setSelected((current) =>
      current.includes(id) ? current.filter((entry) => entry !== id) : [...current, id],
    );

  return (
    <section className="flex flex-col gap-4 rounded-lg border border-border bg-background p-4">
      <SectionHeading
        title="Conformance suite (K21)"
        description="The published, runnable suite — the same one `monkey-cli conformance` runs. It talks to a live node over HTTP rather than reading this process's own state, so a passing run is evidence about the listener, not about this window. Required section: contract. Optional: isolation, limits, ledger — a section a node does not claim is reported as skipped, never as a silent pass."
      />

      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Node base URL" hint="Any live listener, including one this app did not start.">
          <input
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            className={`${CONTROL_CLASS} font-mono`}
            placeholder={DEFAULT_TARGET}
          />
        </Field>
        <Field label="API token" hint="Never stored. Mint one in Settings → API server.">
          <input
            type="password"
            value={token}
            onChange={(event) => setToken(event.target.value)}
            className={`${CONTROL_CLASS} font-mono`}
            placeholder="Leave empty if the node needs no token"
          />
        </Field>
      </div>

      <fieldset className="flex flex-wrap items-center gap-3">
        <legend className="sr-only">Sections to run</legend>
        <span className="text-xs text-muted">Sections (none selected runs all):</span>
        {SECTIONS.map((section) => (
          <label key={section.id} className="flex items-center gap-1.5 text-xs text-foreground">
            <input
              type="checkbox"
              checked={selected.includes(section.id)}
              onChange={() => toggle(section.id)}
              className="size-4"
            />
            {section.label} <span className="text-faint">({section.covers})</span>
          </label>
        ))}
      </fieldset>

      <ErrorNotice message={error} />

      <div className="flex justify-end">
        <BusyButton
          type="button"
          busy={busy}
          disabled={!baseUrl.trim()}
          onClick={() => void run(baseUrl.trim(), token.trim() || null, selected)}
        >
          <BadgeCheck size={15} aria-hidden="true" /> Run conformance suite
        </BusyButton>
      </div>

      {report && (
        <div className="flex flex-col gap-3">
          <div className="flex flex-wrap items-center gap-2 text-xs text-muted">
            {report.verdict.state === "compatible" ? (
              <StatusPill tone="success">Compatible — {report.verdict.suiteRevision}</StatusPill>
            ) : (
              <StatusPill tone="danger">Not compatible</StatusPill>
            )}
            <span className="font-mono">{report.target}</span>
            {report.nodeSuiteRevision && report.nodeSuiteRevision !== report.suiteRevision && (
              <span className="flex items-center gap-1">
                <ShieldAlert size={13} aria-hidden="true" />
                node built against {report.nodeSuiteRevision}
              </span>
            )}
          </div>

          {report.verdict.state === "notCompatible" && (
            <ul className="list-disc pl-5 text-xs text-danger">
              {report.verdict.reasons.map((reason) => (
                <li key={reason}>{reason}</li>
              ))}
            </ul>
          )}

          {report.skippedOptionalSections.length > 0 && (
            <p className="text-xs text-muted">
              Optional sections not run: {report.skippedOptionalSections.join(", ")}
            </p>
          )}

          {report.sections.map((section) => (
            <section key={section.id} className="rounded-md border border-border p-3">
              <h4 className="mb-2 flex flex-wrap items-center gap-2 text-sm font-semibold text-foreground">
                {section.id}
                <span className="text-xs font-normal text-muted">
                  {section.requirement} · {section.covers}
                </span>
                <StatusPill tone={SECTION_TONE[section.status]}>
                  {SECTION_LABEL[section.status]}
                </StatusPill>
              </h4>
              {section.skipReason && <p className="text-xs text-muted">{section.skipReason}</p>}
              {section.checks.length > 0 && (
                <ul className="flex flex-col gap-1.5">
                  {section.checks.map((check) => (
                    <li key={check.id} className="flex flex-wrap items-baseline gap-2 text-xs">
                      <StatusPill tone={CHECK_TONE[check.status]}>{check.status}</StatusPill>
                      <span className="font-mono text-foreground">{check.id}</span>
                      <span className="text-muted">{check.detail}</span>
                    </li>
                  ))}
                </ul>
              )}
            </section>
          ))}
        </div>
      )}
    </section>
  );
}

export default RuntimeHubConformance;
