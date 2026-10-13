#!/usr/bin/env node
/**
 * REAL vulnerability -> review -> human-approved fix -> independent tests.
 * Exercises the installed resident daemon's frozen graph, model runtime,
 * verified predecessor reports and manual tool approval process.
 *
 * Never silently installs services, changes any non-disposable repository,
 * authorizes tool calls, or makes network/deployment/Git pushes.
 */
import {spawnSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {randomUUID,createHash} from 'node:crypto';
const option=name=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1]??null;};
const target=option('--target'),reviewTarget=option('--review-target');
const bin=option('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey';
const maxSeconds=Number(option('--timeout-seconds')??'1800');
const approved=process.argv.includes('--confirm-manual-approvals');
const keep=process.argv.includes('--keep-repository');
if(!process.argv.includes('--self-check') && (!approved||!/^ollama:[A-Za-z0-9_.:@+-]{2,140}$/.test(target??'')||
   !/^ollama:[A-Za-z0-9_.:@+-]{2,140}$/.test(reviewTarget??'')||
   reviewTarget===target||
   !Number.isSafeInteger(maxSeconds)||maxSeconds<120||maxSeconds>7200)) {
  console.error('Usage: --target ollama:<installed-model> --review-target ollama:<different-installed-model> --confirm-manual-approvals [--timeout-seconds 120..7200] [--keep-repository] [--monkey-bin PATH]');
  process.exit(2);
}
function execute(command,args,cwd,timeout=30_000) {
  const r=spawnSync(command,args,{cwd,encoding:'utf8',timeout,
    maxBuffer:2*1024*1024,env:process.env});
  if(r.error||r.status!==0)throw Error('Command failed: '+command+' '+args.slice(0,2).join(' '));
  return r.stdout;
}
function cli(args,cwd){const out=execute(bin,args,cwd);
  try{return JSON.parse(out.trim());}catch{throw Error('Installed CLI did not return JSON');}
}
const source='export function isOwner(requester, resourceOwner) { return requester !== resourceOwner; }\n';
const patchedSource=source.replace('!==','===');
const tests=[
'import assert from "node:assert/strict";',
'import { isOwner } from "./src/module.mjs";',
'assert.equal(isOwner("alice","alice"),true);',
'assert.equal(isOwner("alice","bob"),false);',
'assert.equal(isOwner("bob","alice"),false);',
'console.log("all authorization regressions pass");',
'',
].join('\n');
function independentlyVerify(work,baselineCommit){
  // The verifier is outside the model graph. Never execute arbitrary model
  // code: only the exact safe expected fixture repair may be evaluated.
  if(execute('git',['rev-parse','HEAD'],work).trim()!==baselineCommit)
    throw Error('Git baseline was rewritten by an agent.');
  const statusNow=()=>execute('git',['status','--porcelain=v1','--untracked-files=all','--ignored'],work);
  if(statusNow()!==' M src/module.mjs\n')
    throw Error('Unexpected tracked, untracked, ignored or staged changes: '+JSON.stringify(statusNow()));
  const sourceAfter=readFileSync(join(work,'src','module.mjs'),'utf8');
  if(sourceAfter!==patchedSource)
    throw Error('Only the exact minimal authorization fix may pass the source oracle.');
  if(execute('git',['show','HEAD:src/module.mjs'],work)!==source ||
     readFileSync(join(work,'test.mjs'),'utf8')!==tests ||
     execute('git',['show','HEAD:test.mjs'],work)!==tests)
    throw Error('Frozen source and independent regression tests have been tampered with.');
  execute('git',['diff','--check'],work);
  const identities=Array.from({length:16},()=>randomUUID());
  const holdout=[
    "import assert from 'node:assert/strict';",
    "import {pathToFileURL} from 'node:url';",
    "const {isOwner}=await import(pathToFileURL(process.argv[1]).href);",
    "const names="+JSON.stringify(identities)+";",
    "for(const name of names){for(const owner of names){",
    "assert.equal(isOwner(name,owner),name===owner);}}",
    "console.log('independent holdout passed');",
  ].join('\n');
  if(!execute(process.execPath,['--input-type=module','--eval',holdout,
      join(work,'src','module.mjs')],work,10_000).includes('independent holdout passed'))
    throw Error('Independent holdout did not execute.');
  if(!execute(process.execPath,['test.mjs'],work,10_000).includes('authorization regressions pass'))
    throw Error('External regression tests did not run.');
  if(statusNow()!==' M src/module.mjs\n'||
     readFileSync(join(work,'src','module.mjs'),'utf8')!==patchedSource)
    throw Error('Worktree changed during independent tests.');
  const sha256=text=>createHash('sha256').update(text).digest('hex');
  const evidence={baselineCommit,
    vulnerableSha256:sha256(source),repairedSha256:sha256(sourceAfter),
    frozenRegressionSha256:sha256(tests),externalRegression:'passed',
    independentHoldoutCases:identities.length**2};
  return evidence;
}
function oracleContract(){
  const good=patchedSource;
  if(!source.includes('!==')||!good.includes('==='))throw Error('Broken test fixture.');
  const tmp=mkdtempSync(join(tmpdir(),'lm-repair-oracle-'));
  try {
    mkdirSync(join(tmp,'src'),{recursive:true});
    writeFileSync(join(tmp,'package.json'),JSON.stringify({type:'module',private:true}));
    writeFileSync(join(tmp,'test.mjs'),tests);
    writeFileSync(join(tmp,'src','module.mjs'),source);
    const bad=spawnSync(process.execPath,['test.mjs'],{cwd:tmp,timeout:5000,encoding:'utf8'});
    if(bad.status===0)throw Error('Initial vulnerability was not detected by regression test.');
    writeFileSync(join(tmp,'src','module.mjs'),good);
    const repaired=spawnSync(process.execPath,['test.mjs'],{cwd:tmp,timeout:5000,encoding:'utf8'});
    if(repaired.status!==0)throw Error('Regression oracle rejects correct fix.');
    const gitRepo=join(tmp,'repo'),gitSource=join(gitRepo,'src','module.mjs');
    mkdirSync(join(gitRepo,'src'),{recursive:true});
    writeFileSync(join(gitRepo,'package.json'),JSON.stringify({type:'module',private:true}));
    writeFileSync(join(gitRepo,'test.mjs'),tests);
    writeFileSync(gitSource,source);
    writeFileSync(join(gitRepo,'MONKEY.md'),'Disposable fixture');
    writeFileSync(join(gitRepo,'.gitignore'),'*.scratch\\n');
    execute('git',['init','-q'],gitRepo);
    execute('git',['add','.'],gitRepo);
    execute('git',['-c','user.name=Oracle Contract',
      '-c','user.email=oracle@example.invalid','-c','commit.gpgsign=false',
      'commit','-qm','frozen independent fixture'],gitRepo);
    const baseline=execute('git',['rev-parse','HEAD'],gitRepo).trim();
    const mustReject=reason=>{
      let rejected=false;
      try{independentlyVerify(gitRepo,baseline);}catch{rejected=true;}
      if(!rejected)throw Error('External verifier accepted '+reason);
    };
    mustReject('unrepaired vulnerable source');
    writeFileSync(gitSource,patchedSource+'// injected model code\\n');
    mustReject('noncanonical source injection');
    writeFileSync(gitSource,patchedSource);
    if(independentlyVerify(gitRepo,baseline).independentHoldoutCases!==256)
      throw Error('Exact safe repair failed external holdouts.');
    writeFileSync(join(gitRepo,'test.mjs'),tests+'// tampered\\n');
    mustReject('tampered regression tests');
    writeFileSync(join(gitRepo,'test.mjs'),tests);
    writeFileSync(join(gitRepo,'rogue.scratch'),'ignored malware');
    mustReject('ignored worktree side effects');
  }finally{rmSync(tmp,{recursive:true,force:true});}
}
if(process.argv.includes('--self-check')) {
  oracleContract();
  console.log(JSON.stringify({status:'source-oracle-passed',warning:'No model, daemon or service was tested.'}));
}else{
  oracleContract();
  const status=cli(['daemon','status','--json']);
  if(!status.installed||!status.service_running||!status.heartbeat_fresh) {
    console.error(JSON.stringify({status:'not_verified',error:'Dedicated installed daemon with fresh heartbeat required; no service was changed.'}));
    process.exit(1);
  }
  const root=mkdtempSync(join(tmpdir(),'lm-live-repair-'));
  const work=join(root,'repo');
  const graphId='swarm-repair-'+randomUUID().replaceAll('-','').slice(0,20);
  let submitted=false,verified=false;
  try{
    mkdirSync(join(work,'src'),{recursive:true});
    writeFileSync(join(work,'package.json'),JSON.stringify({type:'module',private:true}));
    writeFileSync(join(work,'src','module.mjs'),source);
    writeFileSync(join(work,'test.mjs'),tests);
    writeFileSync(join(work,'MONKEY.md'),
      'Disposable offline source fixture. Never modify tests, MONKEY.md or Git configuration. No network, deployment or Git push.\n');
    execute('git',['init','-q'],work);
    execute('git',['add','.'],work);
    execute('git',['-c','user.name=Swarm Acceptance',
      '-c','user.email=acceptance@example.invalid','-c','commit.gpgsign=false',
      'commit','-qm','source fixture'],work);
    const baselineCommit=execute('git',['rev-parse','HEAD'],work).trim();
    const broken=spawnSync(process.execPath,['test.mjs'],{cwd:work,timeout:5000});
    if(broken.status===0)throw Error('The seeded vulnerable project already passes.');
    const phases=[
      {id:'audit',deps:[],permission:'plan',
        prompt:'Read-only: inspect src/module.mjs and test.mjs, cite the actual authorization bug and the minimal fix. Do not modify any files.'},
      {id:'repair',deps:['audit'],permission:'manual',
        prompt:'Fix the authorization comparison in src/module.mjs using only the minimal one-operator change, with normal per-tool human approvals. Do not reformat, add code or edit tests, MONKEY.md, dependencies or Git metadata. No bypasses.'},
      {id:'verify',deps:['repair'],permission:'plan',
        prompt:'Independently inspect the actual modified source against tests. Do not rely on predecessor reports, edit files, or invent test evidence. Report any remaining weakness and exact changes.'},
    ];
    const nodes=phases.map(item=>{
      const path=join(root,item.id+'.json');
      const recipe={version:1,name:graphId+'-'+item.id,
        target:{ollama:(item.id==='verify'?reviewTarget:target).slice('ollama:'.length)},workspace:work,
        permission_mode:item.permission,
        system:item.permission==='plan'
          ? 'Perform a source-only read-only review. Never modify files or ask to elevate tool permissions.'
          : 'Only operator-confirmed tools may modify the disposable source fixture. Never auto-approve, deploy or push.',
        prompt:item.prompt,params:{},max_iterations:24,
        timeout_seconds:Math.min(1800,maxSeconds),output:{json:true}};
      writeFileSync(path,JSON.stringify(recipe,null,2));
      return {id:item.id,recipe:path,depends_on:item.deps,
        include_dependency_reports:item.id==='repair',
        max_runtime_seconds:Math.min(1800,maxSeconds)};
    });
    const graphPath=join(root,'graph.json');
    writeFileSync(graphPath,JSON.stringify({version:1,id:graphId,max_parallel:1,nodes},null,2));
    const queued=cli(['daemon','swarm-submit',graphPath]);
    if(queued.id!==graphId||queued.nodes!==3)throw Error('Daemon did not durably register the three-stage graph.');
    submitted=true;
    console.error('Registered '+graphId+'. Approve or deny code changes through the installed daemon/desktop UI. No approval is automatic.');
    const deadline=Date.now()+maxSeconds*1000;
    let terminal=null;
    while(Date.now()<deadline){
      const graph=cli(['daemon','swarm-status','--id',graphId]).graphs?.find(g=>g.id===graphId);
      if(!graph||!Array.isArray(graph.tasks)||graph.tasks.length!==3)throw Error('Durable workflow graph disappeared.');
      if(graph.state==='succeeded') {terminal=graph;break;}
      if(['failed','cancelled'].includes(graph.state))throw Error('Resident workflow stopped in '+graph.state);
      // Busy-sleep is bounded and does not make any model or permission calls.
      const pause=spawnSync(process.execPath,['-e','setTimeout(()=>{},3000)'],{timeout:4500});
      if(pause.error)throw Error('Poll timer failed.');
    }
    if(!terminal)throw Error('Timed out without a fully succeeded resident graph.');
    const jobIds=terminal.tasks.map(t=>t.job_id);
    if(terminal.tasks.some(t=>t.state!=='succeeded')||
       new Set(jobIds).size!==3||jobIds.some(id=>typeof id!=='string'||!id)) {
      throw Error('A model report was accepted without three distinct completed daemon jobs.');
    }
    const evidence=independentlyVerify(work,baselineCommit);
    verified=true;
    console.log(JSON.stringify({mode:'real-resident-repository-repair',status:'verified',
      graphId,repairModel:target,independentReviewModel:reviewTarget,
      distinctResidentJobs:jobIds.length,sourceChanged:['src/module.mjs'],
      ...evidence,testStatus:'pass',
      caveat:'Actual approvals and model run ledger must be inspected separately; this is only a disposable fixture.'},null,2));
  }catch(error){
    console.error(JSON.stringify({mode:'real-resident-repository-repair',status:'not_verified',
      graphId,submitted,error:String(error),
      note:'Do not automatically re-submit: inspect the same graph, approvals and Git diff.'},null,2));
    process.exitCode=1;
  }finally{
    if(verified&&!keep)rmSync(root,{recursive:true,force:true});
    else console.error('Acceptance workspace retained for operator review: '+root);
  }
}
