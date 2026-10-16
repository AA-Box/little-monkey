#!/usr/bin/env node
/**
 * Explicitly approved, transactional activation of SELF-BUILT CLI binaries.
 * Never touches a system/desktop install, the user's existing CLI, or a
 * remotely installed daemon. A caller opts in to PATH pointing to
 * ~/.littlemonkey/selfdev/current/monkey-cli.
 *
 * Build from a CLEAN Git tree; run real Rust tests, compile a native release
 * binary, smoke it, store a SHA-256-pinned immutable candidate; activation
 * atomically changes a managed symlink and rolls back on health failure.
 */
import {
  existsSync,readFileSync,writeFileSync,mkdirSync,statSync,lstatSync,
  copyFileSync,chmodSync,renameSync,rmSync,symlinkSync,readlinkSync,
  realpathSync,
} from 'node:fs';
import {homedir} from 'node:os';
import {resolve,join,basename,dirname} from 'node:path';
import {createHash,randomUUID} from 'node:crypto';
import {spawnSync} from 'node:child_process';

const usage='Usage: node scripts/selfdev-transaction.mjs build|stage|activate|rollback|status [--candidate PATH] [--confirm-sha SHA256] [--confirm-rollback] [--root PATH]';
const args=process.argv.slice(2),command=args[0];
const opt=(name)=>{const ix=args.indexOf(name);return ix<0?null:args[ix+1]??null;};
const DEFAULT_ROOT=join(homedir(),'.littlemonkey','selfdev');
const root=resolve(opt('--root')??DEFAULT_ROOT),entry=join(root,'current');
const fallback=join(root,'previous'),versions=join(root,'versions');
const BIN='monkey-cli';
const mode='selfdev-managed-v1';
function fail(message){throw Error(message);}
if(!['build','stage','activate','rollback','status'].includes(command)){console.error(usage);process.exit(2);}
if(process.platform==='win32')fail('Atomic symlink activation currently requires macOS/Linux');
function digest(path){return createHash('sha256').update(readFileSync(path)).digest('hex');}
function exec(file,params,{cwd,timeout=120000}={}){
 const r=spawnSync(file,params,{cwd,encoding:'utf8',timeout,maxBuffer:2*1024*1024,
  env:{...process.env,CI:'true',GIT_TERMINAL_PROMPT:'0'}});
 if(r.error||r.status!==0)fail(file+' verification failed (exit='+r.status+'): '+
  String(r.error?.message??r.stderr??r.stdout).slice(0,380));
 return r.stdout.trim();
}
function safePath(path){
 try {
  if(lstatSync(path).isSymbolicLink())
   fail('Refusing symlink-controlled installation root: '+path);
 }catch(error){
  if(error?.code!=='ENOENT')throw error;
 }
}
function ensureRoot(){
 for(const d of [root,versions])safePath(d);
 const marker=join(root,'.selfdev-managed');
 // Verify an existing marker by directly reading it; missing-file errors are
 // the ONLY case where we initialize, so there is no existence-check/write
 // race. A competing initializer wins by creating with O_EXCL ('wx').
 try{
  const value=readFileSync(marker,'utf8');
  if(value!==mode+'\n')fail('Root is not an approved selfdev directory');
  return;
 }catch(error){
  if(error?.code!=='ENOENT')throw error;
 }
 // A preexisting current path without the marker belongs to another tool.
 if(lstatMaybe(entry))fail('Refusing to adopt existing installation');
 mkdirSync(versions,{recursive:true,mode:0o700});
 try{
  writeFileSync(marker,mode+'\n',{flag:'wx',mode:0o600});
 }catch(error){
  if(error?.code!=='EEXIST')throw error;
  if(readFileSync(marker,'utf8')!==mode+'\n')
   fail('Concurrent selfdev initializer wrote an unrecognized marker');
 }
}
function linkTarget(link){
 // readlink is the actual authority, including for dangling symlinks.
 // Avoid a lstat/existence test followed by a separate vulnerable readlink.
 let target;
 try {target=readlinkSync(link);}
 catch(error){
  if(error?.code==='ENOENT')return null;
  fail('Managed activation path is not a valid symlink: '+error.code);
 }
 const dest=resolve(dirname(link),target);
 if(!dest.startsWith(versions+'/'))fail('Activation link points outside managed versions');
 return dest;
}
function lstatMaybe(path){
 try{return lstatSync(path);}
 catch(error){if(error?.code==='ENOENT')return null;throw error;}
}
function verify(binary){
 const r=spawnSync(binary,['--version'],{encoding:'utf8',timeout:15000,maxBuffer:128*1024});
 if(r.error||r.status!==0)fail('Candidate did not pass native CLI --version smoke: '+String(r.error?.message??r.stderr).slice(0,200));
 const output=(r.stdout+r.stderr).trim();
 if(!/monkey/i.test(output))fail('Candidate does not identify as Little Monkey CLI');
 return output.slice(0,150);
}
function recordFor(sha){
 if(!/^[a-f0-9]{64}$/.test(sha))fail('Expected complete SHA-256');
 const dest=join(versions,sha),binary=join(dest,BIN),meta=join(dest,'manifest.json');
 if(!existsSync(dest)||!existsSync(binary)||!existsSync(meta))
  fail('Candidate SHA is not present in managed versions');
 if(lstatSync(dest).isSymbolicLink()||lstatSync(binary).isSymbolicLink()||
    digest(binary)!==sha)fail('Staged native artifact failed digest check');
 const m=JSON.parse(readFileSync(meta,'utf8'));
 if(m.sha256!==sha || m.kind!==mode)fail('Staged provenance mismatch');
 return{dest,binary,manifest:m};
}
function stage(candidate,sourceCommit=null){
 ensureRoot();
 const source=realpathSync(candidate);
 if(!statSync(source).isFile() || statSync(source).size<100)
  fail('Expected a real native CLI executable, not an empty text file');
 const actual=digest(source),dest=join(versions,actual);
 if(existsSync(dest)){
  recordFor(actual);
  return actual;
 }
 const temp=join(versions,'.stage-'+randomUUID());
 mkdirSync(temp,{mode:0o700});
 try{
  const bin=join(temp,BIN);
  copyFileSync(source,bin);
  chmodSync(bin,0o700);
  if(digest(bin)!==actual)fail('Candidate changed during copy');
  const version=verify(bin);
  writeFileSync(join(temp,'manifest.json'),JSON.stringify({
   kind:mode,sha256:actual,sourceCommit,cliVersion:version,
   stagedAt:new Date().toISOString(),
  },null,2)+'\n',{flag:'wx',mode:0o600});
  renameSync(temp,dest);
  recordFor(actual);
  return actual;
 }catch(e){rmSync(temp,{recursive:true,force:true});throw e;}
}
function switchTo(link,target){
 const temp=join(root,'.swap-'+randomUUID());
 symlinkSync(target,temp,'dir');
 try{renameSync(temp,link);}catch(e){rmSync(temp,{force:true});throw e;}
}
function activate(sha){
 ensureRoot();
 const {dest,binary}=recordFor(sha);
 if(opt('--confirm-sha')!==sha)fail('Explicit --confirm-sha must match the staged artifact digest');
 verify(binary);
 const previous=linkTarget(entry);
 if(previous===dest)return{active:sha,unchanged:true};
 if(previous) { // Save the previous valid activation before any swap.
  const prevRecord=recordFor(basename(previous));
  verify(prevRecord.binary);
  switchTo(fallback,previous);
 }
 switchTo(entry,dest);
 try {verify(join(entry,BIN));}
 catch(e){
  if(previous) switchTo(entry,previous);
  else rmSync(entry,{force:true});
  throw Error('Post-activation smoke failed, original restored: '+e.message);
 }
 return{active:sha,previous:previous?basename(previous):null,rolledBack:false};
}
function rollback(){
 ensureRoot();
 if(!args.includes('--confirm-rollback'))fail('Explicit --confirm-rollback required');
 const previous=linkTarget(fallback);
 if(!previous)fail('No previous version to restore');
 const old=recordFor(basename(previous));
 verify(old.binary);
 const active=linkTarget(entry);
 switchTo(entry,previous);
 try{verify(join(entry,BIN));}
 catch(e){if(active)switchTo(entry,active);throw e;}
 if(active)switchTo(fallback,active);
 else rmSync(fallback,{force:true});
 return{active:old.manifest.sha256,restored:true};
}
function build(){
 const repo=resolve(opt('--repo')??process.cwd());
 const head=exec('git',['rev-parse','HEAD'],{cwd:repo});
 if(exec('git',['status','--porcelain'],{cwd:repo})!=='')
  fail('Refusing to build from a dirty Git source tree');
 exec('pnpm',['typecheck'],{cwd:repo,timeout:240000});
 exec('cargo',['test','--manifest-path','src-tauri/Cargo.toml','--lib'],{
  cwd:repo,timeout:20*60*1000,
 });
 exec('cargo',['build','--release','--manifest-path','src-tauri/Cargo.toml',
  '--bin','monkey-cli'],{cwd:repo,timeout:20*60*1000});
 const candidate=join(repo,'src-tauri','target','release',BIN);
 if(exec('git',['rev-parse','HEAD'],{cwd:repo})!==head||
   exec('git',['status','--porcelain'],{cwd:repo})!=='')
  fail('Source tree changed during build; refuse to stage');
 return{sha:stage(candidate,head),sourceCommit:head};
}
try{
 let result;
 if(command==='status'){
  const current=linkTarget(entry),old=linkTarget(fallback);
  result={active:current?basename(current):null,previous:old?basename(old):null,
   managedRoot:root,automaticSystemInstall:false};
 }else if(command==='stage'){
  if(!opt('--candidate'))fail('--candidate is required');
  result={sha:stage(opt('--candidate'))};
 }else if(command==='build')result=build();
 else if(command==='activate')result=activate(opt('--confirm-sha')??'');
 else result=rollback();
 console.log(JSON.stringify({ok:true,command,...result}));
}catch(e){console.error('SELFDEV_ERROR: '+String(e.message??e));process.exitCode=1;}
