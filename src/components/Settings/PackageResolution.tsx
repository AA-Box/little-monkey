import { StatusPill } from "../ui";
import { surfaceClaimLabel, versionConstraintLabel, type DependencyProblem, type DependencyRequirement, type InstallPlan } from "../../lib/ecosystemClient";
import { useT } from "../../lib/i18n";

/** Renders a resolution problem with the ids, constraints and versions the
 *  backend named, so a refusal is readable without opening a log. */
function problemLabel(problem: DependencyProblem, t: (key: string, vars?: Record<string, string | number>) => string): string {
  const needs = (constraints: DependencyRequirement[]) =>
    constraints.map((entry) => `${entry.required_by} → ${versionConstraintLabel(entry.constraint)}`).join("; ");
  switch (problem.kind) {
    case "unsatisfiable":
      return t("EcosystemPackages.problemUnsatisfiable", {
        packageId: problem.package_id,
        needs: needs(problem.constraints),
        available: problem.available_versions.join(", ") || t("EcosystemPackages.none"),
      });
    case "pin_conflict":
      return t("EcosystemPackages.problemPinned", { packageId: problem.package_id, pinned: problem.pinned, needs: needs(problem.constraints) });
    case "disabled":
      return t("EcosystemPackages.problemDisabled", { packageId: problem.package_id, needs: needs(problem.constraints) });
    case "surface_collision":
      return t("EcosystemPackages.problemCollision", { claim: surfaceClaimLabel(problem.claim), packageIds: problem.package_ids.join(", ") });
    case "contract_mismatch":
      return t("EcosystemPackages.problemContract", {
        packageId: problem.package_id,
        required: versionConstraintLabel(problem.required),
        implemented: problem.implemented,
      });
    default:
      return t("EcosystemPackages.problemTooComplex", { packageId: problem.package_id });
  }
}

export function ResolutionSection({ plan }: { plan: InstallPlan }) {
  const { t } = useT();
  const dependencySteps = plan.steps.filter((step) => step.package_id !== plan.package_id);
  if (dependencySteps.length === 0 && plan.problems.length === 0) return null;
  return (
    <section className="mt-4">
      <h4 className="mb-2 text-xs font-semibold text-foreground">{t("EcosystemPackages.dependencies")}</h4>
      {dependencySteps.length > 0 && (
        <ul className="space-y-1.5">
          {dependencySteps.map((step) => (
            <li key={step.package_id} className="flex flex-wrap items-center gap-2 rounded-lg border border-border bg-surface-2 p-2 text-xs">
              <code className="text-foreground">{step.package_id}</code>
              <span className="text-faint">v{step.version}</span>
              <StatusPill tone={step.action === "already_installed" ? "success" : "warning"}>
                {t(`EcosystemPackages.planAction.${step.action}`)}
              </StatusPill>
              {step.required_by.length > 0 && (
                <span className="text-[11px] text-muted">{t("EcosystemPackages.requiredBy", { packages: step.required_by.join(", ") })}</span>
              )}
            </li>
          ))}
        </ul>
      )}
      {plan.problems.map((problem, index) => (
        <p key={`${problem.kind}:${index}`} className="mt-2 rounded-lg border border-danger/30 bg-danger-soft p-2 text-xs text-danger">
          {problemLabel(problem, t)}
        </p>
      ))}
    </section>
  );
}
