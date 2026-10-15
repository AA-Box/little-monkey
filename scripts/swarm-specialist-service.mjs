#!/usr/bin/env node
/**
 * Explicit, current-user macOS LaunchAgent installer for continuous,
 * independent specialist benchmark -> CLI routing deployment.
 *
 * No sudo, no shell interpretation, no unrequested auto-install and no
 * inherited environment secrets in the plist. launchd supervises the loop.
 */
import {spawnSync} from 'node:child_process';
import {existsSync,lstatSync,mkdirSync,openSync,writeFileSync,closeSync,
  unlinkSync,realpathSync,readFileSync} from 'node:fs';
import {homedir} from 'node:os';
import {resolve,dirname,join,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {parsePolicy} from './swarm-specialist-continuous-loop.mjs';

export const SERVICE_LABEL='com.littlemonkey.verified-specialist-learning';
const CONTROL='<!-- little-monkey-verified-specialist-learning-v1 -->';
const cleanString=value=>{
 if(typeof value!=='string'||!value||/[<>&"'\x00-\x1f]/.test(value))
  throw Error('Unsafe service path or plist value');
 return value;
};
const xml=value=>String(value).replaceAll('&','&amp;').replaceAll('<','&lt;')
 .replaceAll('>','&gt;').replaceAll('"','&quot;').replaceAll("'",'&apos;');
const string=value=>'<string>'+xml(value)+'</string>';
export function servicePlist({nodePath,scriptPath,policyPath,stateDir}){
 for(const path of [nodePath,scriptPath,policyPath,stateDir]){
  if(typeof path!=='string'||!isAbsolute(path)||/[\x00-\x1f]/.test(path))
   throw Error('LaunchAgent requires reviewed absolute local paths');
 }
 const args=[nodePath,scriptPath,'--policy',policyPath,'--confirm-unattended-local-evaluation'];
 return '<?xml version="1.0" encoding="UTF-8"?>\n'+
  '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" '+
  '"http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'+
  '<plist version="1.0"><dict>\n'+CONTROL+'\n'+
  '<key>Label</key>'+string(SERVICE_LABEL)+'\n'+
  '<key>ProgramArguments</key><array>'+args.map(string).join('')+'</array>\n'+
  '<key>WorkingDirectory</key>'+string(dirname(scriptPath))+'\n'+
  '<key>RunAtLoad</key><true/>\n'+
  '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n'+
  '<key>ThrottleInterval</key><integer>120</integer>\n'+
  '<key>StandardOutPath</key>'+string(join(stateDir,'service.out.log'))+'\n'+
  '<key>StandardErrorPath</key>'+string(join(stateDir,'service.err.log'))+'\n'+
  '</dict></plist>\n';
}
function existingRegular(path){
 if(!existsSync(path))return false;
 const st=lstatSync(path);
 if(!st.isFile()||st.isSymbolicLink())
  throw Error('Existing service path is not a regular file');
 return true;
}
export function manageService(action,policyPath,{
 platform=process.platform,home=homedir(),nodePath=process.execPath,
 scriptPath=resolve(dirname(fileURLToPath(import.meta.url)),
  'swarm-specialist-continuous-loop.mjs'),
 uid=process.getuid?.(),run=spawnSync,
}={}){
 if(platform!=='darwin'||!Number.isSafeInteger(uid)||uid<1)
  throw Error('The user LaunchAgent installer requires macOS in a non-root login session');
 if(!['install','uninstall','status'].includes(action))
  throw Error('Use install, uninstall or status');
 if(!isAbsolute(home)||realpathSync(home)!==home)
  throw Error('Use a canonical current-user home directory');
 const agents=join(home,'Library','LaunchAgents');
 const path=join(agents,SERVICE_LABEL+'.plist');
 const service='gui/'+uid+'/'+SERVICE_LABEL,domain='gui/'+uid;
 if(action==='status'){
  const result=run('/bin/launchctl',['print',service],{encoding:'utf8',timeout:15000});
  return {installed:existingRegular(path),running:!result.error&&result.status===0};
 }
 if(action==='uninstall'){
  if(!existingRegular(path))return {installed:false,running:false};
  const text=readFileSync(path,'utf8');
  if(!text.includes(CONTROL))throw Error('Refusing to uninstall an unrelated LaunchAgent');
  // bootout can fail if service is already stopped. The verified owned
  // plist can still be removed, but a live service must not be orphaned.
  const stopped=run('/bin/launchctl',['bootout',domain,path],{encoding:'utf8',timeout:30000});
  const queried=run('/bin/launchctl',['print',service],{encoding:'utf8',timeout:15000});
  if((stopped.error||stopped.status!==0)&&queried.status===0)
   throw Error('Service is still running; refusing to orphan its LaunchAgent');
  unlinkSync(path);
  return {installed:false,running:false};
 }
 if(!isAbsolute(policyPath)||!existingRegular(policyPath))
  throw Error('Install needs an absolute, operator-reviewed JSON policy');
 const policy=parsePolicy(readFileSync(policyPath,'utf8'));
 if(realpathSync(nodePath)!==nodePath||realpathSync(scriptPath)!==scriptPath)
  throw Error('Node and worker must have canonical reviewed executable paths');
 if(existingRegular(path))throw Error('LaunchAgent already exists; uninstall explicitly before replacing it');
 mkdirSync(agents,{recursive:true,mode:0o700});
 const payload=servicePlist({nodePath,scriptPath,policyPath:realpathSync(policyPath),
  stateDir:policy.stateDir});
 const fd=openSync(path,'wx',0o600);
 try{writeFileSync(fd,payload);}finally{closeSync(fd);}
 const result=run('/bin/launchctl',['bootstrap',domain,path],{encoding:'utf8',timeout:30000});
 if(result.error||result.status!==0){
  unlinkSync(path);
  throw Error('launchd refused the specialist service: '+String(result.stderr??result.error??'').slice(0,250));
 }
 return {installed:true,running:true,plist:path,domain:service};
}
const arg=name=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1]??null;};
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
 try{
  const action=process.argv[2];
  if(!['install','uninstall','status'].includes(action)||
     (action==='install'&&!process.argv.includes('--confirm-install-service')))
   throw Error('Usage: node scripts/swarm-specialist-service.mjs install --policy /absolute/path.json --confirm-install-service | status | uninstall');
  const result=manageService(action,arg('--policy'));
  console.log(JSON.stringify(result));
 }catch(error){
  console.error(JSON.stringify({status:'service_action_failed',error:String(error)}));
  process.exitCode=1;
 }
}
