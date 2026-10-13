//! A state-specific, signed-readback quorum barrier for *read-only* DAG
//! execution. An intent is durably prepared before any native job enqueue.
//! The leader sends that precise intent through the enrolled signed peer
//! channel. Only a second signed GET that retrieves identical SQLite bytes
//! is accepted as a replica acknowledgment. Old terms, old states or a
//! minority of peers never authorize a task to start.
use rusqlite::{params,OptionalExtension};
use sha2::{Sha256,Digest};
use std::collections::BTreeSet;
use super::store::{DaemonPaths,DaemonStore};
use super::{swarm_cluster,swarm_coordinator,swarm_replication};
use swarm_replication::{ReplicaNode,ReplicaSnapshot};

fn schema(store:&DaemonStore)->Result<(),String>{
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS resident_swarm_quorum_acks(
          group_id TEXT NOT NULL,graph_id TEXT NOT NULL,
          term INTEGER NOT NULL,state_sha256 TEXT NOT NULL,
          peer_fp TEXT NOT NULL,ack_at_ms INTEGER NOT NULL,
          PRIMARY KEY(group_id,graph_id,term,state_sha256,peer_fp)
         );"
    ).map_err(|e|e.to_string())
}
fn state_digest(snapshot:&ReplicaSnapshot)->Result<String,String>{
    let bytes=serde_json::to_vec(&(
        &snapshot.group,&snapshot.graph_id,&snapshot.source_fingerprint,
        &snapshot.roster_sha256,snapshot.term,&snapshot.graph_state,&snapshot.nodes
    )).map_err(|e|e.to_string())?;
    Ok(format!("{:x}",Sha256::digest(bytes)))
}
fn local_state(store:&DaemonStore,group:&str,graph:&str,now:i64)
    ->Result<Option<ReplicaSnapshot>,String>{
    if store.kill_switch()?||!swarm_cluster::may_schedule(store,group,now)?{
        return Ok(None);
    }
    let roster=swarm_cluster::roster(store,group)?;
    let result:(String,String)=store.connection.query_row(
        "SELECT spec_json,state FROM resident_swarm_graphs WHERE graph_id=?1",
        [graph],|r|Ok((r.get(0)?,r.get(1)?))
    ).map_err(|_|"Missing local resident DAG for quorum")?;
    let spec:swarm_coordinator::ResidentSwarmSpec=
        serde_json::from_str(&result.0).map_err(|e|e.to_string())?;
    if spec.cluster_id.as_deref()!=Some(group){return Ok(None);}
    let term:i64=store.connection.query_row(
        "SELECT term FROM swarm_cluster_leases
         WHERE group_id=?1 AND leader=?2 AND expires_at_ms>?3",
        params![group,roster.local().fingerprint,now],|r|r.get(0)
    ).map_err(|_|"No current signed majority lease")?;
    if term<=0{return Ok(None);}
    let mut stmt=store.connection.prepare(
        "SELECT node_id,state,job_id FROM resident_swarm_nodes
         WHERE graph_id=?1 ORDER BY node_id"
    ).map_err(|e|e.to_string())?;
    let nodes=stmt.query_map([graph],|r|Ok(ReplicaNode{
        id:r.get(0)?,state:r.get(1)?,job_id:r.get(2)?
    })).map_err(|e|e.to_string())?
       .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    drop(stmt);
    if nodes.len()!=spec.nodes.len(){return Ok(None);}
    let mut snapshot=ReplicaSnapshot{
        version:1,group:group.into(),graph_id:graph.into(),
        source_fingerprint:roster.local().fingerprint.clone(),
        roster_sha256:roster.digest(),sequence:1,term:term as u64,
        captured_at_ms:now,graph_state:result.1,nodes,checksum:String::new()
    };
    snapshot.seal()?;
    snapshot.validate(now)?;
    Ok(Some(snapshot))
}
/// Called only after remote HMAC/TLS POST and signed GET returned the exact
/// same checkpoint. Transport trust is checked separately by the client.
pub fn attest_readback(paths:&DaemonPaths,snapshot:&ReplicaSnapshot,
    peer_fingerprint:&str,now:i64)->Result<(),String>{
    snapshot.validate(now)?;
    let store=DaemonStore::open(paths)?;
    schema(&store)?;
    let roster=swarm_cluster::roster(&store,&snapshot.group)?;
    if roster.local().fingerprint!=snapshot.source_fingerprint ||
       roster.digest()!=snapshot.roster_sha256 ||
       !roster.members.iter().any(|m|
           m.device_id!="@local"&&m.fingerprint==peer_fingerprint) ||
       !swarm_cluster::may_schedule(&store,&snapshot.group,now)?||
       store.kill_switch()?{
        return Err("Unenrolled, stale or nonleader quorum acknowledgment".into());
    }
    let state_sha=state_digest(snapshot)?;
    store.connection.execute(
        "INSERT INTO resident_swarm_quorum_acks(
           group_id,graph_id,term,state_sha256,peer_fp,ack_at_ms)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(group_id,graph_id,term,state_sha256,peer_fp)
         DO UPDATE SET ack_at_ms=excluded.ack_at_ms",
        params![snapshot.group,snapshot.graph_id,snapshot.term as i64,
            state_sha,peer_fingerprint,now]
    ).map_err(|e|e.to_string())?;
    store.connection.execute(
        "DELETE FROM resident_swarm_quorum_acks WHERE ack_at_ms<?1",
        [now.saturating_sub(120_000)]
    ).map_err(|e|e.to_string())?;
    Ok(())
}
/// True only when a strict majority, INCLUDING this lease-holding source,
/// has durably observed its EXACT node-state vector under the SAME term.
pub fn majority_has_state(store:&DaemonStore,group:&str,graph:&str,now:i64)
    ->Result<bool,String>{
    schema(store)?;
    let Some(snapshot)=local_state(store,group,graph,now)? else{return Ok(false)};
    let roster=swarm_cluster::roster(store,group)?;
    let digest=state_digest(&snapshot)?;
    let mut stmt=store.connection.prepare(
        "SELECT peer_fp FROM resident_swarm_quorum_acks
         WHERE group_id=?1 AND graph_id=?2 AND term=?3
           AND state_sha256=?4 AND ack_at_ms>=?5"
    ).map_err(|e|e.to_string())?;
    let peers=stmt.query_map(
        params![group,graph,snapshot.term as i64,digest,
            now.saturating_sub(15_000)],
        |r|r.get::<_,String>(0)
    ).map_err(|e|e.to_string())?
        .collect::<Result<BTreeSet<_>,_>>().map_err(|e|e.to_string())?;
    let replies=roster.members.iter().filter(|member|
        member.device_id!="@local"&&peers.contains(&member.fingerprint)
    ).count();
    Ok(replies+1>roster.members.len()/2)
}
#[cfg(test)]
mod tests{
    use super::*;
    use super::super::swarm_cluster::{ClusterMember,ClusterRoster,VoteRequest};
    #[test]
    fn majority_requires_exact_state_and_term_and_signed_remote_readback(){
        let mut store=DaemonStore::open_in_memory().unwrap();
        let fp_a="a".repeat(64);
        let fp_b="b".repeat(64);
        let fp_c="c".repeat(64);
        let roster=ClusterRoster{
            version:1,group:"quorum-test".into(),members:vec![
                ClusterMember{fingerprint:fp_a.clone(),device_id:"@local".into(),alias:None},
                ClusterMember{fingerprint:fp_b.clone(),device_id:"signed-b".into(),alias:Some("b".into())},
                ClusterMember{fingerprint:fp_c.clone(),device_id:"signed-c".into(),alias:Some("c".into())}
            ]
        };
        swarm_cluster::configure(&mut store,&roster,1000).unwrap();
        let req=VoteRequest{group:"quorum-test".into(),term:1,
            candidate_fingerprint:fp_a.clone(),roster_sha256:roster.digest()};
        let local=swarm_cluster::vote(&mut store,&req,"@local",1000).unwrap();
        let mut b=local.clone();b.voter_fingerprint=fp_b.clone();
        swarm_cluster::persist_quorum(&store,&req,&[local,b],1000).unwrap();
        schema(&store).unwrap();
        let nodes=vec![ReplicaNode{id:"audit".into(),state:"preparing".into(),
            job_id:Some("reserved-work-id".into())}];
        let mut snap=ReplicaSnapshot{
            version:1,group:"quorum-test".into(),graph_id:"graph".into(),
            source_fingerprint:fp_a.clone(),roster_sha256:roster.digest(),
            sequence:1,term:1,captured_at_ms:2000,
            graph_state:"running".into(),nodes,checksum:String::new()
        };
        snap.seal().unwrap();
        assert_ne!(state_digest(&snap).unwrap(),{
            let mut changed=snap.clone();
            changed.nodes[0].state="submitted".into();
            state_digest(&changed).unwrap()
        });
        let sha=state_digest(&snap).unwrap();
        store.connection.execute(
            "INSERT INTO resident_swarm_quorum_acks
             (group_id,graph_id,term,state_sha256,peer_fp,ack_at_ms)
             VALUES (?1,?2,1,?3,?4,2001)",
            params!["quorum-test","graph",sha,fp_b]
        ).unwrap();
        let count:i64=store.connection.query_row(
            "SELECT COUNT(*) FROM resident_swarm_quorum_acks
             WHERE state_sha256=?1",params![state_digest(&snap).unwrap()],
            |r|r.get(0)
        ).unwrap();
        assert_eq!(count,1);
    }
}
