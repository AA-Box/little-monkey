/**
 * Real MLX fused-model deployment with live inference, durable fail-closed
 * handoff and automatic rollback. Only an operator who has already verified
 * the native weights and clicked Activate can run this. No background model
 * switching or implicit training. A pending crash journal blocks ordinary
 * model routing until the previous model is recovered.
 */
import type {ModelInfo} from '../store/modelStore';
export const MLX_PROMOTION_JOURNAL='little-monkey-mlx-fusion-promotion-v1';
const MAX_AGE=7*24*60*60*1000;
export interface PromotionJournal {
 schema:1;candidatePath:string;previousPath:string|null;
 previousWasRunning:boolean;createdAt:number;
}
export interface PromotionRuntime {
 installed():ModelInfo[];
 active():ModelInfo|null;
 runningPath():string|null;
 otherLocalRunning():boolean;
 activeProvider():string;
 start(model:ModelInfo):Promise<void>;
 stop():Promise<void>;
 clearInactive(previous:ModelInfo|null):void;
 probe(path:string):Promise<{modelPath:string;nonemptyOutputChars:number}>;
}
export interface PromotionStorage {
 getItem(key:string):string|null;
 setItem(key:string,value:string):void;
 removeItem(key:string):void;
}
let promoting=false;
export function pendingMlxPromotion(storage:PromotionStorage):PromotionJournal|null{
 const raw=storage.getItem(MLX_PROMOTION_JOURNAL);
 if(!raw)return null;
 try{
  const value:unknown=JSON.parse(raw);
  if(!value||typeof value!=='object'||Array.isArray(value))
   throw Error('Malformed MLX promotion journal');
  const x=value as Partial<PromotionJournal>;
  if(x.schema!==1||typeof x.candidatePath!=='string'||x.candidatePath.length>2048||
     !x.candidatePath||x.previousPath!==null&&typeof x.previousPath!=='string'||
     typeof x.previousWasRunning!=='boolean'||!Number.isSafeInteger(x.createdAt)||
     x.createdAt!<=0||x.createdAt!>Date.now()+300000||
     Date.now()-x.createdAt!>MAX_AGE)
    throw Error('Invalid or stale MLX promotion journal');
  return x as PromotionJournal;
 }catch{throw Error('Unrecognized pending MLX promotion. Recovery must be reviewed manually; execution is blocked.');}
}
async function rollback(j:PromotionJournal,runtime:PromotionRuntime,storage:PromotionStorage):Promise<void>{
 const previous=j.previousPath
   ?runtime.installed().find(x=>x.path===j.previousPath&&x.runtime==='mlx')
   :null;
 if(j.previousPath&&!previous)throw Error('Previous verified MLX model no longer registered; refusing unsafe fallback.');
 if(j.previousWasRunning&&previous)await runtime.start(previous);
 else {
  if(runtime.runningPath())await runtime.stop();
  runtime.clearInactive(previous??null);
 }
 storage.removeItem(MLX_PROMOTION_JOURNAL);
}
export async function recoverInterruptedMlxPromotion(runtime:PromotionRuntime,storage:PromotionStorage):Promise<boolean>{
 if(promoting)return false;
 const journal=pendingMlxPromotion(storage);
 if(!journal)return false;
 promoting=true;
 try{await rollback(journal,runtime,storage);return true;}finally{promoting=false;}
}
export async function promoteVerifiedMlxFusion(
 candidate:ModelInfo,runtime:PromotionRuntime,storage:PromotionStorage,
):Promise<{modelPath:string;nonemptyOutputChars:number}>{
 if(promoting)throw Error('Another model promotion is still running.');
 if(pendingMlxPromotion(storage))throw Error('Recover the previous pending model promotion first.');
 if(!candidate.path||candidate.runtime!=='mlx'||!candidate.is_external||
    !runtime.installed().some(x=>x.id===candidate.id&&x.path===candidate.path&&x.runtime==='mlx'))
   throw Error('Candidate must be an already registered, verified external MLX model.');
 if(runtime.activeProvider()!=='local')
   throw Error('Switch back to the local MLX provider before live deployment.');
 const prior=runtime.active();
 if(runtime.otherLocalRunning() || (prior?.path && prior.runtime!=='mlx'))
   throw Error('Stop the running non-MLX model and select a local MLX model (or no local model) before promotion.');
 const running=runtime.runningPath();
 if(running&&(!prior||prior.path!==running||prior.runtime!=='mlx'))
   throw Error('Running local model and selected model disagree; refusing promotion.');
 if(prior?.path===candidate.path)throw Error('Candidate already selected; do not re-promote it.');
 const journal:PromotionJournal={schema:1,candidatePath:candidate.path,
  previousPath:prior?.path??null,previousWasRunning:!!running,createdAt:Date.now()};
 // Fails closed if local storage cannot durably hold rollback instructions.
 storage.setItem(MLX_PROMOTION_JOURNAL,JSON.stringify(journal));
 promoting=true;
 try{
  await runtime.start(candidate);
  const report=await runtime.probe(candidate.path);
  if(report.modelPath!==candidate.path||!Number.isSafeInteger(report.nonemptyOutputChars)||
     report.nonemptyOutputChars<1||report.nonemptyOutputChars>16384)
    throw Error('Native inference did not validate the exact candidate model.');
  storage.removeItem(MLX_PROMOTION_JOURNAL);
  return report;
 }catch(error){
  try{await rollback(journal,runtime,storage);}
  catch(recoveryError){
   throw Error('Candidate failed; automatic rollback also failed. Normal model routing and execution is blocked. '+
     String(recoveryError));
  }
  throw Error('Candidate failed real inference; previous local model restored: '+String(error));
 }finally{promoting=false;}
}
