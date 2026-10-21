/**
 * In-memory, turn-scoped context exposure tracker for executing desktop agents.
 *
 * Never suppress a changed result or span a new turn (a model's context may
 * have been summarized/rebuilt). Agent identity and managed worktree are part
 * of the key so concurrent child turns cannot inherit one another's reads.
 * No filesystem IO or long-lived disk persistence; only identical successful
 * tool payloads are compacted after the native tool has actually run.
 */
export class ContextExposureTracker {
  private readonly seen = new Map<string,{response:string;lastSeen:number}>();
  constructor(private readonly maxEntries=48,private readonly maxBytes=512*1024) {}
  observe(scope:string,tool:"read_file"|"grep",args:Record<string,unknown>,response:string):string {
    if (!scope || !response || response.length>this.maxBytes) return response;
    try {
      const parsed:unknown=JSON.parse(response);
      if (parsed && typeof parsed==="object" && "error" in parsed) return response;
    } catch { /* successful plain text is allowed */ }
    const target=tool==="read_file" ? args.path : args.path ?? ".";
    const query=tool==="grep" ? args.pattern : "";
    if(typeof target!=="string" || typeof query!=="string")return response;
    const key=JSON.stringify([scope,tool,target,query,tool==="grep" && args.structure===true]);
    const prior=this.seen.get(key);
    if(prior && prior.response===response) {
      this.seen.delete(key);
      this.seen.set(key,{response,lastSeen:Date.now()});
      return JSON.stringify({
        already_exposed:true,tool,path:target,
        original_bytes:new TextEncoder().encode(response).length,
        note:"Identical result already returned earlier this turn. Content unchanged; no redundant copy emitted."
      });
    }
    this.seen.delete(key);
    this.seen.set(key,{response,lastSeen:Date.now()});
    while(this.seen.size>this.maxEntries) {
      const oldest=this.seen.keys().next().value;
      if(oldest===undefined) break;
      this.seen.delete(oldest);
    }
    return response;
  }
  /** Call for any potentially mutating file tool, even on a failed attempt. */
  invalidate(scope:string):void {
    const prefix=JSON.stringify([scope]).slice(0,-1)+",";
    for(const key of this.seen.keys())if(key.startsWith(prefix))this.seen.delete(key);
  }
}
export const desktopContextExposure=new ContextExposureTracker();
