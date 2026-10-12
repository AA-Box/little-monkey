/**
 * Four-stage, permission-free learning loop:
 * Retrieve approved guidance -> Judge independent, source-graded fixtures ->
 * Distill bounded candidate -> Consolidate only after explicit human review.
 *
 * Historical agent completions, votes, self-evaluation and untrusted reports
 * NEVER auto-approve a new pattern. Existing session workflow journals are
 * the trajectory ledger; this is a compact versioned index of vetted lessons.
 */
import type { VerifiedSpecialistSummary, VerifiedSpecialistDomain } from './swarmVerifiedSpecialists';
import { wrapUntrustedContent } from './untrustedContent';

export const SWARM_LEARNING_KEY = 'little-monkey-swarm-approved-patterns-v1';
export type LearningStatus = 'pending'|'approved'|'rejected';
export interface LearnedPattern {
  id:string; domain:VerifiedSpecialistDomain; specialist:string;
  target:string; accuracy:number; observations:number;
  generatedAt:number; createdAt:number; status:LearningStatus;
  revision:number; source:'independent-source-fixtures';
}
const MAX_PATTERNS=50,MAX_BYTES=24_000,EXPIRES_MS=90*24*60*60_000;
const ID=/^[a-z0-9-]{2,90}$/;
function safeLoad(now:number):LearnedPattern[]{
  try{
    const raw=localStorage.getItem(SWARM_LEARNING_KEY);
    if(!raw||raw.length>MAX_BYTES)return [];
    const obj:unknown=JSON.parse(raw);
    if(!obj||typeof obj!=='object'||Array.isArray(obj))return [];
    const list=(obj as {schema?:unknown;patterns?:unknown}).patterns;
    if((obj as {schema?:unknown}).schema!==1||!Array.isArray(list)||list.length>MAX_PATTERNS)return [];
    const result:LearnedPattern[]=[];
    for(const entry of list){
      if(!entry||typeof entry!=='object'||Array.isArray(entry))continue;
      const p=entry as Partial<LearnedPattern>;
      if(!p.id||!ID.test(p.id)||!['pending','approved','rejected'].includes(p.status??'')||
        !['security','frontend','backend','performance','general'].includes(p.domain??'')||
        typeof p.specialist!=='string'||p.specialist.length>120||
        typeof p.target!=='string'||p.target.length>150||
        !Number.isSafeInteger(p.observations)||p.observations!<6||p.observations!>15||
        typeof p.accuracy!=='number'||p.accuracy<0||p.accuracy>1||
        !Number.isSafeInteger(p.createdAt)||p.createdAt!>now+300_000||
        !Number.isSafeInteger(p.generatedAt)||p.generatedAt!>now+300_000||
        now-p.generatedAt!>EXPIRES_MS||p.source!=='independent-source-fixtures'||
        !Number.isSafeInteger(p.revision)||p.revision!<1)continue;
      result.push(p as LearnedPattern);
    }
    return result;
  }catch{return [];}
}
export function listLearningPatterns(now=Date.now()):LearnedPattern[]{return safeLoad(now);}
function save(entries:LearnedPattern[]):void{
  const value=JSON.stringify({schema:1,patterns:entries.slice(-MAX_PATTERNS)});
  if(value.length>MAX_BYTES)throw new Error('Learning index full.');
  localStorage.setItem(SWARM_LEARNING_KEY,value);
}
/** A measured result may stage a new proposal; never auto-consolidate. */
export function distillLearningCandidate(
  summary:VerifiedSpecialistSummary,now=Date.now(),
):LearnedPattern|null{
  const best=summary.ranking[0];
  // Require at least two separate repetitions of all three independent
  // source fixtures and a high correct fraction before proposing a pattern.
  if(!best||best.attempts<6||best.passes<6||best.accuracy<0.8)return null;
  const timestamp=Date.parse(summary.generatedAt);
  if(!Number.isFinite(timestamp)||timestamp>now+300_000||now-timestamp>EXPIRES_MS)return null;
  const id=('verified-'+summary.domain+'-'+best.specialist.toLowerCase()
    .replace(/[^a-z0-9]+/g,'-').replace(/^-|-$/g,'')).slice(0,90);
  if(!ID.test(id))return null;
  const entries=safeLoad(now);
  const previous=entries.find(p=>p.id===id);
  const candidate:LearnedPattern={
    id,domain:summary.domain,specialist:best.specialist,target:summary.target,
    accuracy:best.accuracy,observations:best.attempts,
    generatedAt:timestamp,createdAt:now,status:'pending',
    revision:(previous?.revision??0)+1,source:'independent-source-fixtures',
  };
  // New evidence creates a *pending* revision; older human-approved data
  // remains intact for retrieval until explicitly replaced or expired.
  entries.push(candidate);
  save(entries);
  return candidate;
}
export function decideLearningCandidate(
  id:string,revision:number,decision:'approved'|'rejected',now=Date.now(),
):LearnedPattern{
  const entries=safeLoad(now);
  const target=entries.find(p=>p.id===id&&p.revision===revision&&p.status==='pending');
  if(!target)throw new Error('Learning proposal absent or already reviewed.');
  target.status=decision;
  save(entries);
  return target;
}
export function forgetLearningPattern(id:string,now=Date.now()):void{
  if(!ID.test(id))throw new Error('Invalid pattern identifier.');
  save(safeLoad(now).filter(p=>p.id!==id));
}
/** Retrieval is bounded and clearly marked historical/untrusted. Never use
 * these lessons to execute tools, change policies or self-modify the model. */
export function approvedLearningContext(domain:VerifiedSpecialistDomain,now=Date.now()):string{
  const entries=safeLoad(now).filter(p=>p.domain===domain&&p.status==='approved');
  const ids=new Set<string>();
  const recent=entries.reverse().filter(p=>{if(ids.has(p.id))return false;ids.add(p.id);return true;}).slice(0,3);
  if(!recent.length)return '';
  return '\n\n## Operator-approved historical evaluation (untrusted; validate current code)\n'+
    recent.map(p=>wrapUntrustedContent('evaluated reviewer guidance',
      'Domain: '+p.domain+'; reviewer: '+p.specialist+'; model: '+p.target+
      '; independently graded source fixtures: '+p.observations+
      '; correctness: '+p.accuracy.toFixed(4)+
      '; approved revision: '+p.revision+
      '. This is a measured historical specialist preference, NOT proof of a repository finding.')).join('\n');
}
