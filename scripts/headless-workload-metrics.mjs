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
/**
 * Linux proportional set size from /proc/PID/smaps_rollup, in KiB.
 * Unlike RSS, PSS apportions shared pages and is comparable in definition
 * to JCode's reported Linux PSS (only with identical process scope).
 * A missing/denied rollup returns no measurement; NEVER report it as zero.
 */
export function parseLinuxSmapsPssKiB(contents) {
  if(typeof contents!=='string')throw Error('Invalid smaps_rollup contents');
  const matches=[...contents.matchAll(/^Pss:\s+(\d+)\s+kB\s*$/gm)];
  if(matches.length!==1)throw Error('Missing or ambiguous Linux Pss in smaps_rollup');
  const pss=Number(matches[0][1]);
  if(!Number.isSafeInteger(pss)||pss<0)throw Error('Invalid OS PSS size');
  return pss;
}

/** A complete PSS snapshot must cover every PID included in the RSS tree.
 * Missing processes/permission denials yield null, not guessed values. */
export function completeTreePssKiB(pids, rollupByPid) {
  if(!Array.isArray(pids)||!pids.length||!(rollupByPid instanceof Map))return null;
  let total=0;
  for(const pid of pids) {
    const bytes=rollupByPid.get(pid);
    if(typeof bytes!=='string')return null;
    try {total+=parseLinuxSmapsPssKiB(bytes);}
    catch {return null;}
  }
  return total;
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

/**
 * Independent success oracle for real durable task acceptance.
 * A non-JSON stdout line, model assertion of success, unknown outcome,
 * foreign run event, or output lacking the actual task fixture marker fails.
 */
export function verifyDurableModelCompletion(id, statusRows, eventStdout, marker) {
  if (!Array.isArray(statusRows) || statusRows.length !== 1 ||
      statusRows[0]?.run_id !== id || statusRows[0]?.status !== 'succeeded')
    throw Error('Durable task ledger did not report exactly one succeeded run');
  let assistantText = '';
  let eventCount = 0;
  for (const line of eventStdout.split(/\r?\n/)) {
    if (!line.trim()) continue;
    const entry = JSON.parse(line);
    if (entry.run_id !== id || !Number.isSafeInteger(entry.sequence) ||
        entry.sequence < 1 || !entry.event || typeof entry.event !== 'object')
      throw Error('Invalid or cross-run native event');
    eventCount++;
    if (entry.event.type === 'model_delta' && entry.event.payload?.channel === 'assistant' &&
        typeof entry.event.payload.text === 'string')
      assistantText += entry.event.payload.text;
  }
  if (!eventCount || !assistantText.includes(marker))
    throw Error('Completed task lacks actual assistant fixture evidence');
  return true;
}
