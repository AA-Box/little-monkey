#!/usr/bin/env node
/**
 * Actual Linux OS memory sampling, running under CI without a model.
 * This tests /proc integration (NOT AI inference/performance).
 */
import {spawnSync} from 'node:child_process';
import {readFileSync} from 'node:fs';
import {parsePs,rssForTree,completeTreePssKiB} from './headless-workload-metrics.mjs';

if(process.platform!=='linux'){
  console.log(JSON.stringify({kind:'real-os-pss-smoke',skipped:true,
    reason:'Linux smaps_rollup required',platform:process.platform}));
  process.exit(0);
}
const table=spawnSync('ps',['-axo','pid=,ppid=,rss='],{
  encoding:'utf8',timeout:15_000,maxBuffer:12*1024*1024,
});
if(table.error || table.status!==0)throw Error('Could not query real Linux process tree');
const snapshot=rssForTree(parsePs(table.stdout),process.pid);
const rollups=new Map();
for(const pid of snapshot.pids){
  // Never fill in a guessed PSS value when the kernel denies a real read.
  rollups.set(pid,readFileSync('/proc/'+pid+'/smaps_rollup','utf8'));
}
const pssKiB=completeTreePssKiB(snapshot.pids,rollups);
if(!Number.isSafeInteger(pssKiB)||pssKiB<=0||snapshot.rssKiB<=0)
  throw Error('Real Linux OS memory sampling did not produce valid RSS+PSS');
console.log(JSON.stringify({
  kind:'real-os-pss-smoke',skipped:false,pid:process.pid,
  measuredPids:snapshot.pids.length,
  rssKiB:snapshot.rssKiB,pssKiB,source:'Linux ps and /proc smaps_rollup',
}));
