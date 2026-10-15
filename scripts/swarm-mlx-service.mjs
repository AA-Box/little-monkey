#!/usr/bin/env node
/** Explicit current-user launchd supervision for the Apple Silicon MLX loop.
 * No root, shell, account secrets, implicit install, or network exposure.
 */
import {spawnSync} from 'node:child_process';
import {existsSync,lstatSync,realpathSync,readFileSync,mkdirSync,openSync,
 writeFileSync,closeSync,unlinkSync} from 'node:fs';
import {homedir} from 'node:os';
import {dirname,join,resolve,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {parseServingPolicy} from './swarm-mlx-verified-serving.mjs';
export const SERVICE_LABEL='com.littlemonkey.verified-mlx-serving';
const OWNER_MARK='<!-- little-monkey-current-user-verified-mlx-serving-v1 -->';
const escaped=value=>String(value).replaceAll('&','&amp;').replaceAll('<','&lt;')
 .replaceAll('>','&gt;').replaceAll('"','&quot;').replaceAll("'",'&apos;');
const xmlString=value=>'<string>'+escaped(value)+'</string>';
const regular=path=>{
 if(!existsSync(path))return false;
 const stat=lstatSync(path);
 if(!stat.isFile()||stat.isSymbolicLink())throw Error('Refusing unexpected LaunchAgent or policy path');
 return true;
};
export function mlxLaunchAgent({nodePath,scriptPath,policyPath,stateDir}){
 for(const path of [nodePath,scriptPath,policyPath,stateDir]){
  if(typeof path!=='string'||!isAbsolute(path)||/[\x00-\x1f]/.test(path))
   throw Error('LaunchAgent needs safe reviewed absolute paths');
 }
 const args=[nodePath,scriptPath,'--policy',policyPath,'--confirm-unattended-mlx-deployment'];
 return '<?xml version="1.0" encoding="UTF-8"?>\n'+
  '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" '+
  '"http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'+
  '<plist version="1.0"><dict>\n'+OWNER_MARK+'\n'+
  '<key>Label</key>'+xmlString(SERVICE_LABEL)+'\n'+
  '<key>ProgramArguments</key><array>'+args.map(xmlString).join('')+'</array>\n'+
  '<key>WorkingDirectory</key>'+xmlString(dirname(scriptPath))+'\n'+
  '<key>RunAtLoad</key><true/>\n'+
  '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n'+
  '<key>ThrottleInterval</key><integer>120</integer>\n'+
  '<key>StandardOutPath</key>'+xmlString(join(stateDir,'mlx-serving.out.log'))+'\n'+
  '<key>StandardErrorPath</key>'+xmlString(join(stateDir,'mlx-serving.err.log'))+'\n'+
  '</dict></plist>\n';
}
export function manageMlxService(action,policyPath,{
 platform=process.platform,uid=process.getuid?.(),home=homedir(),
 nodePath=process.execPath,run=spawnSync,
 scriptPath=resolve(dirname(fileURLToPath(import.meta.url)),'swarm-mlx-verified-serving.mjs'),
}={}){
 if(platform!=='darwin'||!Number.isSafeInteger(uid)||uid<=0)
  throw Error('MLX LaunchAgent requires a current non-root macOS login session');
 if(!['install','uninstall','status'].includes(action))
  throw Error('Use install, uninstall or status');
 if(!isAbsolute(home)||realpathSync(home)!==home)
  throw Error('Home must be a canonical local directory');
 const agents=join(home,'Library','LaunchAgents');
 const plist=join(agents,SERVICE_LABEL+'.plist');
 const domain='gui/'+uid,service=domain+'/'+SERVICE_LABEL;
 if(action==='status'){
  const result=run('/bin/launchctl',['print',service],{encoding:'utf8',timeout:15000});
  return {installed:regular(plist),running:!result.error&&result.status===0};
 }
 if(action==='uninstall'){
  if(!regular(plist))return {installed:false,running:false};
  if(!readFileSync(plist,'utf8').includes(OWNER_MARK))
   throw Error('Refusing to remove an unrelated LaunchAgent');
  const bootout=run('/bin/launchctl',['bootout',domain,plist],{encoding:'utf8',timeout:30000});
  const status=run('/bin/launchctl',['print',service],{encoding:'utf8',timeout:15000});
  if((bootout.error||bootout.status!==0)&&status.status===0)
   throw Error('Cannot remove service configuration while owned process is live');
  unlinkSync(plist);
  return {installed:false,running:false};
 }
 if(!policyPath||!isAbsolute(policyPath)||!regular(policyPath))
  throw Error('Install requires an existing reviewed absolute serving policy');
 const policy=parseServingPolicy(readFileSync(policyPath,'utf8'));
 if(regular(plist))throw Error('Existing MLX service must be explicitly uninstalled first');
 const executable=realpathSync(nodePath),worker=realpathSync(scriptPath);
 if(!lstatSync(executable).isFile()||!lstatSync(worker).isFile())
  throw Error('Node executable and worker must be regular local files');
 mkdirSync(agents,{recursive:true,mode:0o700});
 const payload=mlxLaunchAgent({nodePath:executable,scriptPath:worker,
  policyPath:realpathSync(policyPath),stateDir:policy.stateDir});
 const fd=openSync(plist,'wx',0o600);
 try{writeFileSync(fd,payload);}finally{closeSync(fd);}
 const result=run('/bin/launchctl',['bootstrap',domain,plist],{encoding:'utf8',timeout:30000});
 if(result.error||result.status!==0){
  unlinkSync(plist);
  throw Error('launchd refused MLX serving supervision: '+String(result.stderr??result.error??'').slice(0,200));
 }
 return {installed:true,running:true,plist,service};
}
const option=name=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1];};
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
 try{
  const action=process.argv[2];
  if(!['install','uninstall','status'].includes(action)||
     (action==='install'&&!process.argv.includes('--confirm-install-service')))
   throw Error('Usage: install --policy /absolute/serving-policy.json --confirm-install-service | status | uninstall');
  console.log(JSON.stringify(manageMlxService(action,option('--policy'))));
 }catch(e){
  console.error(JSON.stringify({status:'mlx_launchagent_action_failed',error:String(e)}));
  process.exitCode=1;
 }
}