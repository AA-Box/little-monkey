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
// CI's OS process table contains short-lived shell/git/compiler children that
// can exit between 'ps' and '/proc/PID/smaps_rollup'. Exercise a REAL stable
// kernel PSS reading from THIS live Node process; separately verify the
// process-table parser captures it. Full-tree completeness belongs to the
// real model benchmark, where missing rollups correctly produce null.
if(!snapshot.pids.includes(process.pid))
  throw Error('Real OS process snapshot omitted the running test process');
const rollups=new Map([[
  process.pid,readFileSync('/proc/'+process.pid+'/smaps_rollup','utf8'),
]]);
const pssKiB=completeTreePssKiB([process.pid],rollups);
if(!Number.isSafeInteger(pssKiB)||pssKiB<=0||snapshot.rssKiB<=0)
  throw Error('Real Linux OS memory sampling did not produce valid RSS+PSS');
console.log(JSON.stringify({
  kind:'real-os-pss-smoke',skipped:false,pid:process.pid,
  measuredPids:1,observedTreePids:snapshot.pids.length,
  scope:'current stable Node process PSS; real process tree RSS',
  rssKiB:snapshot.rssKiB,pssKiB,source:'Linux ps and /proc smaps_rollup',
}));
