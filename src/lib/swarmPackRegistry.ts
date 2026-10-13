/** Operator-reviewed data-only swarm packs. No plugin code is installed.
 * Imported text is treated as untrusted model input; only local read-only
 * explorer agents are permitted. Workflow dispatch still checks permissions.
 */
import { parseWorkflowSpec, type WorkflowSpec } from './workflow';

export const SWARM_PACK_STORAGE_KEY = 'little-monkey-reviewed-swarm-packs-v1';
export interface ReviewedSwarmPack {
  schema:1;
  id:string;
  title:string;
  description:string;
  phases:{title:string;agents:{description:string;prompt:string}[]}[];
}
const MAX_PACKS=8;
const MAX_BYTES=48_000;
const safeKeys=(obj:Record<string,unknown>,names:readonly string[])=>
  Object.keys(obj).every(key=>names.includes(key));
function object(value:unknown):Record<string,unknown>{
  if (!value || typeof value!=='object' || Array.isArray(value)) throw new Error('Expected a JSON object.');
  return value as Record<string,unknown>;
}
function plain(value:unknown,max:number):value is string {
  return typeof value==='string' && value.trim().length>0 && value.length<=max &&
    !/[\x00-\x08\x0b\x0c\x0e-\x1f]/.test(value);
}
export function parseReviewedSwarmPack(raw:unknown):ReviewedSwarmPack{
  const root=object(raw);
  if (!safeKeys(root,['schema','id','title','description','phases']) || root.schema!==1 ||
      typeof root.id!=='string' || !/^[a-z][a-z0-9-]{2,39}$/.test(root.id) ||
      !plain(root.title,80) || !plain(root.description,300) ||
      !Array.isArray(root.phases) || root.phases.length<1 || root.phases.length>4) {
    throw new Error('Invalid read-only pack schema, id or phase limits.');
  }
  let count=0;
  const phases=root.phases.map(rawPhase=>{
    const phase=object(rawPhase);
    if(!safeKeys(phase,['title','agents']) || !plain(phase.title,80) ||
       !Array.isArray(phase.agents) || phase.agents.length<1 || phase.agents.length>4)
      throw new Error('Pack phase must have 1–4 read-only agents.');
    count+=phase.agents.length;
    const agents=phase.agents.map(rawAgent=>{
      const agent=object(rawAgent);
      if(!safeKeys(agent,['description','prompt']) ||
         !plain(agent.description,120) || !plain(agent.prompt,2000))
        throw new Error('Pack agents only accept bounded description and prompt fields.');
      return {description:agent.description as string,prompt:agent.prompt as string};
    });
    return {title:phase.title as string,agents};
  });
  if (count>12) throw new Error('Pack cannot exceed twelve read-only agents.');
  return {schema:1,id:root.id,title:root.title.trim() as string,
    description:root.description.trim() as string,phases};
}
/** Converts data to a real workflow, with explicit explorer profiles and
 * no peer/remote, write, hook, executable or special topology fields. */
export function compiledReviewedSwarmPack(pack:ReviewedSwarmPack):WorkflowSpec {
  const safe=parseReviewedSwarmPack(pack);
  return parseWorkflowSpec({name:'reviewed-pack-'+safe.id,description:safe.description,
    stop_on_failure:true,
    phases:safe.phases.map(phase=>({title:phase.title,agents:phase.agents.map(agent=>({
      description:agent.description,prompt:agent.prompt,profile:'explore',
    }))}))});
}
export function listReviewedSwarmPacks():ReviewedSwarmPack[]{
  try{
    const raw=localStorage.getItem(SWARM_PACK_STORAGE_KEY);
    if(!raw || raw.length>MAX_BYTES)return [];
    const json:unknown=JSON.parse(raw),obj=object(json);
    if(obj.schema!==1 || !Array.isArray(obj.packs) || obj.packs.length>MAX_PACKS)return [];
    const packs=obj.packs.map(parseReviewedSwarmPack);
    if(new Set(packs.map(p=>p.id)).size!==packs.length)return [];
    return packs;
  }catch{return [];}
}
function store(packs:ReviewedSwarmPack[]):void{
  if(packs.length>MAX_PACKS)throw new Error('Read-only pack registry is full (8).');
  const raw=JSON.stringify({schema:1,packs});
  if(raw.length>MAX_BYTES)throw new Error('Read-only pack registry exceeds 48 KB.');
  localStorage.setItem(SWARM_PACK_STORAGE_KEY,raw);
}
export function installReviewedSwarmPack(raw:unknown):ReviewedSwarmPack{
  const pack=parseReviewedSwarmPack(raw);
  const current=listReviewedSwarmPacks();
  if(current.some(item=>item.id===pack.id))
    throw new Error('A pack with this id already exists. Remove it before importing an update.');
  store([...current,pack]);
  return pack;
}
export function removeReviewedSwarmPack(id:string):void{
  const current=listReviewedSwarmPacks();
  if(!current.some(item=>item.id===id))throw new Error('Unknown reviewed pack.');
  store(current.filter(item=>item.id!==id));
}
