#!/usr/bin/env node
/**
 * Opt-in real provider billing reconciliation. Reads only official admin cost
 * endpoints (fixed HTTPS hosts), never accepts arbitrary URLs or stores keys.
 * Incomplete/duplicate pages, missing days and partial current months fail
 * closed. No estimate is presented as a provider bill.
 */
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync,renameSync,mkdirSync,openSync,closeSync,unlinkSync} from 'node:fs';
import {dirname,resolve,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';

const option=(name)=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1]??null};
const MONTH=/^(20\d\d)-(0[1-9]|1[0-2])$/;
const SAFE_ID=/^[A-Za-z0-9_-]{1,160}$/;
function fail(message){throw Error(message)}
function dates(month,now=new Date()){
 if(!MONTH.test(month))fail('Require --month YYYY-MM in UTC');
 const start=Date.parse(month+'-01T00:00:00Z');
 const [year,m]=month.split('-').map(Number);
 const monthEnd=Date.UTC(year,m,1);
 const today=Date.UTC(now.getUTCFullYear(),now.getUTCMonth(),now.getUTCDate());
 const end=Math.min(monthEnd,today);
 if(end<=start)fail('Billing period has no fully completed UTC days');
 const expected=Math.round((end-start)/86400000);
 if(expected<1||expected>31)fail('Invalid billing month interval');
 return {start,end,expected,final:end===monthEnd,month};
}
function micro(amount,scale=1000000){
 const value=typeof amount==='number'?String(amount):amount;
 if(typeof value!=='string'||!/^\d+(?:\.\d{1,12})?$/.test(value))
  fail('Official provider returned an invalid decimal cost');
 const [integer,fraction='']=value.split('.');
 const numerator=BigInt(integer+fraction.padEnd(12,'0'))*BigInt(scale);
 const micros=(numerator+500000000000n)/1000000000000n;
 if(micros>100000000000000n)fail('Official amount exceeds supported bound');
 return micros;
}
function parsePage(provider,body,range,seen){
 if(!body||!Array.isArray(body.data)||typeof body.has_more!=='boolean'||
    (body.has_more&&(!body.next_page||typeof body.next_page!=='string'))||
    body.data.length>180)fail('Provider response lacks complete pagination');
 let total=0n;
 for(const bucket of body.data){
  const start=provider==='openai'?bucket.start_time*1000:Date.parse(bucket.starting_at);
  const end=provider==='openai'?bucket.end_time*1000:Date.parse(bucket.ending_at);
  if(!Number.isSafeInteger(start)||!Number.isSafeInteger(end)||
    start<range.start||end>range.end||end-start!==86400000||
    start%86400000!==0||seen.has(start)||!Array.isArray(bucket.results))
   fail('Missing, duplicate, malformed or out-of-range billing bucket');
  seen.add(start);
  for(const row of bucket.results){
   if(provider==='openai'){
    if(row?.object!=='organization.costs.result'||
       row?.amount?.currency?.toLowerCase()!=='usd')
      fail('OpenAI returned non-cost or non-USD line item');
    total+=micro(row.amount.value);
   }else{
    if(row?.currency!=='USD')fail('Anthropic returned non-USD cost item');
    // Anthropic Cost Report amounts are decimal CENTS, not dollars.
    total+=micro(row.amount,10000);
   }
  }
 }
 return total;
}
export async function fetchOfficialCost({provider,month,token,scopeId=null,
  fetchImpl=fetch,now=new Date()}){
 if(!['openai','anthropic'].includes(provider))fail('Unsupported provider');
 if(typeof token!=='string'||token.length<10)fail('Admin API key not configured');
 if(scopeId!==null&&!SAFE_ID.test(scopeId))fail('Invalid account scope ID');
 const range=dates(month,now);
 const base=provider==='openai'?'https://api.openai.com/v1/organization/costs':
   'https://api.anthropic.com/v1/organizations/cost_report';
 const headers=provider==='openai'?{
   Authorization:'Bearer '+token,'Content-Type':'application/json',
 }:{'x-api-key':token,'anthropic-version':'2023-06-01',
   'User-Agent':'LittleMonkey-SwarmBilling/1.0'};
 const seen=new Set(),cursors=new Set();
 let page=null,total=0n;
 for(let iteration=0;iteration<35;iteration++){
  const u=new URL(base);
  if(provider==='openai'){
   u.searchParams.set('start_time',String(range.start/1000));
   u.searchParams.set('end_time',String(range.end/1000));
   u.searchParams.set('bucket_width','1d');
   u.searchParams.set('limit','31');
   if(scopeId)u.searchParams.append('api_key_ids[]',scopeId);
  }else{
   u.searchParams.set('starting_at',new Date(range.start).toISOString());
   u.searchParams.set('ending_at',new Date(range.end).toISOString());
   u.searchParams.set('bucket_width','1d');
   u.searchParams.set('limit','31');
   if(scopeId)u.searchParams.append('workspace_ids[]',scopeId);
  }
  if(page)u.searchParams.set('page',page);
  const ctrl=new AbortController();
  const timeout=setTimeout(()=>ctrl.abort(),30000);
  let response;
  try{response=await fetchImpl(u.toString(),{method:'GET',headers,signal:ctrl.signal,redirect:'error'});}
  catch{fail('Provider billing API request failed (no amount was accepted)');}
  finally{clearTimeout(timeout)}
  if(!response.ok)fail('Provider billing API refused the request (HTTP '+response.status+')');
  let json;
  try{json=await response.json()}catch{fail('Malformed provider billing JSON')}
  total+=parsePage(provider,json,range,seen);
  if(!json.has_more){
   if(seen.size!==range.expected)fail('Provider omitted one or more complete daily billing buckets');
   const billedMicro=Number(total);
   if(!Number.isSafeInteger(billedMicro))fail('Billing amount exceeds safe precision');
   return {version:1,source:'official_provider_admin_cost_api',provider,month,
    scope:{kind:scopeId===null?'whole_organization':provider==='openai'?'api_key_id':'workspace_id',
      id:scopeId},fromUtc:new Date(range.start).toISOString(),
    toUtc:new Date(range.end).toISOString(),final:range.final,
    days:seen.size,currency:'USD',billedMicroUsd:billedMicro,
    fetchedAt:new Date(now).toISOString()};
  }
  if(typeof json.next_page!=='string'||json.next_page.length>500||
    cursors.has(json.next_page))fail('Malformed/replayed provider billing pagination');
  cursors.add(json.next_page);page=json.next_page;
 }
 fail('Exceeded bounded billing pagination');
}
export function reconcileObservedBilling(bill,ledger){
 if(!ledger||!Array.isArray(ledger.entries)||ledger.entries.length>100000)
  fail('Require a bounded exported Little Monkey cost ledger');
 if(bill.scope.kind==='whole_organization')return {
  state:'not_comparable',reason:'Organization total includes unrelated services or API keys'};
 let known=0,unknown=0,estimatedMicro=0;
 for(const row of ledger.entries){
  if(!row||typeof row!=='object'||!Number.isSafeInteger(row.occurredAtMs))continue;
  const rowMonth=new Date(row.occurredAtMs).toISOString().slice(0,7);
  const key=typeof row.targetKey==='string'?row.targetKey:'';
  if(rowMonth!==bill.month||!key.startsWith('provider:'+bill.provider+':'))continue;
  // A model rate is not a provider invoice; all calls in the same filtered
  // workspace/API key must be attributable before judging drift.
  if(row.costUsd===null||!Number.isFinite(row.costUsd)||row.costUsd<0){
   unknown++;continue;
  }
  const rounded=Math.round(row.costUsd*1_000_000);
  if(!Number.isSafeInteger(rounded))fail('Invalid local measured model estimate');
  estimatedMicro+=rounded;known++;
 }
 if(!known||unknown||!bill.final)return {
  state:'not_comparable',knownCalls:known,unknownCalls:unknown,
  reason:!bill.final?'Current-month provider cost is provisional':
    unknown?'Some model calls have no estimated price':'No source model calls were metered'};
 // A filter scoped to an admin API key or workspace is only safely comparable
 // if the ledger is explicitly tagged as that very billing scope.
 if(ledger.scope?.kind!==bill.scope.kind||ledger.scope?.id!==bill.scope.id)
  return {state:'not_comparable',reason:'Ledger lacks exact provider billing scope attribution'};
 const drift=bill.billedMicroUsd-estimatedMicro;
 return {state:Math.abs(drift)<=Math.max(100,Math.round(bill.billedMicroUsd*.05))?
   'within_tolerance':'variance_detected',
   knownCalls:known,unknownCalls:0,estimatedMicroUsd:estimatedMicro,
   billedMicroUsd:bill.billedMicroUsd,driftMicroUsd:drift,
   exactScopeCompared:true};
}
function atomicReceipt(filepath,record){
 const target=resolve(filepath);
 mkdirSync(dirname(target),{recursive:true,mode:0o700});
 const temp=target+'.new-'+process.pid;
 let fd;
 try{fd=openSync(temp,'wx',0o600);
  writeFileSync(fd,JSON.stringify(record,null,2)+'\n',{encoding:'utf8'});
  closeSync(fd);fd=null;renameSync(temp,target);
 }catch(error){if(fd!==null&&fd!==undefined)closeSync(fd);
  try{unlinkSync(temp)}catch{}throw error}
}
async function main(){
 const provider=option('--provider'),month=option('--month'),output=option('--output');
 if(!provider||!month||!output)fail('Require --provider openai|anthropic --month YYYY-MM --output ABSOLUTE_PATH');
 if(!isAbsolute(output))fail('Require absolute output path');
 const token=process.env[provider==='openai'?'OPENAI_ADMIN_KEY':'ANTHROPIC_ADMIN_KEY'];
 const scopeId=option('--scope-id');
 const bill=await fetchOfficialCost({provider,month,token,scopeId});
 const ledgerPath=option('--ledger');
 const compare=ledgerPath?reconcileObservedBilling(bill,JSON.parse(readFileSync(ledgerPath,'utf8'))):
  {state:'not_comparable',reason:'No measured ledger supplied'};
 if(process.argv.includes('--require-reconciled')&&compare.state!=='within_tolerance')
  fail('Official provider costs are not reconciled to a complete matching model-call ledger');
 const receipt={...bill,comparison:compare};
 receipt.sha256=createHash('sha256').update(JSON.stringify(receipt)).digest('hex');
 atomicReceipt(output,receipt);
 console.log(JSON.stringify({status:'provider_cost_readback_recorded',provider,month,
  final:bill.final,comparison:compare.state,receipt:resolve(output)}));
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)){
 main().catch(error=>{console.error('NOT_RECONCILED: '+String(error.message).slice(0,240));process.exitCode=1});
}
