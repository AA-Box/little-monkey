#!/usr/bin/env node
/** Real TLS-pinned quorum acceptance, never fabricated or model-generated votes. */
import {spawnSync} from 'node:child_process';
const arg=k=>{const i=process.argv.indexOf(k);return i<0?null:process.argv[i+1];};
const group=arg('--group'),binary=arg('--monkey-bin')??'monkey';
const required=Number(arg('--min-voters')??'2');
if(!group||!/^[a-zA-Z0-9_.-]{1,80}$/.test(group)||
 !Number.isInteger(required)||required<2||required>7||
 !process.argv.includes('--confirm-operator-cluster')){
 console.error('Usage: --group GROUP --min-voters 2..7 --confirm-operator-cluster [--monkey-bin PATH]');
 process.exit(2);
}
const call=args=>{
 const r=spawnSync(binary,args,{encoding:'utf8',timeout:60_000,maxBuffer:64*1024});
 if(r.error||r.status!==0||!r.stdout?.trim())throw Error('Native CLI failed: '+args.slice(0,3).join(' '));
 try{return JSON.parse(r.stdout.trim());}catch{throw Error('Native CLI gave malformed JSON');}
};
try{
 const before=call(['peers','cluster-status',group]);
 if(before.group!==group||!before.roster_sha256||
    before.configured_members<required)throw Error('Cluster not configured or too few peer voters');
 const elected=call(['peers','cluster-elect',group]);
 const after=call(['peers','cluster-status',group]);
 if(elected.status!=='elected'||elected.quorum<required||
    elected.quorum<=Math.floor(elected.members/2)||
    elected.leader!==after.local_fingerprint||
    elected.cluster!==group||after.may_schedule!==true||
    after.lease?.term!==elected.term||
    after.lease?.expires_at_ms!==elected.expires_at_ms)
   throw Error('Real signed peer strict majority lease cannot be independently verified');
 console.log(JSON.stringify({version:1,status:'real-signed-peer-quorum-confirmed',
  cluster:group,term:elected.term,quorum:elected.quorum,members:elected.members,
  roster_sha256:before.roster_sha256,expires_at_ms:elected.expires_at_ms,
  warning:'Only signed quorum verified, not live model execution or a physical failover'}));
}catch(error){
 console.error(JSON.stringify({status:'not_verified',error:String(error)}));
 process.exitCode=1;
}
