#!/usr/bin/env node
/** Three genuine child processes, private SQLite roots, six pinned TLS
 * pairings, majority failover, signed replica readback and new DAG import.
 * Linux CI only; never run against users' real devices or model accounts. */
import {spawn,spawnSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,writeFileSync,existsSync,rmSync,statSync} from 'node:fs';
import {join,isAbsolute} from 'node:path';
import {tmpdir} from 'node:os';
import {createServer,connect} from 'node:net';
import {setTimeout as delay} from 'node:timers/promises';
const opt=x=>{const i=process.argv.indexOf(x);return i<0?null:process.argv[i+1]??null};
const bin=opt('--monkey-bin');
const check=(condition,message)=>{if(!condition)throw Error(message)};
function cmd(exe,args,env,timeout=60000){
 const r=spawnSync(exe,args,{env,encoding:'utf8',timeout,maxBuffer:1048576});
 if(r.error||r.status!==0)throw Error('Native step failed: '+exe.split('/').at(-1)+' '+args.slice(0,2).join(' '));
 return r.stdout.trim();
}
function run(node,args){return cmd(bin,args,node.env)}
function json(node,args){try{return JSON.parse(run(node,args))}catch{throw Error('Native JSON contract failed: '+args[0]+' '+args[1])}}
const children=new Set();
async function port(){
 return new Promise((ok,fail)=>{
  const server=createServer();server.once('error',fail);
  server.listen(0,'127.0.0.1',()=>{const p=server.address().port;server.close(e=>e?fail(e):ok(p))});
 });
}
async function launch(node,mode){
 const child=spawn(bin,mode==='full'?['daemon','serve']:['daemon','remote','host-serve'],
  {env:node.env,stdio:'ignore'});
 node.child=child;children.add(child);
 for(let i=0;i<65;i++){
  if(child.exitCode!==null||child.signalCode!==null)throw Error('Independent native peer exited');
  const online=await new Promise(ok=>{
   const sock=connect({host:'127.0.0.1',port:node.port});
   sock.setTimeout(300);
   sock.on('connect',()=>{sock.destroy();ok(true)});
   sock.on('error',()=>{sock.destroy();ok(false)});
   sock.on('timeout',()=>{sock.destroy();ok(false)});
  });
  if(online)return;
  await delay(200);
 }
 throw Error('Native signed peer listener never bound');
}
async function stop(node){
 const child=node.child;if(!child)return;
 child.kill('SIGTERM');
 for(let i=0;i<20;i++){if(child.exitCode!==null||child.signalCode!==null)break;await delay(100)}
 if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');
 children.delete(child);node.child=null;
}
async function main(){
 if(process.argv.includes('--self-check')){
  console.log(JSON.stringify({status:'contract_passed',mode:'no_network'}));return;
 }
 check(process.platform==='linux'&&process.argv.includes('--confirm-disposable')&&
  bin&&isAbsolute(bin)&&existsSync(bin)&&statSync(bin).isFile(),
  'Only disposable Linux with explicitly selected built native CLI is accepted');
 const root=mkdtempSync(join(tmpdir(),'lm-three-process-'));
 const nodes=[],by={},incoming={a:{},b:{},c:{}},group='ci-signed-dag';
 try{
  for(const name of ['a','b','c']){
   const dir=join(root,name),home=join(dir,'home'),data=join(dir,'data'),auth=join(dir,'auth');
   for(const x of [dir,home,data,auth])mkdirSync(x,{recursive:true,mode:0o700});
   const p=await port(),cert=join(dir,'cert.pem'),key=join(dir,'key.pem'),cfg=join(dir,'openssl.cnf');
   writeFileSync(cfg,[
    '[req]','prompt = no','distinguished_name = dn','x509_extensions = leaf',
    '[dn]','CN = 127.0.0.1','[leaf]','subjectAltName = IP:127.0.0.1',
    'basicConstraints = CA:FALSE','keyUsage = digitalSignature,keyEncipherment',
    'extendedKeyUsage = serverAuth',''
   ].join('\n'),{mode:0o600});
   cmd('openssl',['req','-x509','-newkey','rsa:2048','-nodes','-days','1',
    '-config',cfg,'-extensions','leaf','-keyout',key,'-out',cert],process.env,20000);
   const node={name,port:p,dir,child:null,env:{...process.env,HOME:home,
    XDG_DATA_HOME:data,XDG_CONFIG_HOME:join(dir,'config'),LITTLE_MONKEY_HOME:auth}};
   nodes.push(node);by[name]=node;
   run(node,['daemon','remote','host-configure','--listen','127.0.0.1:'+p,
    '--advertise-url','https://127.0.0.1:'+p,
    '--tls-certificate',cert,'--tls-private-key',key]);
   node.fp=json(node,['daemon','remote','host-status','--json']).certificate_sha256;
   check(/^[0-9a-f]{64}$/.test(node.fp),'No real node certificate pin');
   await launch(node,'host');
  }
  check(new Set(nodes.map(n=>n.fp)).size===3,'Peers share TLS keys');
  // Full directed operator pairing; no mocked identities or credentials.
  for(const owner of nodes)for(const client of nodes){
   if(owner===client)continue;
   const file=join(root,owner.name+'-invites-'+client.name+'.json');
   const before=new Set(json(owner,['peers','list','--json']).inbound.map(x=>x.device_id));
   json(owner,['peers','invite',client.name,'--allow','task','--output',file,'--json']);
   const receipt=json(client,['peers','accept',file,owner.name,'--json']);
   check(receipt.certificate_sha256===owner.fp,'TLS pin not checked on acceptance');
   const newly=json(owner,['peers','list','--json']).inbound.filter(x=>!before.has(x.device_id));
   check(newly.length===1&&newly[0].state==='active','Real signed pairing missing');
   incoming[owner.name][client.name]=newly[0].device_id;
  }
  for(const node of nodes){
   const members=nodes.map(x=>x===node?
    {fingerprint:x.fp,device_id:'@local',alias:null}:
    {fingerprint:x.fp,device_id:incoming[node.name][x.name],alias:x.name});
   const file=join(node.dir,'roster.json');
   writeFileSync(file,JSON.stringify({version:1,group,members}),{mode:0o600});
   const proof=json(node,['peers','cluster-setup',file]);
   check(proof.members===3,'Invalid enrolled TLS roster');
   node.digest=proof.roster_sha256;
  }
  check(new Set(nodes.map(n=>n.digest)).size===1,'Peer rosters differ');
  const a=by.a,b=by.b;
  const first=json(a,['peers','cluster-elect',group,'--term','1']);
  check(first.status==='elected'&&first.quorum>=2&&first.leader===a.fp,
    'A failed to elect from actual signed peer votes');
  const ws=join(a.dir,'workspace');mkdirSync(ws,{mode:0o700});
  const recipe=join(a.dir,'plan.json'),spec=join(a.dir,'spec.json');
  writeFileSync(recipe,JSON.stringify({version:1,name:'ci-plan',
   target:{ollama:'ci-disposable-nonexistent:1'},workspace:ws,
   permission_mode:'plan',prompt:'Read-only CI graph fixture',params:{}}),{mode:0o600});
  writeFileSync(spec,JSON.stringify({version:1,id:'ci-original',cluster_id:group,
   max_parallel:1,nodes:[{id:'audit',recipe,depends_on:[],max_runtime_seconds:60}]}),{mode:0o600});
  check(json(a,['daemon','swarm-submit',spec]).id==='ci-original','Original DAG absent');
  const bundle=join(root,'explicit-operator-bundle.json');
  json(a,['daemon','swarm-export','ci-original',bundle]);
  for(const peer of ['b','c']){
   const r=json(a,['daemon','swarm-replicate',group,'ci-original',peer]);
   check(r.remote_readback_verified===true&&r.execution_authorized===false,
    'Peer SQLite checkpoint did not independently read back');
  }
  const wsB=join(b.dir,'workspace');mkdirSync(wsB,{mode:0o700});
  const armed=json(b,['daemon','swarm-failover-arm','recover-on-b',
   '--group',group,'--source-peer','a','--new-id','ci-recovered',
   '--bundle',bundle,'--workspace',wsB,'--confirm-read-only-reexecution']);
  check(armed.state==='armed','Destination operator did not approve recovery');
  await stop(a);await stop(b);await launch(b,'full');
  let term=0,imported=false;
  const started=Date.now();
  while(Date.now()-started<90000){
   await delay(2000);
   check(b.child?.exitCode===null&&b.child?.signalCode===null,
    'Destination resident daemon unexpectedly exited');
   const status=json(b,['peers','cluster-status',group]);
   if(status.may_schedule!==true)continue;
   term=status.lease?.term??0;
   const plans=json(b,['daemon','swarm-failover-status']).plans??[];
   const plan=plans.find(x=>x.id==='recover-on-b');
   check(plan?.state!=='failed','Native recovery rejected an actual majority');
   if(plan?.state==='imported'){imported=true;break}
  }
  check(imported&&term>first.term,'B+C did not recover source graph after partition');
  const recovered=json(b,['daemon','swarm-status','--id','ci-recovered']);
  check(recovered.graphs?.some(x=>x.id==='ci-recovered'),
   'Real daemon did not persist replacement graph');
  console.log(JSON.stringify({status:'verified_real_three_process_TLS_recovery',
   processCount:3,originalTerm:first.term,recoveryTerm:term,
   evidence:'signed full-mesh peer TLS, durable remote SQLite and leader takeover',
   scope:'separate processes on one ephemeral Linux runner; no real model or physical hosts'}));
 }finally{
  for(const node of nodes.reverse())await stop(node);
  rmSync(root,{recursive:true,force:true});
 }
}
main().catch(e=>{
 console.error(JSON.stringify({status:'not_verified',
  reason:String(e.message||e).slice(0,240)}));
 process.exitCode=1;
});
