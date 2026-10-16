import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync,writeFileSync,chmodSync,readlinkSync,readFileSync,rmSync,existsSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join,resolve,basename} from 'node:path';

const script=resolve('scripts/selfdev-transaction.mjs');
function call(...args){
  const result=spawnSync(process.execPath,[script,...args],{
    encoding:'utf8',timeout:25000,maxBuffer:256000,
  });
  let body;
  try{body=JSON.parse(result.stdout);}catch{}
  return{ok:result.status===0,stdout:result.stdout,stderr:result.stderr,body};
}
function fixture(root,name,version){
  const path=join(root,name);
  writeFileSync(path,'#!/bin/sh\nif [ "$1" = "--version" ]; then echo "Little Monkey CLI '+version+
    '"; exit 0; fi\nexit 5\n'+'# harmless local CLI test binary\n'.repeat(5),{
      mode:0o700,
    });
  chmodSync(path,0o700);
  return path;
}
test('real OS executable symlinks: stage, two activations, rollback and digest refusal',{
  skip:process.platform==='win32'?'Symlink atomic rename uses POSIX contract':false,
},()=>{
  const temp=mkdtempSync(join(tmpdir(),'little-monkey-selfdev-'));
  const root=join(temp,'manager');
  try{
    const a=fixture(temp,'candidate-a','0.1.0');
    const b=fixture(temp,'candidate-b','0.2.0');
    const bad=join(temp,'malicious-file');
    writeFileSync(bad,'not a binary');
    let r=call('stage','--candidate',bad,'--root',root);
    assert.equal(r.ok,false);

    r=call('stage','--candidate',a,'--root',root);
    assert.equal(r.ok,true,r.stderr);
    const shaA=r.body.sha;
    assert.match(shaA,/^[a-f0-9]{64}$/);
    assert.equal(existsSync(join(root,'current')),false);

    r=call('activate','--root',root,'--confirm-sha','f'.repeat(64));
    assert.equal(r.ok,false,'Activation must not accept an unstaged digest');

    r=call('activate','--root',root,'--confirm-sha',shaA);
    assert.equal(r.ok,true,r.stderr);
    assert.equal(basename(readlinkSync(join(root,'current'))),shaA);
    assert.equal(call('status','--root',root).body.active,shaA);

    r=call('stage','--candidate',b,'--root',root);
    assert.equal(r.ok,true,r.stderr);
    const shaB=r.body.sha;
    assert.notEqual(shaB,shaA);
    r=call('activate','--root',root,'--confirm-sha',shaB);
    assert.equal(r.ok,true,r.stderr);
    assert.equal(basename(readlinkSync(join(root,'current'))),shaB);
    assert.equal(basename(readlinkSync(join(root,'previous'))),shaA);

    r=call('rollback','--root',root);
    assert.equal(r.ok,false,'Rollback needs deliberate --confirm-rollback');
    r=call('rollback','--root',root,'--confirm-rollback');
    assert.equal(r.ok,true,r.stderr);
    assert.equal(basename(readlinkSync(join(root,'current'))),shaA);
    assert.equal(basename(readlinkSync(join(root,'previous'))),shaB);

    // Manually corrupt the staged artifact to prove activation checks bytes,
    // not only metadata or the name of the SHA-pinned directory.
    writeFileSync(join(root,'versions',shaB,'monkey-cli'),
      '#!/bin/sh\necho "Little Monkey CLI corrupted"\n'+
       '# corrupted artifact cannot pass SHA-256 verification\n'.repeat(5));
    chmodSync(join(root,'versions',shaB,'monkey-cli'),0o700);
    r=call('activate','--root',root,'--confirm-sha',shaB);
    assert.equal(r.ok,false);
    assert.equal(basename(readlinkSync(join(root,'current'))),shaA);
  }finally{rmSync(temp,{recursive:true,force:true});}
});
