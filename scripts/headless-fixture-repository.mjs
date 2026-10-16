/**
 * Autonomous tasks require a real Git HEAD to freeze/recheck the workspace.
 * Create a private, disposable, fully committed benchmark fixture without
 * reading user's Git identities or touching any existing repository.
 */
import {spawnSync} from 'node:child_process';
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
}
