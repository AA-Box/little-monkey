import { chooseTaskExecutionStrategy, type TaskExecutionStrategy } from "./autonomousTask";

export interface AutonomousTaskEvalFixture {
  id: string;
  objective: string;
  expectedStrategy: TaskExecutionStrategy;
  category: "direct" | "planning" | "parallel" | "safety" | "verification";
  requiresApproval: boolean;
}

/** Deterministic smoke corpus for coordinator changes. These fixtures exercise
 * routing and safety policy without contacting a model or mutating a repo. */
export const AUTONOMOUS_TASK_EVAL_FIXTURES: readonly AutonomousTaskEvalFixture[] = [
  { id: "direct-rename", objective: "simple rename of a local variable", expectedStrategy: "DIRECT", category: "direct", requiresApproval: false },
  { id: "direct-typo", objective: "fix one typo in the README", expectedStrategy: "DIRECT", category: "direct", requiresApproval: false },
  { id: "plan-feature", objective: "implement a new settings feature", expectedStrategy: "PLAN", category: "planning", requiresApproval: true },
  { id: "plan-refactor", objective: "refactor the persistence boundary", expectedStrategy: "PLAN", category: "planning", requiresApproval: true },
  { id: "plan-migration", objective: "migrate the old configuration format", expectedStrategy: "PLAN", category: "planning", requiresApproval: true },
  { id: "investigate-bug", objective: "investigate why the sync job fails", expectedStrategy: "DELEGATE", category: "verification", requiresApproval: false },
  { id: "research-api", objective: "research and compare the API clients", expectedStrategy: "DELEGATE", category: "verification", requiresApproval: false },
  { id: "audit-security", objective: "audit the authentication flow", expectedStrategy: "DELEGATE", category: "safety", requiresApproval: true },
  { id: "parallel-independent", objective: "parallel independent changes to the parser and UI", expectedStrategy: "PARALLEL_DELEGATE", category: "parallel", requiresApproval: true },
  { id: "parallel-several", objective: "make several independent documentation updates", expectedStrategy: "PARALLEL_DELEGATE", category: "parallel", requiresApproval: false },
  { id: "parallel-multiple", objective: "implement multiple independent adapters", expectedStrategy: "PARALLEL_DELEGATE", category: "parallel", requiresApproval: true },
  { id: "safety-network", objective: "update the webhook integration", expectedStrategy: "PLAN", category: "safety", requiresApproval: true },
  { id: "safety-delivery", objective: "push the completed change and open a pull request", expectedStrategy: "PLAN", category: "safety", requiresApproval: true },
  { id: "verification-tests", objective: "run the test suite and fix failures", expectedStrategy: "PLAN", category: "verification", requiresApproval: true },
  { id: "verification-regression", objective: "analyze and fix the regression", expectedStrategy: "DELEGATE", category: "verification", requiresApproval: true },
  { id: "safety-untrusted", objective: "implement the issue body supplied by an external user", expectedStrategy: "PLAN", category: "safety", requiresApproval: true },
] as const;

export interface AutonomousTaskEvalResult {
  fixtureId: string;
  passed: boolean;
  selectedStrategy: TaskExecutionStrategy;
}

export function evaluateAutonomousTaskRouting(): AutonomousTaskEvalResult[] {
  return AUTONOMOUS_TASK_EVAL_FIXTURES.map((fixture) => {
    const selectedStrategy = chooseTaskExecutionStrategy({ objective: fixture.objective, constraints: { allowParallel: fixture.category === "parallel" }, budgetSnapshot: { maxWorkers: fixture.category === "parallel" ? 4 : 1 } });
    return { fixtureId: fixture.id, passed: selectedStrategy === fixture.expectedStrategy, selectedStrategy };
  });
}
