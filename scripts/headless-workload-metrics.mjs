/**
 * Dependency-free process-tree memory sample and honest headless summaries.
 * Process listings MUST be from a real operating system, not mocked in the
 * acceptance runner. Unit tests here only pin parsing and accounting.
 */
export function parsePs(snapshot) {
  if (typeof snapshot !== 'string') throw Error('Invalid ps text');
  const rows = new Map();
  for (const line of snapshot.split(/\r?\n/)) {
    const found = /^\s*(\d+)\s+(\d+)\s+(\d+)\s*$/.exec(line);
    if (!found) continue;
    const pid = Number(found[1]), ppid = Number(found[2]), rssKiB = Number(found[3]);
    if (!Number.isSafeInteger(pid) || pid < 1 || !Number.isSafeInteger(ppid) ||
        !Number.isSafeInteger(rssKiB)) continue;
    rows.set(pid,{pid,ppid,rssKiB});
  }
  return rows;
}
export function descendants(rows, rootPid) {
  if (!Number.isSafeInteger(rootPid) || rootPid < 1 || !rows.has(rootPid))
    throw Error('Actual daemon PID is missing from the OS process list');
  const included = new Set([rootPid]);
  let advanced;
  do {
    advanced = false;
    for (const item of rows.values()) {
      if (included.has(item.ppid) && !included.has(item.pid)) {
        included.add(item.pid);advanced = true;
      }
    }
  } while(advanced);
  return [...included].sort((a,b)=>a-b);
}
export function rssForTree(rows, rootPid) {
  const pids = descendants(rows,rootPid);
  return {pids,rssKiB:pids.reduce((sum,pid)=>sum+rows.get(pid).rssKiB,0)};
}
export function summary(level, outcomes, samples) {
  if (!Number.isInteger(level) || level < 1 || !Array.isArray(outcomes) ||
      outcomes.length !== level || !Array.isArray(samples) || !samples.length)
    throw Error('Incomplete real workload measurement');
  const complete = outcomes.filter(x=>x.completed===true && Number.isFinite(x.elapsedMs));
  const durations = complete.map(x=>x.elapsedMs).sort((a,b)=>a-b);
  const q = p => durations.length ? durations[Math.min(durations.length-1,Math.ceil(p*durations.length)-1)]:null;
  const rss = samples.map(x=>x.rssKiB).filter(x=>Number.isFinite(x) && x>=0);
  if (!rss.length) throw Error('No valid measured OS memory samples');
  return {
    sessions:level,completed:complete.length,failed:level-complete.length,
    p50CompletionMs:q(.5),p95CompletionMs:q(.95),
    daemonTreePeakRssMiB:+(Math.max(...rss)/1024).toFixed(2),
    daemonTreeMedianRssMiB:+(rss.sort((a,b)=>a-b)[Math.floor(rss.length/2)]/1024).toFixed(2),
    samples:samples.length,
  };
}
