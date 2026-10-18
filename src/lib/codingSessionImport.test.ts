import { describe, expect, it } from "vitest";
import { parseCodingSession } from "./codingSessionImport";
const jsonl=(records: unknown[])=>records.map(record=>JSON.stringify(record)).join("\n");

describe("resumable coding-session parsing",()=>{
  it("imports only user and visible assistant text from Claude Code",()=>{
    const transcript=jsonl([
      {type:"user",uuid:"1",timestamp:"2026-01-01T12:00:00Z",message:{role:"user",content:"fix my CI"}},
      {type:"assistant",uuid:"2",message:{id:"response-1",role:"assistant",content:[
        {type:"thinking",thinking:"private reasoning"},
        {type:"text",text:"Working..."},
        {type:"tool_use",id:"tool",name:"Bash",input:{command:"delete all"}}]}},
      {type:"assistant",uuid:"3",message:{id:"response-1",role:"assistant",content:[{type:"text",text:"CI fixed"}]}},
      {type:"user",uuid:"4",message:{role:"user",content:[{type:"tool_result",content:"secret token"}]}}
    ]);
    const parsed=parseCodingSession(transcript)!;
    expect(parsed.format).toBe("claude-code");
    expect(parsed.messages.map(msg=>msg.content)).toEqual(["fix my CI","Working...\nCI fixed"]);
    expect(parsed.createdAt).toBe(Date.parse("2026-01-01T12:00:00Z"));
    expect(JSON.stringify(parsed)).not.toMatch(/delete all|private reasoning|secret token/);
  });

  it("collapses cumulative Claude Code snapshots without duplicating text",()=>{
    const parsed=parseCodingSession(jsonl([
      {type:"user",message:{role:"user",content:"Question"}},
      {type:"assistant",message:{id:"response",role:"assistant",content:[{type:"text",text:"Hello"}]}},
      {type:"assistant",message:{id:"response",role:"assistant",content:[{type:"text",text:"Hello world"}]}},
      {type:"assistant",message:{id:"response",role:"assistant",content:[{type:"text",text:"Hello world"}]}}
    ]))!;
    expect(parsed.messages.map(msg=>msg.content)).toEqual(["Question","Hello world"]);
  });

  it("imports Codex responses without duplicating event messages",()=>{
    const parsed=parseCodingSession(jsonl([
      {type:"session_meta",timestamp:"2026-05-01T00:00:00Z",payload:{id:"run",timestamp:"2026-05-01T00:00:00Z"}},
      {type:"event_msg",payload:{type:"user_message",message:"hello"}},
      {type:"response_item",payload:{type:"message",role:"user",content:[{type:"input_text",text:"hello"}]}},
      {type:"response_item",payload:{type:"function_call",name:"shell",arguments:"dangerous"}},
      {type:"response_item",payload:{type:"message",role:"assistant",content:[{type:"output_text",text:"answer"}]}}
    ]))!;
    expect(parsed.format).toBe("codex");
    expect(parsed.messages.map(msg=>msg.content)).toEqual(["hello","answer"]);
    expect(JSON.stringify(parsed)).not.toContain("dangerous");
  });

  it("imports the active Pi branch without abandoned branches or tool results",()=>{
    const parsed=parseCodingSession(jsonl([
      {type:"session",version:3,id:"s",cwd:"/private/project",timestamp:"2026-06-01T00:00:00Z"},
      {type:"message",id:"u",parentId:null,message:{role:"user",content:"fix"}},
      {type:"message",id:"old",parentId:"u",message:{role:"assistant",content:[{type:"text",text:"abandoned"}]}},
      {type:"message",id:"new",parentId:"u",message:{role:"assistant",content:[{type:"text",text:"working"}]}},
      {type:"message",id:"tool",parentId:"new",message:{role:"toolResult",content:[{type:"text",text:"tool secret"}]}}
    ]))!;
    expect(parsed.format).toBe("pi");
    expect(parsed.messages.map(msg=>msg.content)).toEqual(["fix","working"]);
    expect(JSON.stringify(parsed)).not.toContain("tool secret");
  });

  it("imports OpenCode official JSON exports with message parts",()=>{
    const parsed=parseCodingSession(JSON.stringify({
      info:{id:"ses1",title:"My project",time:{created:1720000000000}},
      messages:[
        {info:{role:"user"},parts:[{type:"text",text:"Implement X"}]},
        {info:{role:"assistant"},parts:[{type:"reasoning",text:"private"},{type:"text",text:"Completed X"}]},
        {info:{role:"tool"},parts:[{type:"text",text:"a secret"}]}
      ]
    }))!;
    expect(parsed.format).toBe("opencode");
    expect(parsed.title).toBe("My project");
    expect(parsed.messages.map(msg=>msg.content)).toEqual(["Implement X","Completed X"]);
    expect(JSON.stringify(parsed)).not.toMatch(/private|a secret/);
  });

  it("imports Cursor CLI stream-json once, excluding tool output and credentials",()=>{
    const parsed=parseCodingSession(jsonl([
      {type:"system",subtype:"init",session_id:"cursor-1",apiKeySource:"login",cwd:"/secret/project"},
      {type:"user",session_id:"cursor-1",message:{role:"user",content:[{type:"text",text:"Fix tests"}]}},
      {type:"assistant",session_id:"cursor-1",message:{role:"assistant",content:[{type:"text",text:"I will "}]}},
      {type:"tool_call",session_id:"cursor-1",subtype:"completed",tool_call:{readToolCall:{result:{success:{content:"sensitive token"}}}}},
      {type:"assistant",session_id:"cursor-1",message:{role:"assistant",content:[{type:"text",text:"fix tests."}]}},
      {type:"result",subtype:"success",is_error:false,session_id:"cursor-1",result:"I will fix tests."}
    ]))!;
    expect(parsed.format).toBe("cursor");
    expect(parsed.messages.map(m=>m.content)).toEqual(["Fix tests","I will fix tests."]);
    expect(JSON.stringify(parsed)).not.toMatch(/sensitive token|\/secret\/project|login/);
  });

  it("rejects Cursor CLI failed or mixed-session transcripts",()=>{
    const init={type:"system",subtype:"init",session_id:"one"};
    const user={type:"user",session_id:"one",message:{role:"user",content:"Hi"}};
    expect(parseCodingSession(jsonl([init,user,{type:"result",session_id:"one",subtype:"error",is_error:true}]))).toBeNull();
    expect(parseCodingSession(jsonl([init,user,{type:"assistant",session_id:"two",message:{role:"assistant",content:"Other"}}]))).toBeNull();
    expect(parseCodingSession(jsonl([{type:"result",subtype:"success",result:"Unpaired output"}]))).toBeNull();
  });

  it("rejects unsupported and truncated formats without silently dropping events",()=>{
    expect(parseCodingSession("{}")).toBeNull();
    expect(parseCodingSession("")).toBeNull();
    expect(()=>parseCodingSession('{"type":"session_meta","payload":{}}\n{bad')).toThrow(/Invalid coding transcript JSONL/);
    expect(()=>parseCodingSession("x".repeat(32*1024*1024+1))).toThrow(/32 MiB/);
  });
});
