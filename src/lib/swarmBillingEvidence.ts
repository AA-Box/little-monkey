/**
 * Optional provider-admin-cost reconciliation gate. These are provider API
 * reported costs, not final tax/credit invoices. Missing, stale, mismatched,
 * or unscoped evidence blocks cost-based rerouting when operator opts in.
 */
import {isTauri} from '@tauri-apps/api/core';
import {appDataDir,join} from '@tauri-apps/api/path';
import {readTextFile,writeTextFile} from '@tauri-apps/plugin-fs';
import {useCostControlStore,providerIdOfTargetKey} from '../store/costControlStore';

export type MeteredBillingProvider='openai'|'anthropic';
const PROVIDERS:readonly MeteredBillingProvider[]=['openai','anthropic'];
const GUARD='little-monkey-swarm-official-billing-guard-v1';
const scopeKey=(p:MeteredBillingProvider)=>'little-monkey-swarm-billing-scope-'+p;
const cacheKey=(p:MeteredBillingProvider)=>'little-monkey-swarm-billing-admin-receipt-'+p;
const ID=/^[A-Za-z0-9_-]{1,160}$/;
const HASH=/^[0-9a-f]{64}$/;
const MONTH=/^20\d\d-(0[1-9]|1[0-2])$/;
const MAX_AGE=36*60*60_000;

export function billingGuardEnabled():boolean{
 try{return localStorage.getItem(GUARD)==='true'}catch{return false}
}
export function setBillingGuardEnabled(enabled:boolean):void{
 localStorage.setItem(GUARD,enabled?'true':'false');
}
export function billingScope(provider:MeteredBillingProvider):string|null{
 try{const s=localStorage.getItem(scopeKey(provider));return s&&ID.test(s)?s:null}
 catch{return null}
}
export function setBillingScope(provider:MeteredBillingProvider,id:string):void{
 if(id!==''&&!ID.test(id))throw new Error('Invalid official billing API key/workspace scope ID');
 if(id)localStorage.setItem(scopeKey(provider),id);
 else localStorage.removeItem(scopeKey(provider));
 localStorage.removeItem(cacheKey(provider));
}
function lastUtcMonth(now:number):string{
 const date=new Date(now);
 return new Date(Date.UTC(date.getUTCFullYear(),date.getUTCMonth()-1,1))
   .toISOString().slice(0,7);
}
export interface ProviderBillingEvidence {
 provider:MeteredBillingProvider;
 month:string;
 scope:{kind:'api_key_id'|'workspace_id';id:string};
 billedMicroUsd:number;
 fetchedAt:string;
 comparison:{state:'within_tolerance';exactScopeCompared:true;
  estimatedMicroUsd:number;driftMicroUsd:number;knownCalls:number;unknownCalls:0};
}
export function validateProviderBillingEvidence(
 raw:unknown,provider:MeteredBillingProvider,scope:string,now=Date.now(),
):ProviderBillingEvidence{
 if(!raw||typeof raw!=='object'||Array.isArray(raw))throw Error('No official billing receipt');
 const v=raw as Record<string,any>;
 if(v.version!==1||v.source!=='official_provider_admin_cost_api'||
    v.provider!==provider||v.currency!=='USD'||v.final!==true||
    !MONTH.test(v.month)||v.month!==lastUtcMonth(now)||
    v.scope?.kind!==(provider==='openai'?'api_key_id':'workspace_id')||
    v.scope?.id!==scope||!ID.test(scope)||
    !Number.isSafeInteger(v.billedMicroUsd)||v.billedMicroUsd<0||
    !Number.isSafeInteger(v.days)||v.days<28||v.days>31||
    typeof v.sha256!=='string'||!HASH.test(v.sha256)||
    typeof v.fetchedAt!=='string'||
    !Number.isFinite(Date.parse(v.fetchedAt))||
    Date.parse(v.fetchedAt)>now+300000||
    now-Date.parse(v.fetchedAt)>MAX_AGE||
    v.comparison?.state!=='within_tolerance'||
    v.comparison?.exactScopeCompared!==true||
    !Number.isSafeInteger(v.comparison.estimatedMicroUsd)||
    !Number.isSafeInteger(v.comparison.driftMicroUsd)||
    !Number.isSafeInteger(v.comparison.knownCalls)||v.comparison.knownCalls<1||
    v.comparison.unknownCalls!==0||
    v.comparison.estimatedMicroUsd+v.comparison.driftMicroUsd!==v.billedMicroUsd||
    Math.abs(v.comparison.driftMicroUsd)>
      Math.max(100,Math.round(v.billedMicroUsd*0.05))){
   throw Error('Billing is stale, incomplete, unscoped, mismatched, or outside tolerance');
 }
 return v as unknown as ProviderBillingEvidence;
}
export function billingGuardAllowsKeys(keys:readonly string[],now=Date.now()):boolean{
 if(!billingGuardEnabled())return true;
 try{
  for(const key of keys){
   const id=providerIdOfTargetKey(key);
   if(id===null)continue;
   if(id!=='openai'&&id!=='anthropic')return false;
   const p=id as MeteredBillingProvider,scope=billingScope(p);
   if(!scope)return false;
   const raw=localStorage.getItem(cacheKey(p));
   if(!raw)return false;
   const verified=validateProviderBillingEvidence(JSON.parse(raw),p,scope,now);
   // Recompute local source usage again on EVERY routing decision.
   // A cached admin receipt cannot be paired with arbitrary edited totals or
   // a new set of calls, and the operator must account for ALL recorded
   // provider calls in this closed calendar month.
   const usage=useCostControlStore.getState().entries.filter(row=>{
     if(providerIdOfTargetKey(row.targetKey)!==p)return false;
     if(!Number.isSafeInteger(row.occurredAtMs))return false;
     return new Date(row.occurredAtMs).toISOString().slice(0,7)===verified.month;
   });
   const known=usage.filter(row=>row.costUsd!==null&&
     Number.isFinite(row.costUsd)&&row.costUsd>=0);
   if(known.length!==usage.length||known.length!==verified.comparison.knownCalls)
     return false;
   const total=known.reduce((n,row)=>n+Math.round(row.costUsd!*1_000_000),0);
   if(!Number.isSafeInteger(total)||total!==verified.comparison.estimatedMicroUsd)
     return false;
  }
  return true;
 }catch{return false}
}
export async function synchronizeOfficialBilling():Promise<void>{
 if(!billingGuardEnabled()||!isTauri())return;
 const root=await appDataDir();
 for(const provider of PROVIDERS){
  const scope=billingScope(provider);
  if(!scope){localStorage.removeItem(cacheKey(provider));continue;}
  try{
   const path=await join(root,'swarm-billing-'+provider+'-latest.json');
   const raw=await readTextFile(path);
   if(raw.length>16000)throw Error('Official provider receipt too large');
   const parsed=JSON.parse(raw) as Record<string,unknown>;
   const {sha256,...unsigned}=parsed;
   // Node writes sha256 over the exact canonical JSON serialization before
   // appending the digest. Validate byte integrity on desktop import too.
   // This is corruption detection; it is NOT a digital signature of the
   // admin provider or a proof of settled invoice payment.
   const bytes=new TextEncoder().encode(JSON.stringify(unsigned));
   const digest=await crypto.subtle.digest('SHA-256',bytes);
   const actual=Array.from(new Uint8Array(digest)).map(byte=>
     byte.toString(16).padStart(2,'0')).join('');
   if(actual!==sha256)throw Error('Corrupted provider admin cost receipt');
   validateProviderBillingEvidence(parsed,provider,scope);
   localStorage.setItem(cacheKey(provider),JSON.stringify(parsed));
  }catch{localStorage.removeItem(cacheKey(provider))}
 }
}
/** Operator-scoped model estimates, with no prompts or provider credentials. */
export async function exportMeteredBillingLedgers():Promise<void>{
 if(!billingGuardEnabled()||!isTauri())return;
 const root=await appDataDir(),entries=useCostControlStore.getState().entries;
 for(const provider of PROVIDERS){
  const scope=billingScope(provider);
  if(!scope)continue;
  const filtered=entries.filter(row=>providerIdOfTargetKey(row.targetKey)===provider)
   .slice(-5000).map(row=>({targetKey:row.targetKey,occurredAtMs:row.occurredAtMs,
    costUsd:row.costUsd,usage:row.usage}));
  const path=await join(root,'swarm-cost-ledger-'+provider+'.json');
  const kind=provider==='openai'?'api_key_id':'workspace_id';
  // Operator declares scope. Local telemetry alone cannot prove which
  // API key or provider workspace actually authorized each model call.
  await writeTextFile(path,JSON.stringify({version:1,scope:{kind,id:scope},
   source:'operator_scoped_local_model_usage',entries:filtered}));
 }
}
export function startSwarmBillingLedgerAndReceiptReader():()=>void{
 if(!isTauri()||typeof window==='undefined')return ()=>{};
 let busy=false;
 const update=()=>{
  if(busy||!billingGuardEnabled())return;
  busy=true;
  void exportMeteredBillingLedgers().then(synchronizeOfficialBilling)
   .catch(()=>undefined).finally(()=>{busy=false});
 };
 update();
 const timer=window.setInterval(update,60_000);
 window.addEventListener('focus',update);
 return ()=>{window.clearInterval(timer);window.removeEventListener('focus',update)};
}
