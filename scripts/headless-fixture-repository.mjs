/**
 * Autonomous tasks require a real Git HEAD to freeze/recheck the workspace.
 * Create a private, disposable, fully committed benchmark fixture without
 * reading user's Git identities or touching any existing repository.
 */
import {spawnSync} from 'node:child_process';
function gitQuery(root,args){
 const out=spawnSync('git',['-C',root,...args],{
  encoding:'utf8',timeout:15000,maxBuffer:1024*1024,
  env:{...process.env,GIT_TERMINAL_PROMPT:'0'},
 });
 if(out.error||out.status!==0)throw Error(
  'Disposable Git fixture integrity probe failed: '+String(out.error?.message??out.stderr).slice(0,250));
 return out.stdout.trim();
}

export function assertHeadlessFixtureUnchanged(root,expectedHead){
 if(!/^[a-f0-9]{40,64}$/.test(expectedHead))
  throw Error('Invalid fixture HEAD identity');
 if(gitQuery(root,['rev-parse','HEAD'])!==expectedHead)
  throw Error('Native agent changed the immutable benchmark fixture HEAD');
 if(gitQuery(root,['status','--porcelain','--untracked-files=all'])!=='')
  throw Error('Native agent changed or added files in the read-only benchmark fixture');
}

export function initHeadlessFixtureRepository(root){
 if(typeof root!=='string'||!root)throw Error('Missing disposable fixture root');
 const git=(args)=>{
  const out=spawnSync('git',['-C',root,...args],{
   encoding:'utf8',timeout:15000,maxBuffer:1024*1024,
   env:{...process.env,GIT_TERMINAL_PROMPT:'0'},
  });
  if(out.error||out.status!==0)throw Error(
   'Disposable Git fixture setup failed: '+String(out.error?.message??out.stderr).slice(0,250));
 };
 git(['init','--quiet']);
 git(['add','--','src/fixture.txt']);
 git(['-c','user.name=Little Monkey Benchmark',
      '-c','user.email=benchmark@localhost','commit','--quiet','-m','Read-only disposable test fixture']);
 const head=gitQuery(root,['rev-parse','HEAD']);
 assertHeadlessFixtureUnchanged(root,head);
 return head;
}
