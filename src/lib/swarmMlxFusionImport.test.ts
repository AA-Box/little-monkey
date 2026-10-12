import {describe,it,expect} from 'vitest';
import {parseValidatedMlxFusionReceipt,verifyFusedModelConfig} from './swarmMlxFusionImport';
const hash='e'.repeat(64);
const valid={version:1,engine:'mlx_lm',
 status:'validated_fused_model_requires_manual_runtime_import',
 fusedModelPath:'/tmp/trained/fused-model',fusedConfigSha256:hash,
 testPerplexity:4,fusedTestPerplexity:4.1,maxTestPpl:5,
 regression:{previousPpl:6,newPpl:6.1,maxFraction:0.05},
 fusedRegressionPpl:6.2};
describe('operator-approved MLX fused model promotion gate',()=>{
 it('permits only fully evaluated, absolute, non-traversing paths',()=>{
  expect(parseValidatedMlxFusionReceipt(valid)).toMatchObject({
   fusedModelPath:valid.fusedModelPath,fusedConfigSha256:hash});
  for(const bad of [
   {...valid,status:'candidate_unreviewed'},
   {...valid,fusedModelPath:'../outside'},
   {...valid,fusedModelPath:'/tmp/../secrets'},
   {...valid,fusedConfigSha256:'not-a-hash'},
   {...valid,fusedTestPerplexity:5.8},
   {...valid,testPerplexity:3.7},
   {...valid,regression:{...valid.regression,maxFraction:0.1,newPpl:9}},
   {...valid,fusedRegressionPpl:12},
  ])expect(()=>parseValidatedMlxFusionReceipt(bad)).toThrow();
 });
 it('compares the actual local config bytes to the receipt hash',async()=>{
  const mock={digest:async()=>new Uint8Array(32).fill(0xee).buffer};
  await expect(verifyFusedModelConfig('{"model":"good"}',hash,mock))
    .resolves.toBeUndefined();
  await expect(verifyFusedModelConfig('{}','0'.repeat(64),mock))
    .rejects.toThrow(/differs/);
 });
});
