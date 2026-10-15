import test from 'node:test';
import assert from 'node:assert/strict';
import {parsePosixTreeRss,parseWindowsTreeRss} from './swarmResidentRss.mjs';

test('POSIX process tree RSS includes descendants, excludes unrelated processes',()=>{
  const input = '10 1 300\n11 10 200\n12 11 100\n13 1 20000\n';
  assert.equal(parsePosixTreeRss(input,10),600*1024);
  assert.equal(parsePosixTreeRss(input,11),300*1024);
  assert.equal(parsePosixTreeRss(input,14),null);
  assert.equal(parsePosixTreeRss(input,-1),null);
  assert.equal(parsePosixTreeRss('nonsense',10),null);
});
test('Windows process tree RSS accepts JSON process inventory with parent references',()=>{
  const value=JSON.stringify([
    {ProcessId:101,ParentProcessId:1,WorkingSetSize:2000},
    {ProcessId:102,ParentProcessId:101,WorkingSetSize:3000},
    {ProcessId:103,ParentProcessId:2,WorkingSetSize:999999},
  ]);
  assert.equal(parseWindowsTreeRss(value,101),5000);
  assert.equal(parseWindowsTreeRss(value,999),null);
  assert.equal(parseWindowsTreeRss('{malformed',101),null);
  assert.equal(parseWindowsTreeRss(value,0),null);
});
