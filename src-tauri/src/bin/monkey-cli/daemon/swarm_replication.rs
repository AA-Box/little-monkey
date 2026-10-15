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
use little_monkey_lib::run_ledger::{ChainVerification, RunLedger};
use little_monkey_lib::run_protocol::RunEvent;

const MAX_GRAPH_NODES: usize = 16;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const MAX_AGE_MS: i64 = 5 * 60 * 1000;
const MAX_STORED_SNAPSHOTS: i64 = 1024;
const MAX_TRACE_EVENTS: usize = 48;

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReplicaNode {
    pub id: String,
    pub state: String,
    pub job_id: Option<String>,
}
/// Redacted durable DAG transition, captured on its original host. This is
/// execution *state telemetry* and never a graded model outcome.
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReplicaTraceEvent {
    pub sequence: u64,
    pub node_id: Option<String>,
    pub kind: String,
    pub state: String,
    pub occurred_at_ms: i64,
}
/// Aggregate metered inference usage from a completed, hash-chain-verified
/// original-host run. This is NOT a provider invoice or answer correctness.
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReplicaModelUsage {
    pub node_id: String,
    pub status: String,
    pub target_id: String,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub model_calls: u32,
    pub cost_micros: Option<u64>,
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
    #[serde(default, skip_serializing_if="Vec::is_empty")]
    pub trace_events: Vec<ReplicaTraceEvent>,
    #[serde(default, skip_serializing_if="Vec::is_empty")]
    pub model_usage: Vec<ReplicaModelUsage>,
    pub checksum: String,
}
fn hash(snapshot:&ReplicaSnapshot)->Result<String,String>{
    // v1 checksum is byte-for-byte compatible with existing peers. v2 seals
    // the trace too; an attacker cannot swap event timestamps or states while
    // retaining the same authenticated, read-back-verified payload.
    let bytes=if snapshot.version==1{
        serde_json::to_vec(&(
            snapshot.version,&snapshot.group,&snapshot.graph_id,
            &snapshot.source_fingerprint,&snapshot.roster_sha256,
            snapshot.sequence,snapshot.term,snapshot.captured_at_ms,
            &snapshot.graph_state,&snapshot.nodes,
        ))
    }else if snapshot.version==2{
        serde_json::to_vec(&(
            snapshot.version,&snapshot.group,&snapshot.graph_id,
            &snapshot.source_fingerprint,&snapshot.roster_sha256,
            snapshot.sequence,snapshot.term,snapshot.captured_at_ms,
            &snapshot.graph_state,&snapshot.nodes,&snapshot.trace_events,
        ))
    }else{
        serde_json::to_vec(&(
            snapshot.version,&snapshot.group,&snapshot.graph_id,
            &snapshot.source_fingerprint,&snapshot.roster_sha256,
            snapshot.sequence,snapshot.term,snapshot.captured_at_ms,
            &snapshot.graph_state,&snapshot.nodes,&snapshot.trace_events,
            &snapshot.model_usage,
        ))
    }.map_err(|e|e.to_string())?;
    Ok(format!("{:x}",Sha256::digest(&bytes)))
}
fn valid_id(s:&str)->bool{
    (1..=80).contains(&s.len()) &&
        s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) &&
        s.bytes().all(|c|c.is_ascii_alphanumeric()||matches!(c,b'-'|b'_'|b'.'))
}
fn valid_state(s:&str)->bool{
    matches!(s,"running"|"failing"|"cancelling"|"succeeded"|"failed"|"cancelled"|
        "pending"|"pending_fallback"|"preparing"|"preparing_fallback"|"submitted"|"submitted_fallback")
}
impl ReplicaSnapshot{
    /// Seal exactly the immutable values carried in this checkpoint. This is
    /// corruption detection, NOT an authentication signature; TLS/HMAC and
    /// the enrolled peer identity authenticate the transport separately.
    pub fn seal(&mut self)->Result<(),String>{
        self.checksum=hash(self)?;
        Ok(())
    }
    pub fn validate(&self,now:i64)->Result<(),String>{
        if !matches!(self.version,1|2|3) || !valid_id(&self.group)||!valid_id(&self.graph_id)||
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
        if (self.version==1 && !self.trace_events.is_empty())||
            self.trace_events.len()>MAX_TRACE_EVENTS{
            return Err("Unsupported or excessive distributed trace events".into());
        }
        if (self.version<3 && !self.model_usage.is_empty())||
           self.model_usage.len()>MAX_GRAPH_NODES{
            return Err("Unsupported or excessive original-host inference telemetry".into());
        }
        let mut usage_nodes=std::collections::BTreeSet::new();
        for usage in &self.model_usage{
            if !unique.contains(usage.node_id.as_str())||
                !self.nodes.iter().any(|node|node.id==usage.node_id && node.state==usage.status)||
                !matches!(usage.status.as_str(),"succeeded"|"failed")||
                !usage_nodes.insert(usage.node_id.as_str())||
                usage.target_id.is_empty()||usage.target_id.len()>160||
                !usage.target_id.bytes().all(|c|
                    c.is_ascii_alphanumeric()||matches!(c,b'-'|b'_'|b'.'|b':'|b'/'|b'@'|b'+'|b'%'))||
                usage.elapsed_ms>3_600_000*24||
                usage.input_tokens>100_000_000||usage.output_tokens>100_000_000||
                usage.cached_input_tokens>usage.input_tokens||
                usage.model_calls==0||usage.model_calls>100_000||
                usage.cost_micros.is_some_and(|cost|cost>1_000_000_000_000){
                return Err("Malformed or unrelated metered inference telemetry".into());
            }
        }
        let mut last=0;
        for event in &self.trace_events{
            if event.sequence<=last||event.sequence>i64::MAX as u64||
                event.occurred_at_ms<=0||
                event.occurred_at_ms>self.captured_at_ms.saturating_add(5_000)||
                event.node_id.as_deref().is_some_and(|id|!unique.contains(id))||
                !valid_state(&event.state)||
                !matches!(event.kind.as_str(),"graph_created"|"graph_transition"|
                    "node_created"|"node_transition"){
                return Err("Invalid signed remote trace event".into());
            }
            last=event.sequence;
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
    list_local(store,group,graph_id)
}
/// Local operator-only view; the signed network route uses list_for_member.
pub fn list_local(
    store:&DaemonStore,group:&str,graph_id:&str,
)->Result<Vec<ReplicaSnapshot>,String>{
    if !valid_id(group)||!valid_id(graph_id){
        return Err("Invalid cluster or graph identity".into());
    }
    swarm_cluster::roster(store,group)?;
    schema(store)?;
    let mut stmt=store.connection.prepare(
        "SELECT snapshot_json FROM resident_swarm_peer_checkpoints
         WHERE group_id=?1 AND graph_id=?2 ORDER BY source_fp LIMIT 7"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![group,graph_id],|r|r.get::<_,String>(0))
        .map_err(|e|e.to_string())?
        .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    rows.into_iter().map(|raw|
        serde_json::from_str(&raw).map_err(|e|e.to_string())
    ).collect()
}
/// Extract only independently hash-chain-verified metered usage from completed
/// local daemon runs. Never trust a job status or model-produced success claim.
fn measured_usage(
    paths:&super::store::DaemonPaths,store:&DaemonStore,nodes:&[ReplicaNode],
)->Vec<ReplicaModelUsage>{
    let Ok(ledger)=RunLedger::open(&paths.ledger_db) else{return vec![]};
    let mut rows=Vec::new();
    for node in nodes {
        if !matches!(node.state.as_str(),"succeeded"|"failed") {continue;}
        let Some(job)=node.job_id.as_deref()
            .and_then(|id|store.get_job(id).ok().flatten()) else {continue};
        let (Some(run_id),Some(start),Some(end))=(
            job.run_id.as_deref(),job.started_at_ms,job.finished_at_ms
        ) else{continue};
        if end<start||end-start>86_400_000{continue;}
        let Ok(Some(run))=ledger.load_run(run_id) else{continue};
        let status=match run.status{
            little_monkey_lib::run_protocol::RunStatus::Succeeded=>"succeeded",
            little_monkey_lib::run_protocol::RunStatus::Failed=>"failed",
            _=>continue,
        };
        if status!=node.state||run.last_sequence==0||run.last_sequence>4000 {continue;}
        if !matches!(ledger.verify_run_chain(run_id),
            Ok(ChainVerification::Intact{
                covered_from:Some(1),covered_through:Some(last),events_seen,..
            }) if last==run.last_sequence && events_seen==last) {continue;}
        // Completed contains canonical terminal usage. A failed run instead
        // records cumulative usage in its last UsageRecorded event, if any.
        // Both are read from the verified event chain; never fabricate tokens
        // for a failed call that provided no metering.
        let mut cursor=0;
        let mut metering=None;
        let mut terminal=None;
        while cursor<run.last_sequence {
            let Ok(events)=ledger.load_events(run_id,cursor,200) else{break};
            if events.is_empty(){break;}
            for envelope in events {
                cursor=envelope.sequence;
                match envelope.event {
                    RunEvent::UsageRecorded{usage}=>metering=Some(usage),
                    RunEvent::Completed{usage,..}=>{
                        metering=Some(usage);terminal=Some("succeeded");
                    },
                    RunEvent::Failed{..}=>terminal=Some("failed"),
                    _=>{},
                }
            }
        }
        if cursor!=run.last_sequence||terminal!=Some(status){continue;}
        let Some(usage)=metering else{continue};
        if usage.model_calls==0{continue};
        let target=run.spec.target.target_id();
        if target.is_empty()||target.len()>160||
            !target.bytes().all(|c|c.is_ascii_alphanumeric()||
                matches!(c,b'-'|b'_'|b'.'|b':'|b'/'|b'@'|b'+'|b'%')) {continue;}
        rows.push(ReplicaModelUsage{
            node_id:node.id.clone(),status:status.into(),target_id:target.to_owned(),
            elapsed_ms:end-start,input_tokens:usage.input_tokens,
            output_tokens:usage.output_tokens,
            cached_input_tokens:usage.cached_input_tokens,
            model_calls:usage.model_calls,cost_micros:usage.cost_micros,
        });
    }
    rows
}
/// Capture an actual locally authorized leader's DAG state for one
/// operator-selected peer. Neither prompts nor credentials nor model outputs
/// are included. Outbound sequence allocation is durable and transactional:
/// a crashed uploader cannot reuse an earlier sequence with different bytes.
pub fn capture_local_leader(
    paths:&super::store::DaemonPaths,group:&str,graph:&str,alias:&str,
    expected_peer_sha256:&str,now:i64,
)->Result<ReplicaSnapshot,String>{
    if !valid_id(group)||!valid_id(graph)||alias.is_empty()||now<=0{
        return Err("Invalid swarm replication selection".into());
    }
    let mut store=DaemonStore::open(paths)?;
    let members=swarm_cluster::roster(&store,group)?;
    let peer=members.members.iter().find(|m|m.alias.as_deref()==Some(alias))
        .ok_or("Replication target must be an operator-enrolled outbound alias")?;
    if peer.fingerprint!=expected_peer_sha256 {
        return Err("Replication peer certificate mismatches enrolled roster".into());
    }
    if store.kill_switch()?||
        !super::swarm_coordinator::owns_current_epoch(&store,group,graph,now)?{
        return Err("DAG publisher is not the original majority-term owner".into());
    }
    let (raw,state):(String,String)=store.connection.query_row(
        "SELECT spec_json,state FROM resident_swarm_graphs WHERE graph_id=?1",
        [graph],|r|Ok((r.get(0)?,r.get(1)?))
    ).map_err(|_|"The resident swarm graph is not locally registered")?;
    let spec:super::swarm_coordinator::ResidentSwarmSpec=
        serde_json::from_str(&raw).map_err(|e|e.to_string())?;
    if spec.cluster_id.as_deref()!=Some(group){
        return Err("Graph is not assigned to this operator-enrolled cluster".into());
    }
    let mut stmt=store.connection.prepare(
        "SELECT node_id,state,job_id FROM resident_swarm_nodes
         WHERE graph_id=?1 ORDER BY node_id"
    ).map_err(|e|e.to_string())?;
    let nodes=stmt.query_map([graph],|r|Ok(ReplicaNode{
        id:r.get(0)?,state:r.get(1)?,job_id:r.get(2)?,
    })).map_err(|e|e.to_string())?
        .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    drop(stmt);
    if nodes.len()!=spec.nodes.len(){
        return Err("Local DAG node ledger is incomplete".into());
    }
    let term:i64=store.connection.query_row(
        "SELECT term FROM swarm_cluster_leases
         WHERE group_id=?1 AND leader=?2 AND expires_at_ms>?3",
        params![group,members.local().fingerprint,now],|r|r.get(0)
    ).map_err(|_|"Signed majority lease expired before DAG snapshot")?;
    schema(&store)?;
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS resident_swarm_checkpoint_outbox(
           group_id TEXT NOT NULL,graph_id TEXT NOT NULL,
           sequence INTEGER NOT NULL,
           PRIMARY KEY(group_id,graph_id)
         );"
    ).map_err(|e|e.to_string())?;
    let tx=store.connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e|e.to_string())?;
    let last:Option<i64>=tx.query_row(
        "SELECT sequence FROM resident_swarm_checkpoint_outbox
         WHERE group_id=?1 AND graph_id=?2",
        params![group,graph],|r|r.get(0)
    ).optional().map_err(|e|e.to_string())?;
    let next=last.unwrap_or(0).checked_add(1)
        .filter(|v|*v>0).ok_or("DAG checkpoint sequence exhausted")?;
    tx.execute(
        "INSERT INTO resident_swarm_checkpoint_outbox(group_id,graph_id,sequence)
         VALUES(?1,?2,?3) ON CONFLICT(group_id,graph_id) DO UPDATE SET
         sequence=excluded.sequence",
        params![group,graph,next]
    ).map_err(|e|e.to_string())?;
    tx.commit().map_err(|e|e.to_string())?;
    // The original author's persistent event ledger is the only trace source.
    // Never serialize messages, recipes, reports or arbitrary model text.
    let mut events_stmt=store.connection.prepare(
        "SELECT event_id,node_id,event_kind,next_state,occurred_at_ms
         FROM resident_swarm_events WHERE graph_id=?1
         ORDER BY event_id DESC LIMIT ?2"
    ).map_err(|e|e.to_string())?;
    let mut trace_events=events_stmt.query_map(params![graph,MAX_TRACE_EVENTS as i64],|r|{
        let id:i64=r.get(0)?;
        Ok(ReplicaTraceEvent{
            sequence:u64::try_from(id).unwrap_or(0),
            node_id:r.get(1)?,kind:r.get(2)?,
            state:r.get(3)?,occurred_at_ms:r.get(4)?,
        })
    }).map_err(|e|e.to_string())?
      .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    trace_events.reverse();
    drop(events_stmt);
    let model_usage=measured_usage(paths,&store,&nodes);
    let mut snapshot=ReplicaSnapshot{
        version:if model_usage.is_empty(){2}else{3},group:group.to_owned(),graph_id:graph.to_owned(),
        source_fingerprint:members.local().fingerprint.clone(),
        roster_sha256:members.digest(),
        sequence:next as u64,
        term:u64::try_from(term).map_err(|_|"Invalid persisted election term")?,
        captured_at_ms:now,graph_state:state,nodes,trace_events,model_usage,
        checksum:String::new(),
    };
    snapshot.checksum=hash(&snapshot)?;
    snapshot.validate(now)?;
    Ok(snapshot)
}

/// Periodic best-effort publication of actual locally leader-owned DAG
/// checkpoints. Every destination is explicitly enrolled in the local
/// cluster's pinned certificate roster and already granted PeerTaskRequest
/// on the receiver. This loop grants nothing, cannot schedule foreign jobs
/// and never applies any remote status as a trusted local completion.
pub fn spawn_replica_worker(paths:super::store::DaemonPaths){
    tokio::spawn(async move {
        let mut timer=tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            timer.tick().await;
            let now=match super::now_ms().and_then(|ms|
                i64::try_from(ms).map_err(|_|"Replica clock overflow".to_string())
            ){Ok(now)=>now,Err(error)=>{
                eprintln!("Swarm replica clock unavailable: {error}");
                continue;
            }};
            let selected:Vec<(String,String,String,String)>={
                let store=match DaemonStore::open(&paths){
                    Ok(s)=>s,Err(error)=>{
                        eprintln!("Swarm replica database unavailable: {error}");
                        continue;
                    }
                };
                if store.kill_switch().unwrap_or(true) {
                    Vec::new()
                }else{
                    // Do not launch unbounded network work from a heartbeat.
                    // All of these are operator-prestaged cluster graphs.
                    let graphs=store.connection.prepare(
                        "SELECT graph_id,spec_json FROM resident_swarm_graphs
                         WHERE state IN ('running','succeeded','failed')
                         ORDER BY updated_at_ms DESC LIMIT 4"
                    ).and_then(|mut stmt|stmt.query_map([],|r|
                        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))
                    ).and_then(|rows|rows.collect::<Result<Vec<_>,_>>()))
                        .unwrap_or_default();
                    let mut peers=Vec::new();
                    for (id,raw) in graphs {
                        let Ok(spec)=serde_json::from_str::<super::swarm_coordinator::ResidentSwarmSpec>(&raw)
                            else {continue};
                        let Some(group)=spec.cluster_id else{continue};
                        if !swarm_cluster::may_schedule(&store,&group,now).unwrap_or(false){continue;}
                        let Ok(roster)=swarm_cluster::roster(&store,&group) else{continue};
                        for member in roster.members {
                            if let Some(alias)=member.alias {
                                peers.push((group.clone(),id.clone(),alias,member.fingerprint));
                                if peers.len()>=12{break;}
                            }
                        }
                        if peers.len()>=12{break;}
                    }
                    peers
                }
            };
            for (group,graph,alias,fp) in selected {
                let snapshot=match capture_local_leader(
                    &paths,&group,&graph,&alias,&fp,now
                ){Ok(snapshot)=>snapshot,Err(_)=>continue};
                let Ok(bytes)=serde_json::to_vec(&snapshot) else{continue};
                let reply=tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    super::remote::signed_swarm_checkpoint(
                        &paths,&alias,&fp,bytes,now as u64
                    )
                ).await;
                if !matches!(reply,Ok(Ok(ref answer)) if
                    answer["accepted"]==true &&
                    answer["sequence"]==snapshot.sequence &&
                    answer["graph_id"]==graph &&
                    answer["group"]==group) {
                    eprintln!("Swarm replica checkpoint deferred for {group}/{graph} on {alias}");
                    continue;
                }
                // A POST acceptance alone cannot prove the receiver durably
                // committed this state. Read the same receiver over independently
                // signed pinned TLS GET and compare the ENTIRE snapshot.
                let readback=tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    super::remote::signed_swarm_checkpoint_readback(
                        &paths,&alias,&fp,&group,&graph,now as u64
                    )
                ).await;
                let Ok(Ok(observed))=readback else{continue;};
                let Ok(expected)=serde_json::to_value(&snapshot) else{continue;};
                if observed["group"]!=group||observed["graph"]!=graph||
                    !observed["replicas"].as_array().is_some_and(|rows|
                        rows.iter().any(|row|row==&expected)) {
                    eprintln!("Swarm replica readback differed for {group}/{graph} on {alias}");
                    continue;
                }
                // A remote readback that matches the exact state/term earns
                // one durable vote toward the PRE-DISPATCH state barrier.
                // It never counts for newer states or terms.
                if let Err(error)=super::swarm_quorum::attest_readback(
                    &paths,&snapshot,&fp,now
                ){
                    eprintln!("Swarm replica quorum record deferred: {error}");
                }
            }
        }
    });
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
            trace_events:vec![],
            model_usage:vec![],
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
        let resident_tables:i64=store.connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='table' AND name='resident_swarm_graphs'",[],|r|r.get(0)
        ).unwrap();
        assert_eq!(resident_tables,0,
            "a remote success assertion must never create a local graph");
    }
    #[test]
    fn v2_remote_journal_is_authenticated_bounded_and_not_execution_authority(){
        let (mut store,mut cp,device)=configured();
        cp.version=2;
        cp.trace_events=vec![
            ReplicaTraceEvent{sequence:10,node_id:None,
                kind:"graph_created".into(),state:"running".into(),occurred_at_ms:900},
            ReplicaTraceEvent{sequence:12,node_id:Some("audit".into()),
                kind:"node_transition".into(),state:"submitted".into(),occurred_at_ms:1000},
        ];
        cp.seal().unwrap();
        assert!(ingest(&mut store,&device,&cp,1100).unwrap());
        let persisted=list_for_member(&store,&device,"team","audit").unwrap();
        assert_eq!(persisted[0].trace_events,cp.trace_events);
        let mut modified=cp.clone();
        modified.trace_events[1].state="succeeded".into();
        assert!(ingest(&mut store,&device,&modified,1100).is_err(),
            "modified remote event without a new checksum");
        modified.seal().unwrap();
        assert!(ingest(&mut store,&device,&modified,1100).is_err(),
            "conflicting same-sequence remote trace");
        modified.sequence=2;modified.seal().unwrap();
        assert!(ingest(&mut store,&device,&modified,1200).unwrap(),
            "a fresh signed revision may report new state observations");
        let mut backward=modified.clone();
        backward.sequence=3;
        backward.trace_events.swap(0,1);
        backward.seal().unwrap();
        assert!(ingest(&mut store,&device,&backward,1200).is_err());
        let mut forged=modified.clone();
        forged.sequence=3;
        forged.trace_events[0].node_id=Some("unknown-node".into());
        forged.seal().unwrap();
        assert!(ingest(&mut store,&device,&forged,1200).is_err());
        let mut too_many=modified.clone();
        too_many.sequence=3;
        too_many.trace_events=(1..=49).map(|n|ReplicaTraceEvent{
            sequence:n,node_id:Some("audit".into()),
            kind:"node_transition".into(),state:"submitted".into(),occurred_at_ms:1000,
        }).collect();
        too_many.seal().unwrap();
        assert!(ingest(&mut store,&device,&too_many,1200).is_err());
        // Existing peers still accept and persist exact legacy v1 checkpoints.
        let (mut other,legacy,device)=configured();
        assert!(ingest(&mut other,&device,&legacy,1100).unwrap());
        assert!(list_for_member(&other,&device,"team","audit").unwrap()[0].trace_events.is_empty());
        let has_graph:i64=store.connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='table' AND name='resident_swarm_graphs'",
            [],|r|r.get(0)
        ).unwrap();
        assert_eq!(has_graph,0,"peer telemetry cannot authorize a resident graph");
    }
    #[test]
    fn v3_source_metering_is_checksums_bound_bounded_and_readback_durable(){
        let (mut store,mut cp,device)=configured();
        cp.version=3;
        cp.nodes[0].state="succeeded".into();
        cp.model_usage=vec![ReplicaModelUsage{
            node_id:"audit".into(),status:"succeeded".into(),target_id:"provider:model".into(),
            elapsed_ms:1230,input_tokens:120,output_tokens:30,
            cached_input_tokens:10,model_calls:2,cost_micros:Some(450),
        }];
        cp.seal().unwrap();
        assert!(ingest(&mut store,&device,&cp,1100).unwrap());
        assert_eq!(list_for_member(&store,&device,"team","audit").unwrap()[0].model_usage,
            cp.model_usage);
        let mut forged=cp.clone();
        forged.model_usage[0].cost_micros=Some(999_999);
        assert!(ingest(&mut store,&device,&forged,1100).is_err(),
            "changed original-host metered amount must invalidate checksum");
        forged.seal().unwrap();
        assert!(ingest(&mut store,&device,&forged,1100).is_err(),
            "conflicting same-sequence model usage must not replace durable receipt");
        for bad in [
            ReplicaModelUsage{node_id:"absent".into(),..cp.model_usage[0].clone()},
            ReplicaModelUsage{cached_input_tokens:121,..cp.model_usage[0].clone()},
            ReplicaModelUsage{model_calls:0,..cp.model_usage[0].clone()},
            ReplicaModelUsage{target_id:"secret query".into(),..cp.model_usage[0].clone()},
        ] {
            let mut modified=cp.clone();
            modified.sequence=2;modified.model_usage=vec![bad];
            modified.seal().unwrap();
            assert!(ingest(&mut store,&device,&modified,1200).is_err());
        }
        let mut legacy=cp.clone();
        legacy.version=2;legacy.sequence=2;legacy.seal().unwrap();
        assert!(ingest(&mut store,&device,&legacy,1200).is_err(),
            "an older schema cannot impersonate source-measured run usage");
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
