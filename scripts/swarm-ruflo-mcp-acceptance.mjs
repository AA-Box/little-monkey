#!/usr/bin/env node
/** Operator-only real Ruflo MCP STDIO compatibility check.
 * The process receives a deliberately restricted environment. This probe
 * invokes only MCP initialize + tools/list, never tools/call, resources/read,
 * prompts/get, hooks or plugin executables. It does not enable the connector.
 */
import {spawn} from 'node:child_process';
import {isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {resolve} from 'node:path';
export const MCP_VERSION='2025-06-18';
const MAX_TOOLS=512,MAX_OUTPUT=1024*1024;
export async function probeRufloMcp(binary,args=[],timeoutMs=25000){
 if(typeof binary!=='string'||!isAbsolute(binary)||!Array.isArray(args)||
   args.length>12||args.some(arg=>typeof arg!=='string'||arg.length>250||/[\x00-\x1f]/.test(arg))||
   !Number.isSafeInteger(timeoutMs)||timeoutMs<1000||timeoutMs>120000)
   throw Error('Use a local absolute executable, bounded argv and timeout.');
 return await new Promise((resolve,reject)=>{
  // Never inherit tokens, provider API keys, arbitrary npm hooks, or GitHub
  // access merely for a read-only interoperability probe.
  const env={PATH:process.env.PATH??'',HOME:process.env.HOME??'',
   TMPDIR:process.env.TMPDIR??'/tmp',TEMP:process.env.TEMP??''};
  const child=spawn(binary,args,{stdio:['pipe','pipe','ignore'],env,shell:false});
  let output='',state='init',settled=false,serverName='external';
  const clear=()=>{clearTimeout(timer);child.stdout.removeAllListeners('data');};
  const fail=(message)=>{if(settled)return;settled=true;clear();child.kill();reject(Error(message));};
  const done=(value)=>{if(settled)return;settled=true;clear();child.kill();resolve(value);};
  const send=object=>{if(!child.stdin.destroyed)child.stdin.write(JSON.stringify(object)+'\n');};
  const timer=setTimeout(()=>fail('Ruflo MCP read-only handshake exceeded timeout'),timeoutMs);
  child.on('error',()=>fail('Ruflo MCP process could not start.'));
  child.on('exit',()=>fail('Ruflo MCP exited before completing tool discovery.'));
  child.stdout.on('data',bytes=>{
   output+=bytes.toString('utf8');
   if(output.length>MAX_OUTPUT)return fail('Ruflo MCP response exceeded 1 MiB.');
   for(;;){
    const index=output.indexOf('\n');if(index<0)break;
    const line=output.slice(0,index).trim();output=output.slice(index+1);
    if(!line||!line.startsWith('{'))continue;
    let msg;try{msg=JSON.parse(line);}catch{return fail('Malformed Ruflo MCP JSON-RPC response.');}
    if(msg.id===1&&state==='init'){
     if(msg.error||!msg.result?.protocolVersion||!msg.result?.serverInfo?.name)
       return fail('MCP initialize response lacks server identity and negotiated protocol.');
     const peerVersion=msg.result.protocolVersion;
     if(typeof peerVersion!=='string'||peerVersion.length>32)
       return fail('MCP protocol negotiation invalid.');
     serverName=msg.result.serverInfo.name;
     state='list';
     send({jsonrpc:'2.0',method:'notifications/initialized'});
     send({jsonrpc:'2.0',id:2,method:'tools/list',params:{}});
    }else if(msg.id===2&&state==='list'){
     if(msg.error||!Array.isArray(msg.result?.tools)||msg.result.tools.length<1||
        msg.result.tools.length>MAX_TOOLS||msg.result.tools.some(t=>
          typeof t?.name!=='string'||t.name.length>150||!t.name.trim()))
       return fail('MCP tool discovery returned no valid bounded tool catalog.');
     if(msg.result.nextCursor) return fail('MCP tool list is paginated; cannot certify incomplete discovery.');
     const names=msg.result.tools.map(t=>t.name);
     if(new Set(names).size!==names.length)return fail('MCP tool catalog contains duplicate names.');
     done({status:'mcp-handshake-and-discovery-passed',server:serverName,
       toolCount:names.length,
       groups:{swarm:names.filter(x=>x.startsWith('swarm_')).length,
        memory:names.filter(x=>x.startsWith('memory_')).length,
        hooks:names.filter(x=>x.startsWith('hooks_')).length},
       tools:names.sort(),executedTools:0,allowedExternalTools:0});
    }
   }
  });
  send({jsonrpc:'2.0',id:1,method:'initialize',params:{
    protocolVersion:MCP_VERSION,capabilities:{},clientInfo:{name:'little-monkey-ruflo-probe',version:'1.0.0'},
  }});
 });
}
function option(key){const i=process.argv.indexOf(key);return i<0?null:process.argv[i+1];}
async function main(){
 const bin=option('--binary'),version=option('--ruflo-version'),args=option('--args-json');
 if(!bin||!version||!args||!/^([1-9]\d{0,2})\.(0|[1-9]\d{0,2})\.(0|[1-9]\d{0,2})(?:-(?:alpha|beta|rc)\.(?:0|[1-9]\d{0,3}))?$/.test(version)||
    !process.argv.includes('--confirm-external-process'))
   throw Error('Provide --binary /absolute/executable --ruflo-version EXACT --args-json JSON_ARRAY --confirm-external-process.');
 const parsed=JSON.parse(args);
 if(!Array.isArray(parsed)||!parsed.includes('ruflo@'+version)||
    !parsed.includes('mcp')||!parsed.includes('start'))
   throw Error('Requested external MCP command does not include the pinned Ruflo package and mcp start.');
 const result=await probeRufloMcp(bin,parsed);
 console.log(JSON.stringify({...result,expectedRufloVersion:version,
   note:'MCP discovery only: not full Ruflo plugin, hook or swarm parity.'}));
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
 main().catch(error=>{console.error(JSON.stringify({status:'mcp-probe-failed',error:String(error)}));process.exitCode=1;});
}
