import {test} from 'node:test';
import assert from 'node:assert/strict';
import {fetchOfficialCost,reconcileObservedBilling} from './swarm-provider-billing.mjs';
const now=new Date('2026-10-03T12:00:00Z');
const start=Date.parse('2026-10-01T00:00:00Z');
const day=86400000;
const openBucket=(d,value)=>({object:'bucket',start_time:(start+d*day)/1000,
 end_time:(start+(d+1)*day)/1000,results:[
  {object:'organization.costs.result',amount:{value,currency:'usd'}},
 ]});
const anthBucket=(d,value)=>({starting_at:new Date(start+d*day).toISOString(),
 ending_at:new Date(start+(d+1)*day).toISOString(),results:[
  {amount:value,currency:'USD'},
 ]});
const response=body=>({ok:true,status:200,json:async()=>body});
test('real OpenAI pagination and exact micro-USD conversion with admin key isolation',async()=>{
 let calls=0;
 const result=await fetchOfficialCost({provider:'openai',month:'2026-10',
  token:'admin-key-secret',scopeId:'key_123',now,fetchImpl:async(url,opts)=>{
   calls++;
   assert.equal(new URL(url).origin,'https://api.openai.com');
   assert.ok(!url.includes('admin-key-secret'));
   assert.equal(opts.headers.Authorization,'Bearer admin-key-secret');
   assert.equal(new URL(url).searchParams.get('api_key_ids[]'),'key_123');
   if(calls===1)return response({data:[openBucket(0,1.000001)],has_more:true,next_page:'page2'});
   assert.equal(new URL(url).searchParams.get('page'),'page2');
   return response({data:[openBucket(1,2.5)],has_more:false,next_page:null});
  }});
 assert.equal(calls,2);
 assert.equal(result.billedMicroUsd,3500001);
 assert.equal(result.days,2);
 assert.equal(result.final,false);
 assert.deepEqual(result.scope,{kind:'api_key_id',id:'key_123'});
});
test('Anthropic fractional CENTS are not misreported as dollars',async()=>{
 const report=await fetchOfficialCost({provider:'anthropic',month:'2026-10',
  token:'admin-key-secret',scopeId:'wrk_1',now,fetchImpl:async(url,opts)=>{
   assert.equal(new URL(url).origin,'https://api.anthropic.com');
   assert.equal(opts.headers['x-api-key'],'admin-key-secret');
   return response({data:[anthBucket(0,'123.78912'),anthBucket(1,'0')],has_more:false,next_page:null});
  }});
 assert.equal(report.billedMicroUsd,1237891);
});
test('must reject missing, repeated, malformed and unbounded provider pages',async()=>{
 const input={provider:'openai',month:'2026-10',token:'admin-key-secret',now};
 for(const page of [
  {data:[openBucket(0,1)],has_more:false,next_page:null},
  {data:[openBucket(0,1),openBucket(0,1)],has_more:false,next_page:null},
  {data:[openBucket(0,-1),openBucket(1,1)],has_more:false,next_page:null},
  {data:[openBucket(0,1),{...openBucket(1,1),end_time:0}],has_more:false,next_page:null},
  {data:[openBucket(0,1)],has_more:true,next_page:null},
 ]){
  await assert.rejects(fetchOfficialCost({...input,fetchImpl:async()=>response(page)}));
 }
 await assert.rejects(fetchOfficialCost({...input,fetchImpl:async()=>({ok:false,status:403})}));
});
test('reconciliation rejects organization-wide attribution and unscoped ledger claims',()=>{
 const bill={provider:'openai',month:'2026-09',final:true,billedMicroUsd:1000000,
  scope:{kind:'api_key_id',id:'key1'}};
 const ledger={scope:bill.scope,entries:[{targetKey:'provider:openai:model',
  occurredAtMs:Date.parse('2026-09-10T12:00:00Z'),costUsd:0.99}]};
 assert.equal(reconcileObservedBilling(bill,ledger).state,'within_tolerance');
 assert.equal(reconcileObservedBilling({...bill,billedMicroUsd:1200000},ledger).state,'variance_detected');
 assert.equal(reconcileObservedBilling({...bill,scope:{kind:'whole_organization',id:null}},ledger).state,'not_comparable');
 assert.equal(reconcileObservedBilling(bill,{...ledger,scope:{kind:'api_key_id',id:'other'}}).state,'not_comparable');
 assert.equal(reconcileObservedBilling(bill,{...ledger,entries:[{...ledger.entries[0],costUsd:null}]}).state,'not_comparable');
});
