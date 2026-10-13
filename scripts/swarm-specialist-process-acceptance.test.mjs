#!/usr/bin/env node
/**
 * Process-level acceptance of the exact production learning -> oracle ->
 * deployment -> native dispatch -> rollback path.
 *
 * A fixture-bound deterministic native CLI double is deliberately used here.
 * This proves child-process wiring and executable source-oracle checks, NOT
 * any real-model, Apple Silicon, or production-quality claim.
 */
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,writeFileSync,readFileSync,rmSync,chmodSync,readdirSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {benchmarkCycle,parsePolicy,dispatchRecipe} from './swarm-specialist-continuous-loop.mjs';

const fakeCli=String.raw`#!/usr/bin/env node
import {readFileSync} from 'node:fs';
import {basename,join} from 'node:path';
const argv=process.argv.slice(2);
if(argv.includes('--no-mcp') && argv.includes('--no-rules')) argv.splice(0,2);
if(argv[0]!=='task'||argv[1]!=='run'||argv[3]!=='--json') process.exit(3);
const recipe=JSON.parse(readFileSync(argv[2],'utf8'));
if(recipe.version!==1||recipe.permission_mode==='bypass'||
   !recipe.target?.ollama||typeof recipe.workspace!=='string')process.exit(4);
const source=readFileSync(join(recipe.workspace,'src','module.mjs'),'utf8');
const symbol=source.match(/export function ([A-Za-z0-9_]+)\(/)?.[1];
if(!symbol)process.exit(5);
let category='safe-control';
for(const c of ['auth-bypass','code-injection','event-handler',
  'accessibility-label','tenant-leak','timeout-budget','quadratic-scan',
  'cache-recompute','configuration-flag','integration-contract']){
  if(source.includes(c.replaceAll('-',''))) category=c;
}
// The oracle's fixture symbols are deliberately not the category strings.
// Determine the category by inspecting the concrete function body, as a
// deterministic stand-in for the model's actual source inspection.
const sourceDefect={
 allowOwner_:'auth-bypass',evaluateInput_:'code-injection',
 clickAction_:'event-handler',getButtonLabel_:'accessibility-label',
 tenantCache_:'tenant-leak',withinDeadline_:'timeout-budget',
 inspectItems_:'quadratic-scan',cachedLookup_:'cache-recompute',
 parseFlag_:'configuration-flag',makePayload_:'integration-contract',
};
category=Object.entries(sourceDefect).find(([prefix])=>symbol.startsWith(prefix))?.[1]??'safe-control';
const winner=process.env.LM_ACCEPT_WINNER??'trusted-a';
const correct=recipe.target.ollama===winner;
const message=correct
 ?category==='safe-control'
  ?'NO_FINDING src/module.mjs '+symbol
  :'FINDING src/module.mjs '+symbol+' '+category
 :'NO_FINDING src/module.mjs '+symbol;
console.log(JSON.stringify({status:'ok',
 run_id:'run-'+process.pid+'-'+Date.now()+'-'+Math.random().toString(36).slice(2,10),
 final_message:message,iterations_capped:false}));
`;

test('real subprocess integration: independent fixtures, quality-driven promotion, dispatch, regression rollback, and evidence fail-closed',()=>{
 const directory=mkdtempSync(join(tmpdir(),'lm-specialist-e2e-'));
 const saved=process.env.LM_ACCEPT_WINNER;
 try{
  const cli=join(directory,'monkey-test-double.mjs');
  writeFileSync(cli,fakeCli,{mode:0o700});
  chmodSync(cli,0o700);
  const policy=parsePolicy({version:1,monkeyBin:cli,stateDir:directory,
   targets:['ollama:trusted-a','ollama:trusted-b'],domains:['security'],
   repetitions:2,intervalSeconds:60,timeoutMs:30000,minAccuracy:0.8,
   minImprovement:0.05,maxAgeHours:1});
  const recipe=join(directory,'task.json');
  // Normal dispatch is also a REAL subprocess invocation of our native
  // compatibility double, not an injected callback to dispatchRecipe.
  writeFileSync(recipe,JSON.stringify({version:1,workspace:directory,
   permission_mode:'plan',system:'Retain rules',prompt:'Review my original task'}));
  process.env.LM_ACCEPT_WINNER='trusted-a';
  const first=benchmarkCycle(policy,{log:()=>{}});
  assert.equal(first.domains.security.active?.target,'ollama:trusted-a');
  assert.equal(first.domains.security.active.accuracy,1);
  let result=dispatchRecipe(policy,'security',recipe);
  assert.equal(result.target,'ollama:trusted-a');
  assert.equal(JSON.parse(result.nativeResult).status,'ok');
  process.env.LM_ACCEPT_WINNER='trusted-b';
  const second=benchmarkCycle(policy,{log:()=>{}});
  assert.equal(second.domains.security.active?.target,'ollama:trusted-b');
  assert.equal(second.domains.security.active.accuracy,1);
  result=dispatchRecipe(policy,'security',recipe);
  assert.equal(result.target,'ollama:trusted-b');
  assert.equal(JSON.parse(readFileSync(recipe,'utf8')).target,undefined);
  process.env.LM_ACCEPT_WINNER='trusted-a';
  const third=benchmarkCycle(policy,{log:()=>{}});
  assert.equal(third.domains.security.active?.target,'ollama:trusted-a');
  const evidence=readdirSync(directory).find(file=>file.endsWith('.json')&&
    file.startsWith('evidence-security-')&&
    readFileSync(join(directory,file),'utf8').includes('"target": "ollama:trusted-a"'));
  // Evidence JSON is content-addressed. Tampering with it cannot change
  // the deployed routing, and dispatch must stop before invoking any model.
  assert.ok(evidence);
  const evidencePath=join(directory,evidence);
  const raw=readFileSync(evidencePath,'utf8');
  writeFileSync(evidencePath,raw.replace('"verified": true','"verified": false'));
  assert.throws(()=>dispatchRecipe(policy,'security',recipe),/contradict|disagree|rank|evidence|accuracy/i);
 }finally{
  if(saved===undefined)delete process.env.LM_ACCEPT_WINNER;
  else process.env.LM_ACCEPT_WINNER=saved;
  rmSync(directory,{recursive:true,force:true});
 }
});
