import { execFile } from 'node:child_process';

const command = (file,args,timeout=2500) => new Promise((ok,bad)=>{
  execFile(file,args,{encoding:'utf8',timeout,maxBuffer:4*1024*1024},(err,out)=>{
    if(err)bad(err);else ok(out);
  });
});

/** PID is reported by Little Monkey's installed resident daemon contract,
 * not inferred from an untrusted process name. */
export async function residentDaemonPid(binary) {
  try {
    const output=await command(binary,['daemon','status','--json']);
    const status=JSON.parse(output);
    if(status.service_running!==true||status.heartbeat_fresh!==true||
      !Number.isSafeInteger(status.pid)||status.pid<1) return null;
    return status.pid;
  } catch {return null;}
}

/** POSIX ps RSS is in KiB. Process-tree sum is resident footprint, NOT PSS
 * and not a unique-page memory metric (shared pages may be counted twice). */
export function parsePosixTreeRss(stdout,pid) {
  if(!Number.isSafeInteger(pid)||pid<1)return null;
  const processes=new Map();
  for(const line of stdout.split(/\r?\n/)){
    const m=/^\s*(\d+)\s+(\d+)\s+(\d+)\s*$/.exec(line);
    if(!m)continue;
    const id=Number(m[1]),parent=Number(m[2]),rss=Number(m[3]);
    if([id,parent,rss].every(Number.isSafeInteger) && rss>=0)
      processes.set(id,{parent,rssBytes:rss*1024});
  }
  if(!processes.has(pid))return null;
  const descendants=new Set([pid]);
  for(let pass=0;pass<processes.size;pass++){
    let changed=false;
    for(const [id,entry] of processes)if(!descendants.has(id)&&descendants.has(entry.parent)){
      descendants.add(id);changed=true;
    }
    if(!changed)break;
  }
  let sum=0;
  for(const id of descendants)sum+=processes.get(id).rssBytes;
  return Number.isSafeInteger(sum)?sum:null;
}
export function parseWindowsTreeRss(output,pid){
  if(!Number.isSafeInteger(pid)||pid<1)return null;
  let rows;
  try{const value=JSON.parse(output);rows=Array.isArray(value)?value:[value];}catch{return null;}
  const table=new Map();
  for(const item of rows){
    if(!item||typeof item!=='object')continue;
    const id=Number(item.ProcessId),parent=Number(item.ParentProcessId),rss=Number(item.WorkingSetSize);
    if(Number.isSafeInteger(id)&&id>0&&Number.isSafeInteger(parent)&&Number.isSafeInteger(rss)&&rss>=0)
      table.set(id,{parent,rssBytes:rss});
  }
  if(!table.has(pid))return null;
  const members=new Set([pid]);
  for(let pass=0;pass<table.size;pass++){
    let changed=false;
    for(const [id,row] of table)if(!members.has(id)&&members.has(row.parent)){
      members.add(id);changed=true;
    }
    if(!changed)break;
  }
  const sum=[...members].reduce((n,id)=>n+table.get(id).rssBytes,0);
  return Number.isSafeInteger(sum)?sum:null;
}
export async function sampleResidentTreeRss(pid) {
  try{
    if(process.platform==='win32'){
      const output=await command('powershell.exe',['-NoProfile','-NonInteractive','-Command',
        'Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,WorkingSetSize | ConvertTo-Json -Compress'],10000);
      return parseWindowsTreeRss(output,pid);
    }
    return parsePosixTreeRss(await command('ps',['-e','-o','pid=,ppid=,rss=']),pid);
  }catch{return null;}
}
