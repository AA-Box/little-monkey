// @vitest-environment jsdom
import {beforeEach,describe,expect,it,vi} from 'vitest';
vi.mock('@tauri-apps/api/core',()=>({isTauri:()=>false}));
vi.mock('@tauri-apps/api/path',()=>({appDataDir:async()=>'/tmp/app',join:async(...s:string[])=>s.join('/')}));
vi.mock('@tauri-apps/plugin-fs',()=>({readTextFile:vi.fn(),writeTextFile:vi.fn()}));
import {
 billingGuardAllowsKeys,billingGuardEnabled,billingScope,
 setBillingGuardEnabled,setBillingScope,validateProviderBillingEvidence,
} from './swarmBillingEvidence';
const NOW=Date.parse('2026-10-14T10:00:00Z');
const payload=()=>({
 version:1,source:'official_provider_admin_cost_api',provider:'openai',month:'2026-09',
 scope:{kind:'api_key_id',id:'key_1'},currency:'USD',final:true,
 billedMicroUsd:1000000,days:30,sha256:'b'.repeat(64),
 fetchedAt:new Date(NOW-60000).toISOString(),
 comparison:{state:'within_tolerance',exactScopeCompared:true,
  estimatedMicroUsd:990000,driftMicroUsd:10000,knownCalls:3,unknownCalls:0},
});
beforeEach(()=>localStorage.clear());
describe('provider-admin billing is an independent cost guard, not an invoice',()=>{
 it('requires a scoped official provider cost report and matched local model ledger',()=>{
  setBillingScope('openai','key_1');
  expect(billingScope('openai')).toBe('key_1');
  setBillingGuardEnabled(true);
  expect(billingGuardEnabled()).toBe(true);
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(false);
  const v=payload();
  expect(validateProviderBillingEvidence(v,'openai','key_1',NOW).billedMicroUsd).toBe(1000000);
  localStorage.setItem('little-monkey-swarm-billing-admin-receipt-openai',JSON.stringify(v));
  expect(billingGuardAllowsKeys(['provider:openai:gpt'],NOW)).toBe(true);
  expect(billingGuardAllowsKeys(['provider:openrouter:model'],NOW)).toBe(false);
  expect(billingGuardAllowsKeys(['provider:anthropic:model'],NOW)).toBe(false);
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
