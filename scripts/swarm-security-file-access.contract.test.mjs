import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {mkdtempSync,rmSync,writeFileSync,symlinkSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {readProtectedEvidence} from './swarm-dag-network-recovery-live-acceptance.mjs';
import {readReceipt,digestFile} from './swarm-mlx-fused-inference-acceptance.mjs';

function fixture(callback){
 const root=mkdtempSync(join(tmpdir(),'lm-security-nofollow-'));
 try{return callback(root);}finally{rmSync(root,{recursive:true,force:true});}
}
describe('GitHub Advanced Security 278: operator network evidence',()=>{
 it('reads a protected regular file by its own descriptor',()=>fixture(root=>{
  const file=join(root,'evidence.json');
  writeFileSync(file,JSON.stringify({sequence:11,source:'signed'}),{mode:0o600});
  assert.deepEqual(readProtectedEvidence(file),{sequence:11,source:'signed'});
 }));
 it('never follows an attacker-replaced symlink or oversized evidence file',()=>fixture(root=>{
  const file=join(root,'target.json');
  const link=join(root,'replacement.json');
  writeFileSync(file,'{"secret":"should never be read through a symlink"}',{mode:0o600});
  symlinkSync(file,link);
  assert.throws(()=>readProtectedEvidence(link));
  const oversized=join(root,'huge.json');
  writeFileSync(oversized,'x'.repeat(16*1024+1),{mode:0o600});
  assert.throws(()=>readProtectedEvidence(oversized),/bounded/);
 }));
});
describe('GitHub Advanced Security 279: fused MLX file validation',()=>{
 it('reads only the verified receipt file and rejects symlinks',()=>fixture(root=>{
  const receipt=join(root,'training-receipt.json');
  writeFileSync(receipt,'{"status":"validated"}',{mode:0o600});
  assert.deepEqual(readReceipt(receipt),{status:'validated'});
  const link=join(root,'swapped.json');
  symlinkSync(receipt,link);
  assert.throws(()=>readReceipt(link));
  const oversized=join(root,'oversized.json');
  writeFileSync(oversized,'x'.repeat(512*1024+1),{mode:0o600});
  assert.throws(()=>readReceipt(oversized),/bounded/);
 }));
 it('hashes the same opened weight descriptor and fails closed after a path swap',async()=>{
  const root=mkdtempSync(join(tmpdir(),'lm-fused-nofollow-'));
  try{
   const shard=join(root,'model.safetensors');
   const data=Buffer.from('independently evaluated safetensors bytes');
   writeFileSync(shard,data,{mode:0o600});
   const expected=createHash('sha256').update(data).digest('hex');
   assert.deepEqual(await digestFile(shard,1024),{size:data.length,sha256:expected});
   const modified=Buffer.from('completely different model weights');
   writeFileSync(shard,modified,{mode:0o600});
   assert.notEqual((await digestFile(shard,1024)).sha256,expected);
   const link=join(root,'redirected.safetensors');
   symlinkSync(shard,link);
   await assert.rejects(digestFile(link,1024));
  }finally{rmSync(root,{recursive:true,force:true});}
 });
});
