/** The local MLX adapter trainer writes a signed-by-data, not cryptographically
 * signed-by-author, evaluation receipt. This parser is NOT attestation: the
 * operator must select it manually, the UI checks the actual config bytes, and
 * Rust independently validates external model formats before registration.
 */
export interface FusedMlxWeightDigest {name:string;sha256:string;sizeBytes:number;}
export interface ValidatedFusedMlxReceipt {
 fusedModelPath:string;
 fusedConfigSha256:string;
 fusedWeights:FusedMlxWeightDigest[];
 testPerplexity:number;
 fusedTestPerplexity:number;
}
const digestRe=/^[a-f0-9]{64}$/;
const positive=(value:unknown):value is number=>typeof value==='number'&&Number.isFinite(value)&&value>0;
export function parseValidatedMlxFusionReceipt(raw:unknown):ValidatedFusedMlxReceipt {
 if(!raw||typeof raw!=='object'||Array.isArray(raw))throw Error('Expected local MLX training JSON receipt.');
 const input=raw as Record<string,unknown>;
 const path=input.fusedModelPath;
 if(input.version!==1||input.engine!=='mlx_lm'||
    input.status!=='validated_fused_model_requires_manual_runtime_import'||
    typeof path!=='string'||path.length>1024||
    !(path.startsWith('/')||/^[a-zA-Z]:[\\/]/.test(path))||
    /[\x00-\x1f]/.test(path)||
    path.split(/[\\/]/).some(x=>x==='..')||
    typeof input.fusedConfigSha256!=='string'||!digestRe.test(input.fusedConfigSha256)||
    !positive(input.testPerplexity)||!positive(input.fusedTestPerplexity)||
    !positive(input.maxTestPpl)||input.fusedTestPerplexity>input.maxTestPpl||
    input.fusedTestPerplexity>input.testPerplexity*1.05){
  throw Error('Not a passing, operator-reviewed MLX fused-model training receipt.');
 }
 const weights=input.fusedWeights;
 if(!Array.isArray(weights)||weights.length<1||weights.length>128)
   throw Error('Missing complete fused model weight manifest.');
 const shards:FusedMlxWeightDigest[]=[];
 for(const entry of weights){
  if(!entry||typeof entry!=='object'||Array.isArray(entry))throw Error('Invalid fused weight evidence.');
  const row=entry as Record<string,unknown>;
  if(typeof row.name!=='string'||row.name.length>128||
    !/^[a-zA-Z0-9_.-]+\.safetensors$/.test(row.name)||
    typeof row.sha256!=='string'||!digestRe.test(row.sha256)||
    typeof row.sizeBytes!=='number'||!Number.isSafeInteger(row.sizeBytes)||
    row.sizeBytes<1||row.sizeBytes>128*1024*1024*1024||
    shards.some(x=>x.name===row.name))throw Error('Invalid or duplicate fused model shard.');
  shards.push({name:row.name,sha256:row.sha256,sizeBytes:row.sizeBytes});
 }
 const regression=input.regression;
 if(regression!=null){
  if(typeof regression!=='object'||Array.isArray(regression))throw Error('Invalid regression proof.');
  const row=regression as Record<string,unknown>;
  if(!positive(row.previousPpl)||!positive(row.newPpl)||
     typeof row.maxFraction!=='number'||!Number.isFinite(row.maxFraction)||
     row.maxFraction<0||row.maxFraction>0.25||
     row.newPpl>row.previousPpl*(1+row.maxFraction)||
     !positive(input.fusedRegressionPpl)||
     input.fusedRegressionPpl>row.previousPpl*(1+row.maxFraction)){
   throw Error('Fused-model historical regression gate did not pass.');
  }
 }
 return {fusedModelPath:path,fusedConfigSha256:input.fusedConfigSha256,
   fusedWeights:shards,testPerplexity:input.testPerplexity,fusedTestPerplexity:input.fusedTestPerplexity};
}
export async function verifyFusedModelConfig(
 configText:string,expected:string,
 subtle:Pick<SubtleCrypto,'digest'>=crypto.subtle,
):Promise<void>{
 if(!digestRe.test(expected))throw Error('Invalid expected fused model digest.');
 const bytes=new TextEncoder().encode(configText);
 if(bytes.length>1024*1024)throw Error('Fused model config exceeds 1 MiB.');
 const sha=new Uint8Array(await subtle.digest('SHA-256',bytes));
 const actual=Array.from(sha,x=>x.toString(16).padStart(2,'0')).join('');
 if(actual!==expected)throw Error('Fused model config differs from trained/evaluated artifact.');
}
