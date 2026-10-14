// @vitest-environment jsdom
import {afterEach,beforeEach,describe,expect,it,vi} from 'vitest';
import {webcrypto} from 'node:crypto';
vi.mock('@tauri-apps/api/core',()=>({isTauri:()=>true}));
vi.mock('@tauri-apps/api/path',()=>({appDataDir:async()=>'/tmp/app',join:async(...s:string[])=>s.join('/')}));
vi.mock('@tauri-apps/plugin-fs',()=>({readTextFile:vi.fn(),writeTextFile:vi.fn()}));
import {
 billingGuardAllowsKeys,billingGuardEnabled,billingScope,
 setBillingGuardEnabled,setBillingScope,synchronizeOfficialBilling,
 validateProviderBillingEvidence,
} from './swarmBillingEvidence';
import {useCostControlStore} from '../store/costControlStore';
import {readTextFile} from '@tauri-apps/plugin-fs';
const NOW=Date.parse('2026-10-14T10:00:00Z');
const payload=()=>({
 version:1,source:'official_provider_admin_cost_api',provider:'openai',month:'2026-09',
 scope:{kind:'api_key_id',id:'key_1'},currency:'USD',final:true,
 billedMicroUsd:1000000,days:30,sha256:'b'.repeat(64),
 fetchedAt:new Date(NOW-60000).toISOString(),
 comparison:{state:'within_tolerance',exactScopeCompared:true,
  estimatedMicroUsd:990000,driftMicroUsd:10000,knownCalls:3,unknownCalls:0},
});
async function mockOfficialReceipt(receipt=payload()):Promise<void>{
 const {sha256:_ignored,...unsigned}=receipt;
 const digest=await webcrypto.subtle.digest('SHA-256',
   new TextEncoder().encode(JSON.stringify(unsigned)));
 const sha256=Array.from(new Uint8Array(digest)).map(b=>
   b.toString(16).padStart(2,'0')).join('');
 vi.mocked(readTextFile).mockResolvedValue(JSON.stringify({...unsigned,sha256}));
}
beforeEach(()=>{
 localStorage.clear();
 vi.mocked(readTextFile).mockReset();
 vi.stubGlobal('crypto',webcrypto);
 setBillingScope('openai','');
 setBillingScope('anthropic','');
 useCostControlStore.setState({entries:[
  ...[0,1,2].map((n)=>({
   id:'c'+n,occurredAtMs:Date.parse('2026-09-10T12:00:00Z'),
   targetKey:'provider:openai:gpt',targetLabel:'gpt',
   sessionId:'s',runId:null,
   usage:{promptTokens:100,completionTokens:50,totalTokens:150},
   costUsd:0.33,
  })),
 ]});
});
afterEach(()=>{vi.useRealTimers();vi.unstubAllGlobals()});
describe('provider-admin billing is an independent cost guard, not an invoice',()=>{
 it('requires a scoped official provider cost report and matched local model ledger',async()=>{
  vi.useFakeTimers();vi.setSystemTime(NOW);
  setBillingScope('openai','key_1');
  expect(billingScope('openai')).toBe('key_1');
  setBillingGuardEnabled(true);
  expect(billingGuardEnabled()).toBe(true);
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(false);
  const v=payload();
  expect(validateProviderBillingEvidence(v,'openai','key_1',NOW).billedMicroUsd).toBe(1000000);
  // Legacy Web Storage values cannot authorize billing; they must be purged.
  localStorage.setItem('little-monkey-swarm-billing-scope-openai','old_scope');
  localStorage.setItem('little-monkey-swarm-billing-admin-receipt-openai',JSON.stringify(v));
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(false);
  setBillingScope('openai','key_1');
  expect(localStorage.getItem('little-monkey-swarm-billing-scope-openai')).toBeNull();
  expect(localStorage.getItem('little-monkey-swarm-billing-admin-receipt-openai')).toBeNull();
  await mockOfficialReceipt();
  await synchronizeOfficialBilling();
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(true);
  expect(localStorage.getItem('little-monkey-swarm-billing-scope-openai')).toBeNull();
  expect(localStorage.getItem('little-monkey-swarm-billing-admin-receipt-openai')).toBeNull();
  useCostControlStore.setState({entries:[]});
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(false);
  useCostControlStore.setState({entries:[
   ...[0,1,2].map(n=>({
    id:'c'+n,occurredAtMs:Date.parse('2026-09-10T12:00:00Z'),
    targetKey:'provider:openai:gpt',targetLabel:'gpt',sessionId:'s',runId:null,
    usage:{promptTokens:100,completionTokens:50,totalTokens:150},costUsd:0.33,
   })),
  ]});
  expect(billingGuardAllowsKeys(['provider:openrouter:model'],NOW)).toBe(false);
  expect(billingGuardAllowsKeys(['provider:anthropic:model'],NOW)).toBe(false);
  // Changing the scope always drops the in-memory receipt and fails closed.
  setBillingScope('openai','other_scope');
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(false);
 });
 it('rejects anomalous, provisional, stale or scope-mismatched provider claims',()=>{
  for(const change of [
   {final:false}, {month:'2026-08'}, {sha256:'fake'},
   {billedMicroUsd:-5}, {scope:{kind:'api_key_id',id:'another'}},
   {comparison:{...payload().comparison,state:'variance_detected'}},
   {comparison:{...payload().comparison,unknownCalls:1}},
   {comparison:{...payload().comparison,driftMicroUsd:999}},
   {fetchedAt:'2025-01-01T00:00:00Z'},
  ])expect(()=>validateProviderBillingEvidence({...payload(),...change},
   'openai','key_1',NOW)).toThrow();
 });
 it('operator can opt out; billing guard cannot affect normal non-cost routing',()=>{
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(true);
  setBillingGuardEnabled(true);
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(false);
  setBillingGuardEnabled(false);
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(true);
  expect(()=>setBillingScope('openai','wrong token\nAuthorization:')).toThrow();
 });
});
