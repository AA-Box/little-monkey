//! Signed-peer replicated resident swarm STATUS checkpoints.
//!
//! A checkpoint is an authenticated *observation*, never an executable job,
//! a trusted model report, an operator approval or a quorum certificate.
//! Pairing+PeerTaskRequest AND explicit local cluster roster membership are
//! required; an enrolled source identity cannot be impersonated via editable
//! aliases. Snapshots advance monotonically per source/graph and persist in
//! the daemon's own SQLite state. A different leader must explicitly import
//! and re-authorize a frozen plan-only graph before it can schedule anything.
//! This deliberately refuses unsafe transparent migration of file edits.
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest,Sha256};
use super::store::DaemonStore;
use super::swarm_cluster;

const MAX_GRAPH_NODES: usize = 16;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const MAX_AGE_MS: i64 = 5 * 60 * 1000;
const MAX_STORED_SNAPSHOTS: i64 = 1024;

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReplicaNode {
    pub id: String,
    pub state: String,
    pub job_id: Option<String>,
}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaSnapshot {
    pub version: u32,
    pub group: String,
    pub graph_id: String,
    pub source_fingerprint: String,
    pub roster_sha256: String,
    pub sequence: u64,
    pub term: u64,
    pub captured_at_ms: i64,
    pub graph_state: String,
    pub nodes: Vec<ReplicaNode>,
    pub checksum: String,
}
fn hash(snapshot:&ReplicaSnapshot)->Result<String,String>{
    let bytes=serde_json::to_vec(&(
        snapshot.version,&snapshot.group,&snapshot.graph_id,
        &snapshot.source_fingerprint,&snapshot.roster_sha256,
        snapshot.sequence,snapshot.term,snapshot.captured_at_ms,
        &snapshot.graph_state,&snapshot.nodes,
    )).map_err(|e|e.to_string())?;
    Ok(format!("{:x}",Sha256::digest(&bytes)))
}
fn valid_id(s:&str)->bool{
    (1..=80).contains(&s.len()) &&
        s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) &&
        s.bytes().all(|c|c.is_ascii_alphanumeric()||matches!(c,b'-'|b'_'|b'.'))
}
fn valid_state(s:&str)->bool{
    matches!(s,"running"|"failing"|"cancelling"|"succeeded"|"failed"|"cancelled"|
        "pending"|"pending_fallback"|"submitted"|"submitted_fallback")
}
impl ReplicaSnapshot{
    pub fn validate(&self,now:i64)->Result<(),String>{
        if self.version!=1 || !valid_id(&self.group)||!valid_id(&self.graph_id)||
            !self.source_fingerprint.bytes().all(|c|c.is_ascii_hexdigit()&&!c.is_ascii_uppercase())||
            self.source_fingerprint.len()!=64||
            self.roster_sha256.len()!=64 ||
            !self.roster_sha256.bytes().all(|c|c.is_ascii_hexdigit()&&!c.is_ascii_uppercase())||
            self.sequence==0||self.sequence>i64::MAX as u64||
            self.term==0||self.term>1_000_000_000||
            self.captured_at_ms<=0||self.captured_at_ms>now.saturating_add(5_000)||
            now.saturating_sub(self.captured_at_ms)>MAX_AGE_MS||
            !valid_state(&self.graph_state)||self.nodes.is_empty()||
            self.nodes.len()>MAX_GRAPH_NODES||
            self.checksum!=hash(self)?{
            return Err("Invalid, stale or incorrectly checksummed signed DAG checkpoint".into());
        }
        let mut unique=std::collections::BTreeSet::new();
        for node in &self.nodes{
            if !valid_id(&node.id)||!valid_state(&node.state)||
                !unique.insert(node.id.as_str())||
                node.job_id.as_ref().is_some_and(|x|
                    x.len()>120||x.len()<4||!x.bytes().all(|c|
                        c.is_ascii_alphanumeric()||matches!(c,b'-'|b'_'|b'.'))) {
                return Err("Malformed or duplicate DAG checkpoint task".into());
            }
        }
        Ok(())
    }
}
fn schema(store:&DaemonStore)->Result<(),String>{
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS resident_swarm_peer_checkpoints(
          group_id TEXT NOT NULL,
          graph_id TEXT NOT NULL,
          source_fp TEXT NOT NULL,
          sequence INTEGER NOT NULL,
          term INTEGER NOT NULL,
          checksum TEXT NOT NULL,
          snapshot_json TEXT NOT NULL,
          updated_at_ms INTEGER NOT NULL,
          PRIMARY KEY(group_id,graph_id,source_fp)
        );
        CREATE INDEX IF NOT EXISTS idx_swarm_peer_checkpoints_updated
          ON resident_swarm_peer_checkpoints(updated_at_ms);"
    ).map_err(|e|e.to_string())
}
/// Authenticated receiver: the signed calling device must match its own
/// immutable enrolled peer fingerprint. No ACL is inferred from aliases.
pub fn ingest(
    store:&mut DaemonStore,source_device:&str,snapshot:&ReplicaSnapshot,now:i64,
)->Result<bool,String>{
    snapshot.validate(now)?;
    let roster=swarm_cluster::roster(store,&snapshot.group)?;
    if roster.digest()!=snapshot.roster_sha256||
        !roster.members.iter().any(|m|
            m.device_id==source_device &&
            m.fingerprint==snapshot.source_fingerprint
        ){
        return Err("Signed checkpoint publisher is not an enrolled cluster member".into());
    }
    if store.kill_switch()? {
        return Err("Global kill switch prevents DAG checkpoint writes".into());
    }
    schema(store)?;
    let raw=serde_json::to_string(snapshot).map_err(|e|e.to_string())?;
    if raw.len()>MAX_CHECKPOINT_BYTES {
        return Err("DAG checkpoint exceeds 64 KiB".into());
    }
    let tx=store.connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e|e.to_string())?;
    let old:Option<(i64,i64,String)>=tx.query_row(
        "SELECT sequence,term,checksum FROM resident_swarm_peer_checkpoints
         WHERE group_id=?1 AND graph_id=?2 AND source_fp=?3",
        params![snapshot.group,snapshot.graph_id,snapshot.source_fingerprint],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional().map_err(|e|e.to_string())?;
    if let Some((sequence,term,checksum))=old{
        if snapshot.term<term as u64 || snapshot.sequence<sequence as u64 {
            return Err("Stale DAG checkpoint term or sequence".into());
        }
        if snapshot.sequence==sequence as u64 {
            if checksum==snapshot.checksum && snapshot.term==term as u64{
                return Ok(false);
            }
            return Err("Conflicting DAG checkpoint at the same sequence".into());
        }
    }
    tx.execute(
        "INSERT INTO resident_swarm_peer_checkpoints(
          group_id,graph_id,source_fp,sequence,term,checksum,snapshot_json,updated_at_ms)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(group_id,graph_id,source_fp) DO UPDATE SET
          sequence=excluded.sequence,term=excluded.term,checksum=excluded.checksum,
          snapshot_json=excluded.snapshot_json,updated_at_ms=excluded.updated_at_ms",
        params![snapshot.group,snapshot.graph_id,snapshot.source_fingerprint,
            snapshot.sequence as i64,snapshot.term as i64,snapshot.checksum,raw,now],
    ).map_err(|e|e.to_string())?;
    // Bounds physical growth even for many unique graph ids. The latest
    // checkpoint of each source survives until it is among oldest.
    tx.execute(
        "DELETE FROM resident_swarm_peer_checkpoints WHERE rowid IN (
            SELECT rowid FROM resident_swarm_peer_checkpoints
            ORDER BY updated_at_ms DESC, rowid DESC LIMIT -1 OFFSET ?1
         )",[MAX_STORED_SNAPSHOTS]
    ).map_err(|e|e.to_string())?;
    tx.commit().map_err(|e|e.to_string())?;
    Ok(true)
}
pub fn list_for_member(
    store:&DaemonStore,device_id:&str,group:&str,graph_id:&str,
)->Result<Vec<ReplicaSnapshot>,String>{
    if !valid_id(group)||!valid_id(graph_id){
        return Err("Invalid cluster or graph identity".into());
    }
    let roster=swarm_cluster::roster(store,group)?;
    if !roster.members.iter().any(|m|m.device_id==device_id) {
        return Err("Requester is not enrolled in this cluster".into());
    }
    schema(store)?;
    let mut stmt=store.connection.prepare(
        "SELECT snapshot_json FROM resident_swarm_peer_checkpoints
         WHERE group_id=?1 AND graph_id=?2 ORDER BY source_fp LIMIT 7"
    ).map_err(|e|e.to_string())?;
    stmt.query_map(params![group,graph_id],|r|r.get::<_,String>(0))
        .map_err(|e|e.to_string())?
        .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?
        .into_iter().map(|raw|
            serde_json::from_str(&raw).map_err(|e|e.to_string())
        ).collect()
}
#[cfg(test)]
mod tests{
    use super::*;
    use super::swarm_cluster::{ClusterMember,ClusterRoster};
    fn configured()->(DaemonStore,ReplicaSnapshot,String){
        let mut store=DaemonStore::open_in_memory().unwrap();
        let peer_fp="b".repeat(64);
        let roster=ClusterRoster{version:1,group:"team".into(),members:vec![
            ClusterMember{fingerprint:"a".repeat(64),device_id:"@local".into(),alias:None},
            ClusterMember{fingerprint:peer_fp.clone(),device_id:"signed-peer".into(),alias:Some("outbound".into())},
        ]};
        swarm_cluster::configure(&mut store,&roster,1000).unwrap();
        let mut snapshot=ReplicaSnapshot{
            version:1,group:"team".into(),graph_id:"audit".into(),
            source_fingerprint:peer_fp,roster_sha256:roster.digest(),
            sequence:1,term:1,captured_at_ms:1000,
            graph_state:"running".into(),
            nodes:vec![ReplicaNode{id:"audit".into(),state:"submitted".into(),job_id:Some("job-original".into())}],
            checksum:String::new(),
        };
        snapshot.checksum=hash(&snapshot).unwrap();
        (store,snapshot,"signed-peer".into())
    }
    #[test]
    fn signed_pinned_publisher_is_monotonic_idempotent_and_not_an_authority_to_run(){
        let (mut store,mut cp,device)=configured();
        assert!(ingest(&mut store,&device,&cp,1100).unwrap());
        assert!(!ingest(&mut store,&device,&cp,1100).unwrap());
        assert!(ingest(&mut store,"different-device",&cp,1100).is_err());
        let mut conflicting=cp.clone();
        conflicting.nodes[0].state="succeeded".into();
        conflicting.checksum=hash(&conflicting).unwrap();
        assert!(ingest(&mut store,&device,&conflicting,1100).is_err());
        cp.sequence=2;cp.nodes[0].state="succeeded".into();
        cp.checksum=hash(&cp).unwrap();
        assert!(ingest(&mut store,&device,&cp,1200).unwrap());
        assert_eq!(list_for_member(&store,&device,"team","audit").unwrap()[0].sequence,2);
        assert!(list_for_member(&store,"alien","team","audit").is_err());
        assert!(ingest(&mut store,&device,&conflicting,1200).is_err());
        // The replicated observation never creates local daemon runs/graphs.
        let jobs:i64=store.connection.query_row(
            "SELECT COUNT(*) FROM daemon_jobs",[],|r|r.get(0)
        ).unwrap_or(0);
        assert_eq!(jobs,0);
    }
    #[test]
    fn altered_timestamp_term_checksum_or_huge_state_fails_closed(){
        let (mut store,cp,device)=configured();
        let mut t=cp.clone();
        t.captured_at_ms=800_000;
        t.checksum=hash(&t).unwrap();
        assert!(ingest(&mut store,&device,&t,1100).is_err());
        let mut t=cp.clone();t.term=0;t.checksum=hash(&t).unwrap();
        assert!(ingest(&mut store,&device,&t,1100).is_err());
        let mut t=cp.clone();t.checksum="0".repeat(64);
        assert!(ingest(&mut store,&device,&t,1100).is_err());
        let mut t=cp.clone();t.nodes[0].job_id=Some("x".repeat(500));
        t.checksum=hash(&t).unwrap();
        assert!(ingest(&mut store,&device,&t,1100).is_err());
    }
}
