#!/usr/bin/env node
/**
 * Private, operator-authorized real-vendor-export acceptance.
 * Never upload sessions to GitHub/CI, print conversation text, or generate
 * pretend JSONL fixtures. Reuse the parser's fail-closed five-format suite.
 */
import {existsSync,readFileSync,writeFileSync,mkdirSync,copyFileSync,realpathSync,lstatSync,chmodSync} from 'node:fs';
import {join,resolve,sep} from 'node:path';
import {homedir,tmpdir} from 'node:os';
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
const argv=process.argv.slice(2);
const val=(flag)=>{const i=argv.indexOf(flag);return i<0?null:argv[i+1];};
const has=(flag)=>argv.includes(flag);
const dir=resolve(val('--out')??'private-agent-evidence');
const inputs=[
 {id:'claude',env:'LM_REAL_CLAUDE_CODE_JSONL',flag:'--claude-file',
  base:join(homedir(),'.claude','projects'),name:'claude.jsonl'},
 {id:'codex',env:'LM_REAL_CODEX_JSONL',flag:'--codex-file',
  base:join(homedir(),'.codex','sessions'),name:'codex.jsonl'},
 {id:'pi',env:'LM_REAL_PI_JSONL',flag:'--pi-file',
  base:join(homedir(),'.pi','agent','sessions'),name:'pi.jsonl'},
];
function die(message){throw Error(message);}
function command(bin,args,cwd,env=process.env){
 const result=spawnSync(bin,args,{cwd,env,encoding:'utf8',timeout:180_000,
  maxBuffer:32*1024*1024});
 if(result.error||result.status!==0)
  die('Official '+bin+' failed: '+String(result.error?.message??result.stderr??'unknown').slice(0,200));
 if(!result.stdout.trim())die('Official CLI returned no export');
 return result.stdout;
}
function safeStored(root,name){
 const parent=resolve(root);
 mkdirSync(parent,{recursive:true,mode:0o700});
 if(lstatSync(parent).isSymbolicLink())die('Evidence directory cannot be symlinked');
 const full=join(parent,name);
 if(existsSync(full))die('Refusing to overwrite previous evidence: '+name);
 return full;
}
function officialFile(filename,base){
 const expected=realpathSync(base);
 const path=realpathSync(resolve(filename));
 if(!path.startsWith(expected+sep))die('Session must be an authentic file under '+base);
 if(!lstatSync(path).isFile())die('Source is not a regular vendor-session file');
 const bytes=readFileSync(path);
 if(bytes.length===0||bytes.length>32*1024*1024)die('Source session outside parser size limit');
 return path;
}
function save(value,name){
 if(Buffer.byteLength(value)>32*1024*1024)die('Vendor export exceeds importer limits');
 const file=safeStored(dir,name);
 writeFileSync(file,value,{mode:0o600,flag:'wx'});
 chmodSync(file,0o600);
 return {file,sha256:createHash('sha256').update(value).digest('hex')};
}
function run(){
 if(!has('--explicit-private-export-consent'))die(
  'Add --explicit-private-export-consent after reviewing sensitive prompts, tool output and file content risks');
 const evidence={};
 for(const item of inputs){
  const source=val(item.flag);
  if(!source)die('Missing required official session path '+item.flag);
  const path=officialFile(source,item.base);
  evidence[item.env]=save(readFileSync(path),item.name);
 }
 const session=val('--opencode-session');
 if(!session||!/^[a-zA-Z0-9_-]{4,128}$/.test(session))
  die('Provide an existing OpenCode session id via --opencode-session');
 const opencode=command(process.env.OPENCODE_BIN??'opencode',
  ['export','--sanitize',session],process.cwd());
 evidence.LM_REAL_OPENCODE_JSON=save(opencode,'opencode.json');
 if(!has('--allow-cursor-agent-execution'))
  die('Cursor print mode can execute shell commands; explicitly pass --allow-cursor-agent-execution to run it in a temporary empty cwd');
 const temporary=join(tmpdir(),'little-monkey-real-cursor-'+process.pid);
 mkdirSync(temporary,{mode:0o700});
 // Cursor's real agent can still reach files outside cwd. Never equate the
 // empty directory with a sandbox. This is an explicit, opted-in operation.
 const cursor=command(process.env.CURSOR_AGENT_BIN??'cursor-agent',
  ['--print','--output-format','stream-json',
   'Reply with exactly: ready. Do not invoke tools. Do not access files.'],
  temporary);
 evidence.LM_REAL_CURSOR_AGENT_STREAM_JSON=save(cursor,'cursor.jsonl');
 const env={...process.env,LM_REQUIRE_REAL_CODING_EXPORTS:'1'};
 for(const [key,record] of Object.entries(evidence))env[key]=record.file;
 const result=spawnSync('pnpm',['exec','vitest','run',
  'src/lib/codingSessionImport.realExports.test.ts'],{
  cwd:process.cwd(),env,stdio:'inherit',timeout:180_000});
 if(result.error||result.status!==0)die('Authentic five-agent import acceptance failed');
 const manifest={schema:1,acceptance:'all-five-original-vendor-exports',
  captures:Object.fromEntries(Object.entries(evidence).map(([key,v])=>
   [key,{name:v.file.slice(dir.length+1),sha256:v.sha256}])),
  parser_tests:'passed',source:'real installed clients'};
 save(JSON.stringify(manifest,null,2)+'\n','manifest.json');
 console.log('ACCEPTED: five genuinely exported and successfully imported vendor transcripts. Private evidence in '+dir);
}
try{run()}catch(error){console.error('VENDOR EXPORT ACCEPTANCE FAILED:',error.message);process.exitCode=1;}
