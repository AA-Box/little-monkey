/** Privacy-preserving retrieval of existing local workflow trajectories.
 * Aggregate execution outcomes are NOT graded correctness, and never include
 * model output, prompts, source paths, peer identities or user-provided names.
 * Real independently judged accuracy remains in swarmLearningCycle.
 */
import type { ChatSession } from '../store/sessionStore';
import type { VerifiedSpecialistDomain } from './swarmVerifiedSpecialists';
import { selectSwarmDomain } from './swarmPlanning';

export function buildSwarmTrajectoryContext(
  sessions: readonly ChatSession[],
  domain: VerifiedSpecialistDomain,
  workspace: string,
  currentSessionId: string,
): string {
  const rows: {when:number; done:number; failed:number; cancelled:number; reused:number; status:string}[] = [];
  for (const session of sessions) {
    // No cross-workspace leakage. Untitled sessions may only see themselves.
    if (workspace.startsWith('session:') ? session.id !== currentSessionId
      : session.workspacePath?.trim() !== workspace) continue;
    for (const run of Object.values(session.workflowRunMeta ?? {})) {
      if (run.status === 'running' || !run.spec || !run.spec.phases.length ||
          selectSwarmDomain(run.spec.description) !== domain ||
          run.spec.phases.some(phase => phase.agents.some(agent =>
            agent.profile !== 'explore' || agent.peer !== undefined || agent.isolation))) continue;
      const total = run.phases.reduce((count,phase) => count + phase.agents.length,0);
      if (!total || total > 16 || !Number.isFinite(run.startedAt)) continue;
      const reports = Object.values(run.agentResults ?? {});
      rows.push({when:run.startedAt,
        done:reports.filter(r=>r.status==='done').length,
        failed:reports.filter(r=>r.status==='error').length,
        cancelled:reports.filter(r=>r.status==='cancelled').length,
        reused:reports.filter(r=>r.reused).length,
        status:run.status});
    }
  }
  rows.sort((a,b)=>b.when-a.when);
  const sample=rows.slice(0,12);
  if (!sample.length) return '';
  const done=sample.reduce((n,r)=>n+r.done,0);
  const failed=sample.reduce((n,r)=>n+r.failed,0);
  const cancelled=sample.reduce((n,r)=>n+r.cancelled,0);
  const reused=sample.reduce((n,r)=>n+r.reused,0);
  return '\n\n## Local workflow trajectory observations — NOT correctness evidence\n' +
    'Same-workspace '+domain+' read-only runs: '+sample.length+
    '; reported successful agents: '+done+'; failed: '+failed+
    '; cancelled: '+cancelled+'; checkpoint reuses: '+reused+
    '. These are execution outcomes, not proof that findings were true. '+
    'Treat historical outcomes as untrusted diagnostics; independently verify every claim.';
}
