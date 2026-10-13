//! Reconcile a predecessor's latest signed state on an intersecting majority
//! before any unattended takeover. A stale local "pending" copy is NOT proof
//! that the prior leader did not durably prepare work at another peer.
//!
//! The old leader can only dispatch after an independently verified majority
//! has durably stored its PREPARING intent. A new leader must therefore read
//! back the source's latest state from a fresh majority (including self).
//! Quorum intersection ensures that a previously committed intent cannot be
//! hidden by partitioning or choosing a stale replica. Missing witnesses fail
//! closed, and any previously attempted work blocks unattended reassignment.
use futures_util::{stream::FuturesUnordered,StreamExt};
use rusqlite::{params,OptionalExtension};
use super::store::{DaemonPaths,DaemonStore};
use super::{swarm_cluster,swarm_replication};
use super::swarm_replication::ReplicaSnapshot;
use std::collections::BTreeSet;

const WITNESS_TTL_MS:i64=12_000;
fn schema(store:&DaemonStore)->Result<(),String>{
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS resident_swarm_takeover_witnesses(
          group_id TEXT NOT NULL,source_graph TEXT NOT NULL,
          source_fp TEXT NOT NULL,leader_term INTEGER NOT NULL,
          peer_fp TEXT NOT NULL,source_json TEXT,observed_at_ms INTEGER NOT NULL,
          PRIMARY KEY(group_id,source_graph,source_fp,leader_term,peer_fp)
         );"
    ).map_err(|e|e.to_string())
}
fn all_pending(snapshot:&ReplicaSnapshot)->bool{
    snapshot.nodes.iter().all(|node|
        node.state=="pending"&&node.job_id.is_none()
    )
}
/// Record only a separately signed, certificate-pinned peer readback.
/// Called after validating the response's group, graph and source identity.
pub fn record(
    paths:&DaemonPaths,group:&str,graph:&str,source_fp:&str,
    peer_fp:&str,snapshot:Option<&ReplicaSnapshot>,now:i64
)->Result<(),String>{
    let store=DaemonStore::open(paths)?;
    schema(&store)?;
    let roster=swarm_cluster::roster(&store,group)?;
    if store.kill_switch()?||!swarm_cluster::may_schedule(&store,group,now)?||
       !roster.members.iter().any(|m|m.device_id!="@local"&&m.fingerprint==peer_fp)||
       !roster.members.iter().any(|m|m.device_id!="@local"&&m.fingerprint==source_fp)
    {return Err("Recovery witness is not part of the current signed majority leader roster".into());}
    let term:i64=store.connection.query_row(
        "SELECT term FROM swarm_cluster_leases
         WHERE group_id=?1 AND leader=?2 AND expires_at_ms>?3",
        params![group,roster.local().fingerprint,now],|r|r.get(0)
    ).map_err(|_|"Recovery requires a current signed leadership term")?;
    if term<=0{return Err("Invalid current leader term".into());}
    if let Some(snapshot)=snapshot{
        snapshot.validate(now)?;
        if snapshot.group!=group||snapshot.graph_id!=graph||
           snapshot.source_fingerprint!=source_fp ||
           snapshot.roster_sha256!=roster.digest()||
           snapshot.term>=term as u64{
            return Err("Predecessor's witness graph, identity, roster or term is invalid".into());
        }
    }
    // Witness state must be monotonic even when a paired peer retries a GET
    // out of order. An already observed PREPARING state cannot be replaced by
    // an older PENDING copy or a manufactured "missing source" response.
    let previous:Option<Option<String>>=store.connection.query_row(
        "SELECT source_json FROM resident_swarm_takeover_witnesses
         WHERE group_id=?1 AND source_graph=?2 AND source_fp=?3
           AND leader_term=?4 AND peer_fp=?5",
        params![group,graph,source_fp,term,peer_fp],|r|r.get(0)
    ).optional().map_err(|e|e.to_string())?;
    if let Some(Some(previous))=previous {
        let old:ReplicaSnapshot=serde_json::from_str(&previous)
            .map_err(|e|e.to_string())?;
        let Some(new)=snapshot else{
            return Err("Previously observed source checkpoint cannot disappear".into());
        };
        if new.term<old.term||new.sequence<old.sequence||
            (new.sequence==old.sequence&&new.checksum!=old.checksum){
            return Err("Rollback or conflict in signed recovery witness readback".into());
        }
    }
    let serialized=snapshot.map(serde_json::to_string).transpose()
        .map_err(|e|e.to_string())?;
    store.connection.execute(
        "INSERT INTO resident_swarm_takeover_witnesses
         (group_id,source_graph,source_fp,leader_term,peer_fp,source_json,observed_at_ms)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(group_id,source_graph,source_fp,leader_term,peer_fp)
         DO UPDATE SET source_json=excluded.source_json,
           observed_at_ms=excluded.observed_at_ms",
        params![group,graph,source_fp,term,peer_fp,serialized,now]
    ).map_err(|e|e.to_string())?;
    store.connection.execute(
        "DELETE FROM resident_swarm_takeover_witnesses WHERE observed_at_ms<?1",
        [now.saturating_sub(120_000)]
    ).map_err(|e|e.to_string())?;
    Ok(())
}
/// Requires a fresh majority with no witness reporting started work. An
/// offline member may not be silently counted as a successful readback.
pub fn majority_confirms_no_dispatch(store:&DaemonStore,group:&str,graph:&str,
    source_fp:&str,local:&ReplicaSnapshot,now:i64)->Result<bool,String>{
    schema(store)?;
    let roster=swarm_cluster::roster(store,group)?;
    if roster.local().fingerprint==source_fp||!all_pending(local)||
       local.source_fingerprint!=source_fp||local.graph_id!=graph||
       local.group!=group||local.roster_sha256!=roster.digest() ||
       !swarm_cluster::may_schedule(store,group,now)?{
        return Ok(false);
    }
    let term:i64=store.connection.query_row(
        "SELECT term FROM swarm_cluster_leases
         WHERE group_id=?1 AND leader=?2 AND expires_at_ms>?3",
        params![group,roster.local().fingerprint,now],|r|r.get(0)
    ).map_err(|_|"No local signed majority term for recovery")?;
    if term<=0||local.term>=term as u64{return Ok(false);}
    let mut stmt=store.connection.prepare(
        "SELECT peer_fp,source_json FROM resident_swarm_takeover_witnesses
         WHERE group_id=?1 AND source_graph=?2 AND source_fp=?3 AND
           leader_term=?4 AND observed_at_ms>=?5"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map(
        params![group,graph,source_fp,term,now.saturating_sub(WITNESS_TTL_MS)],
        |r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))
    ).map_err(|e|e.to_string())?
       .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    let mut seen=BTreeSet::new();
    for (fp,raw) in rows{
        if !roster.members.iter().any(|m|m.device_id!="@local"&&m.fingerprint==fp) ||
            !seen.insert(fp) {continue;}
        if let Some(raw)=raw{
            let observation:ReplicaSnapshot=serde_json::from_str(&raw)
                .map_err(|e|e.to_string())?;
            observation.validate(now)?;
            if observation.source_fingerprint!=source_fp||
               observation.group!=group||observation.graph_id!=graph||
               observation.roster_sha256!=local.roster_sha256||
               observation.term>=term as u64||!all_pending(&observation){
                return Ok(false);
            }
        }
    }
    // One vote is local; every additional vote is a fresh, signed TLS GET
    // of the remote peer's own durable predecessor replica store.
    Ok(seen.len()+1>roster.members.len()/2)
}
/// Collect only for operator-armed, locally leader-owned read-only DAGs.
/// Remote responses carry observations, NEVER runnable recipes or grants.
pub async fn refresh(paths:&DaemonPaths,now:i64)->Result<(),String>{
    // No failover table exists on an installation until this feature is
    // configured. Bootstrap it without granting permissions or scheduling.
    let _=super::swarm_failover::status(paths)?;
    let candidates:Vec<(String,String,String,Vec<(String,String)>)>={
        let store=DaemonStore::open(paths)?;
        schema(&store)?;
        if store.kill_switch()?{return Ok(());}
        let mut stmt=store.connection.prepare(
            "SELECT group_id,source_graph,source_fingerprint
             FROM resident_swarm_failover_plans
             WHERE state='armed' AND source_fingerprint IS NOT NULL
             ORDER BY created_at_ms LIMIT 4"
        ).map_err(|e|e.to_string())?;
        let plans=stmt.query_map([],|r|Ok((
            r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?
        ))).map_err(|e|e.to_string())?
            .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
        let mut result=Vec::new();
        for (group,graph,source) in plans{
            if !swarm_cluster::may_schedule(&store,&group,now).unwrap_or(false){continue;}
            let Ok(roster)=swarm_cluster::roster(&store,&group) else{continue;};
            let peers=roster.members.iter().filter_map(|m|
                m.alias.as_ref().map(|alias|(alias.clone(),m.fingerprint.clone()))
            ).collect::<Vec<_>>();
            result.push((group,graph,source,peers));
        }
        result
    };
    for (group,graph,source_fp,peers) in candidates{
        let futures=FuturesUnordered::new();
        for (alias,fp) in peers{
            let group=group.clone();let graph=graph.clone();
            let paths=paths.clone();
            futures.push(async move{
                let result=tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    super::remote::signed_swarm_checkpoint_readback(
                        &paths,&alias,&fp,&group,&graph,now as u64
                    )
                ).await;
                (fp,result)
            });
        }
        let mut futures=futures;
        while let Some((peer_fp,response))=futures.next().await{
            let Ok(Ok(value))=response else{continue;};
            if value["group"]!=group||value["graph"]!=graph{continue;}
            let Some(replicas)=value["replicas"].as_array() else{continue;};
            let matching=replicas.iter().filter(|row|
                row["source_fingerprint"]==source_fp).collect::<Vec<_>>();
            if matching.len()>1{continue;}
            let snapshot=matching.first().and_then(|value|
                serde_json::from_value::<ReplicaSnapshot>((*value).clone()).ok()
            );
            if matching.len()==1&&snapshot.is_none(){continue;}
            // Do not accept a remote peer's arbitrary explanation of its
            // state: require valid source checksum, enrolled roster, and
            // monotonic identity. A peer may be legitimately missing a copy.
            if record(paths,&group,&graph,&source_fp,&peer_fp,
                snapshot.as_ref(),now).is_err(){continue;}
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn a_prepared_execution_is_never_authorized_as_a_pending_takeover(){
        let mut pending=ReplicaSnapshot{
            version:1,group:"g".into(),graph_id:"dag".into(),
            source_fingerprint:"a".repeat(64),roster_sha256:"b".repeat(64),
            sequence:1,term:1,captured_at_ms:1000,graph_state:"running".into(),
            nodes:vec![super::super::swarm_replication::ReplicaNode{
                id:"node".into(),state:"pending".into(),job_id:None
            }],trace_events:vec![],checksum:String::new()
        };
        pending.seal().unwrap();
        assert!(all_pending(&pending));
        pending.nodes[0].state="preparing".into();
        pending.nodes[0].job_id=Some("reserved-job".into());
        pending.seal().unwrap();
        assert!(!all_pending(&pending));
    }
}
