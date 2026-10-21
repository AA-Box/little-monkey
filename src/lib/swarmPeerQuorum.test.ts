import {describe,it,expect,vi} from 'vitest';
const peersListMock=vi.fn();
vi.mock('./peersClient',()=>({peersList:(...args:unknown[])=>peersListMock(...args)}));
import {authenticatedPeerVoters,revalidateAuthenticatedPeerVoters} from './swarmPeerQuorum';
const identity=(alias:string,index:number)=>({
 alias,peer_id:'peer-'+index,certificate_sha256:String(index).repeat(64),
 grants:['task'],presence:'online' as const,
});
describe('authenticated signed-peer voter identity gate',()=>{
 it('requires distinct certificate-pinned device identities even across alternate aliases',()=>{
  const list=[identity('a',1),identity('b',2),identity('c',3)] as never;
  expect(authenticatedPeerVoters(['a','b','c'],list)).toHaveLength(3);
  expect(()=>authenticatedPeerVoters(['a','b'],[
    identity('a',1),{...identity('b',2),peer_id:'peer-1'},
  ] as never)).toThrow(/not distinct/);
  expect(()=>authenticatedPeerVoters(['a','b'],[
    identity('a',1),{...identity('b',2),certificate_sha256:'1'.repeat(64)},
  ] as never)).toThrow(/not distinct/);
  expect(()=>authenticatedPeerVoters(['a','a'],list)).toThrow(/distinct paired/);
 });
 it('rejects missing task grants, offline identities and forged fingerprints',()=>{
  for(const item of [
    {...identity('b',2),grants:[]},
    {...identity('b',2),presence:'offline'},
    {...identity('b',2),certificate_sha256:'x'.repeat(64)},
  ]){
    expect(()=>authenticatedPeerVoters(['a','b'],[identity('a',1),item] as never)).toThrow();
  }
 });
 it('detects certificate change after dispatch',async()=>{
  const original=authenticatedPeerVoters(['a','b'],[identity('a',1),identity('b',2)] as never);
  peersListMock.mockResolvedValueOnce({outbound:[identity('a',1),identity('b',2)]});
  await expect(revalidateAuthenticatedPeerVoters(original)).resolves.toBeUndefined();
  peersListMock.mockResolvedValueOnce({outbound:[identity('a',1),{...identity('b',2),certificate_sha256:'e'.repeat(64)}]});
  await expect(revalidateAuthenticatedPeerVoters(original)).rejects.toThrow(/identity changed/);
 });
});
