#!/usr/bin/env node
/** Controlled stdio peer for protocol-contract tests only. */
import {createInterface} from 'node:readline';
const input=createInterface({input:process.stdin});
input.on('line',line=>{
 const msg=JSON.parse(line);
 if(msg.method==='initialize')
  process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:msg.id,result:{
    protocolVersion:'2025-06-18',serverInfo:{name:'test-ruflo-stub',version:'0.0.0'},
    capabilities:{tools:{}},
  }})+'\n');
 else if(msg.method==='tools/list')
  process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:msg.id,result:{
    tools:[{name:'swarm_init',inputSchema:{type:'object'}},
      {name:'memory_search',inputSchema:{type:'object'}}],
  }})+'\n');
 else if(msg.method==='tools/call')throw Error('Probe executed a tool; forbidden.');
});
