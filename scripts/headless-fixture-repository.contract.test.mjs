import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {initHeadlessFixtureRepository} from './headless-fixture-repository.mjs';

test('real temporary fixture is a clean committed Git repository accepted by autonomous task startup',()=>{
 const root=mkdtempSync(join(tmpdir(),'little-monkey-native-git-fixture-'));
 try{
  mkdirSync(join(root,'src'));
  writeFileSync(join(root,'src','fixture.txt'),'MARKER_FROM_ACTUAL_FIXTURE\n');
  initHeadlessFixtureRepository(root);
  const git=(...args)=>{
   const out=spawnSync('git',['-C',root,...args],{
    encoding:'utf8',timeout:15000,maxBuffer:1024*1024,
   });
   assert.equal(out.status,0,out.stderr);
   return out.stdout.trim();
  };
  assert.match(git('rev-parse','HEAD'),/^[0-9a-f]{40}$/);
  assert.equal(git('status','--porcelain'),'');
  assert.equal(git('show','HEAD:src/fixture.txt'),'MARKER_FROM_ACTUAL_FIXTURE');
  // Read-only challenge has a committed HEAD even without user's ~/.gitconfig.
  assert.equal(git('log','-1','--format=%an'),'Little Monkey Benchmark');
 }finally{rmSync(root,{recursive:true,force:true});}
});
