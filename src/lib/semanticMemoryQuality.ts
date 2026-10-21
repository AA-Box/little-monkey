/**
 * Local-only real-world adjudication counters for semantic memory review.
 * Counts come from real operator decisions, never invented model fixtures.
 * No fact text, IDs, workspaces, raw prompts, provider secrets or dates
 * are stored. Approval fraction measures human agreement, NOT objective
 * semantic accuracy or recall.
 */
export type ReviewQualityKind='proposed'|'approved'|'dismissed';
export type ReviewQualityCounts={proposed:number;approved:number;dismissed:number};
const STORAGE_KEY='little-monkey-semantic-review-quality-v1';
const empty=():ReviewQualityCounts=>({proposed:0,approved:0,dismissed:0});
export function readSemanticReviewQuality():ReviewQualityCounts{
 try{
  const parsed=JSON.parse(localStorage.getItem(STORAGE_KEY)??'null');
  if(!parsed||typeof parsed!=='object')return empty();
  const read=(key:keyof ReviewQualityCounts)=>{
   const n=parsed[key];
   return Number.isSafeInteger(n)&&n>=0&&n<100_000_000?n:0;
  };
  return {proposed:read('proposed'),approved:read('approved'),dismissed:read('dismissed')};
 }catch{return empty();}
}
export function recordSemanticReviewQuality(kind:ReviewQualityKind,count=1):ReviewQualityCounts{
 if(!Number.isSafeInteger(count)||count<1||count>24)
  throw Error('Quality event count must be 1-24');
 const next=readSemanticReviewQuality();
 next[kind]=Math.min(99_999_999,next[kind]+count);
 try{localStorage.setItem(STORAGE_KEY,JSON.stringify(next));}
 catch{/* Optional local metrics must never block the actual memory operation. */}
 return next;
}
export function humanAgreementFraction(data:ReviewQualityCounts):number|null{
 const decided=data.approved+data.dismissed;
 return decided===0?null:data.approved/decided;
}
