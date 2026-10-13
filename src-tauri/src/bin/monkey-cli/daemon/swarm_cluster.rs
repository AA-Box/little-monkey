//! Signed, pinned-peer quorum leases for read-only resident swarm scheduling.
//!
//! The operator explicitly configures the same member fingerprints at each
//! installation, mapping inbound authenticated pairing device ids to their
//! fingerprint. The roster digest excludes LOCAL pairing ids, which naturally
//! differ per destination. Votes are durable and exclusive until lease expiry;
//! a candidate must hold a strict majority of actual signed TLS responses.
//! No unauthenticated agent text, model reports or gossip can elect a leader.
//!
//! This fences NEW local read-only DAG dispatch, not an arbitrary side effect
//! already submitted under an older term. Clustered recipes are plan-only.
//! It is not a replicated log, BFT agreement or a state transfer protocol.
use rusqlite::{params,OptionalExtension,TransactionBehavior};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use std::collections::BTreeSet;
use super::store::{DaemonPaths,DaemonStore};
const LEASE_MS:i64=20_000;
const SAFETY_MS:i64=5_000;
// Prevent a compromised but task-granted paired node from persisting a
// terminal i64::MAX epoch and permanently exhausting the election counter.
const MAX_ELECTION_TERM:u64=1_000_000_000;
fn valid_id(value:&str)->bool{
    (1..=80).contains(&value.len())&&
    value.bytes().all(|c| c.is_ascii_alphanumeric()||matches!(c,b'-'|b'_'|b'.'))
}
fn valid_fp(value:&str)->bool{
    value.len()==64&&value.bytes().all(|c|c.is_ascii_hexdigit()&&!c.is_ascii_uppercase())
}
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterMember {
    pub fingerprint:String,
    /// @local for self; otherwise the authenticated *inbound* device id.
    pub device_id:String,
    /// An already paired *outbound* alias. Never a URL or arbitrary host.
    pub alias:Option<String>,
}
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterRoster {
    pub version:u32,
    pub group:String,
    pub members:Vec<ClusterMember>,
}
impl ClusterRoster {
    pub fn check(&self)->Result<(),String>{
        if self.version!=1||!valid_id(&self.group)||!(2..=7).contains(&self.members.len()){
            return Err("Cluster requires version 1, valid group and 2–7 explicit members".into());
        }
        let mut ids=BTreeSet::new();
        let mut fps=BTreeSet::new();
        let mut aliases=BTreeSet::new();
        for m in &self.members{
            if !valid_fp(&m.fingerprint)||m.device_id.is_empty()||
                m.device_id.len()>200||!ids.insert(m.device_id.as_str())||
                !fps.insert(m.fingerprint.as_str())||
                (m.device_id=="@local")!=m.alias.is_none()||
                m.alias.as_deref().is_some_and(|x|!valid_id(x)||!aliases.insert(x)){
                return Err("Duplicate, malformed or incomplete cluster membership".into());
            }
        }
        if self.members.iter().filter(|x|x.device_id=="@local").count()!=1{
            return Err("Exactly one locally owned cluster member required".into());
        }
        Ok(())
    }
    pub fn local(&self)->&ClusterMember{
        self.members.iter().find(|x|x.device_id=="@local").expect("validated cluster")
    }
    pub fn digest(&self)->String{
        let mut fps=self.members.iter().map(|m|m.fingerprint.as_str()).collect::<Vec<_>>();
        fps.sort_unstable();
        format!("{:x}",Sha256::digest(fps.join("\n").as_bytes()))
    }
}
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteRequest {
    pub group:String,
    pub term:u64,
    pub candidate_fingerprint:String,
    pub roster_sha256:String,
}
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteReceipt {
    pub group:String,
    pub term:u64,
    pub candidate_fingerprint:String,
    pub voter_fingerprint:String,
    pub roster_sha256:String,
    pub expires_at_ms:i64,
    pub decided_at_ms:i64,
}
fn schema(store:&DaemonStore)->Result<(),String>{
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS swarm_cluster_rosters (
           group_id TEXT PRIMARY KEY, roster_json TEXT NOT NULL,
           configured_at_ms INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS swarm_cluster_votes (
           group_id TEXT PRIMARY KEY, term INTEGER NOT NULL,
           candidate TEXT NOT NULL, expires_at_ms INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS swarm_cluster_leases (
           group_id TEXT PRIMARY KEY, term INTEGER NOT NULL,
           leader TEXT NOT NULL, expires_at_ms INTEGER NOT NULL,
           quorum_json TEXT NOT NULL
         );"
    ).map_err(|e|e.to_string())
}
pub fn configure(store:&mut DaemonStore,roster:&ClusterRoster,now:i64)->Result<(),String>{
    roster.check()?;
    if now<=0{return Err("Invalid local election clock".into());}
    schema(store)?;
    let active=store.connection.query_row(
        "SELECT expires_at_ms FROM swarm_cluster_votes WHERE group_id=?1",
        [&roster.group],|r|r.get::<_,i64>(0)
    ).optional().map_err(|e|e.to_string())?.is_some_and(|expiry|expiry>now);
    if active{return Err("Cannot reconfigure membership during an active election lease".into());}
    let json=serde_json::to_string(roster).map_err(|e|e.to_string())?;
    store.connection.execute(
        "INSERT INTO swarm_cluster_rosters(group_id,roster_json,configured_at_ms)
         VALUES (?1,?2,?3) ON CONFLICT(group_id) DO UPDATE SET
         roster_json=excluded.roster_json, configured_at_ms=excluded.configured_at_ms",
        params![roster.group,json,now]
    ).map_err(|e|e.to_string())?;
    store.connection.execute(
        "DELETE FROM swarm_cluster_leases WHERE group_id=?1",[&roster.group]
    ).map_err(|e|e.to_string())?;
    // The operator may reset an EXPIRED or exhausted election only through
    // explicit local roster enrollment. Active votes were rejected above.
    store.connection.execute(
        "DELETE FROM swarm_cluster_votes WHERE group_id=?1",[&roster.group]
    ).map_err(|e|e.to_string())?;
    Ok(())
}
pub fn roster(store:&DaemonStore,group:&str)->Result<ClusterRoster,String>{
    schema(store)?;
    let raw:String=store.connection.query_row(
        "SELECT roster_json FROM swarm_cluster_rosters WHERE group_id=?1",
        [group],|r|r.get(0)).map_err(|_|"Cluster must be enrolled by the local operator")?;
    let result:ClusterRoster=serde_json::from_str(&raw).map_err(|e|e.to_string())?;
    result.check()?;
    Ok(result)
}
pub fn vote(store:&mut DaemonStore,req:&VoteRequest,caller_device_id:&str,now:i64)
    ->Result<VoteReceipt,String>{
    if now<=0||req.term==0||req.term>MAX_ELECTION_TERM||!valid_fp(&req.candidate_fingerprint)
        {return Err("Invalid election term, identity or clock".into());}
    let registered=roster(store,&req.group)?;
    if registered.digest()!=req.roster_sha256{return Err("Cluster roster differs between nodes".into());}
    if !registered.members.iter().any(|m|m.device_id==caller_device_id&&
            m.fingerprint==req.candidate_fingerprint){
        return Err("Signed peer identity is not enrolled as this candidate".into());
    }
    let transaction=store.connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e|e.to_string())?;
    let existing=transaction.query_row(
        "SELECT term,candidate,expires_at_ms FROM swarm_cluster_votes WHERE group_id=?1",
        [&req.group],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?))
    ).optional().map_err(|e|e.to_string())?;
    if let Some((term,candidate,expiry))=existing{
        if (req.term as i64) < term ||
           (req.term as i64==term&&candidate!=req.candidate_fingerprint) ||
           (req.term as i64>term&&expiry>now){
            return Err("Voter has a newer term or an overlapping lease for another candidate".into());
        }
    }
    let expires=now.checked_add(LEASE_MS).ok_or("Lease timestamp overflow")?;
    transaction.execute(
        "INSERT INTO swarm_cluster_votes(group_id,term,candidate,expires_at_ms)
         VALUES (?1,?2,?3,?4) ON CONFLICT(group_id) DO UPDATE SET
         term=excluded.term,candidate=excluded.candidate,expires_at_ms=excluded.expires_at_ms",
        params![req.group,req.term as i64,req.candidate_fingerprint,expires]
    ).map_err(|e|e.to_string())?;
    transaction.commit().map_err(|e|e.to_string())?;
    Ok(VoteReceipt{group:req.group.clone(),term:req.term,
        candidate_fingerprint:req.candidate_fingerprint.clone(),
        voter_fingerprint:registered.local().fingerprint.clone(),
        roster_sha256:req.roster_sha256.clone(),
        expires_at_ms:expires,decided_at_ms:now})
}
pub fn next_term(store:&DaemonStore,group:&str)->Result<u64,String>{
    schema(store)?;
    let current=store.connection.query_row(
        "SELECT term FROM swarm_cluster_votes WHERE group_id=?1",
        [group],|r|r.get::<_,i64>(0)
    ).optional().map_err(|e|e.to_string())?.unwrap_or(0);
    let next=u64::try_from(current).map_err(|_|"Invalid saved election term".to_string())?
        .checked_add(1).ok_or_else(||"Election term overflow".to_string())?;
    if next>MAX_ELECTION_TERM {
        return Err("Election term exhausted; explicitly re-enroll the roster after lease expiry".into());
    }
    Ok(next)
}
pub fn persist_quorum(store:&DaemonStore,req:&VoteRequest,votes:&[VoteReceipt],
    now:i64)->Result<i64,String>{
    let members=roster(store,&req.group)?;
    let fingerprints=members.members.iter().map(|m|m.fingerprint.as_str())
        .collect::<BTreeSet<_>>();
    let voters=votes.iter().map(|v|v.voter_fingerprint.as_str()).collect::<BTreeSet<_>>();
    if now<=0 || req.term==0 || req.term>MAX_ELECTION_TERM ||
       voters.len()<=members.members.len()/2 || voters.len()!=votes.len() ||
       !voters.is_subset(&fingerprints) ||
       votes.iter().any(|v|v.group!=req.group||v.term!=req.term||
          v.candidate_fingerprint!=req.candidate_fingerprint||
          v.roster_sha256!=req.roster_sha256||
          v.decided_at_ms<now.saturating_sub(5_000)||
          v.decided_at_ms>now.saturating_add(5_000)||
          v.expires_at_ms.checked_sub(v.decided_at_ms)!=Some(LEASE_MS)){
        return Err("Signed peer replies did not establish a fresh strict majority".into());
    }
    let expires=votes.iter().map(|v|v.expires_at_ms).min().unwrap_or(0)
        .checked_sub(SAFETY_MS).ok_or("Election lease timestamp underflow")?;
    if expires<=now+2_000{return Err("Quorum lease expired before it could be accepted".into());}
    let votes_json=serde_json::to_string(votes).map_err(|e|e.to_string())?;
    store.connection.execute(
        "INSERT INTO swarm_cluster_leases(group_id,term,leader,expires_at_ms,quorum_json)
         VALUES (?1,?2,?3,?4,?5) ON CONFLICT(group_id) DO UPDATE SET
         term=excluded.term,leader=excluded.leader,
         expires_at_ms=excluded.expires_at_ms,quorum_json=excluded.quorum_json",
        params![req.group,req.term as i64,req.candidate_fingerprint,expires,votes_json]
    ).map_err(|e|e.to_string())?;
    Ok(expires)
}
pub fn may_schedule(store:&DaemonStore,group:&str,now:i64)->Result<bool,String>{
    let roster=roster(store,group)?;
    let local=roster.local().fingerprint.as_str();
    let current=store.connection.query_row(
        "SELECT leader,expires_at_ms FROM swarm_cluster_leases WHERE group_id=?1",
        [group],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))
    ).optional().map_err(|e|e.to_string())?;
    Ok(current.is_some_and(|(leader,expires)|leader==local&&expires>now))
}
pub fn expire_leader(store:&DaemonStore,group:&str)->Result<(),String>{
    schema(store)?;
    store.connection.execute("DELETE FROM swarm_cluster_leases WHERE group_id=?1",[group])
        .map_err(|e|e.to_string())?;
    Ok(())
}
pub fn all_groups(store:&DaemonStore)->Result<Vec<String>,String>{
    schema(store)?;
    let mut st=store.connection.prepare("SELECT group_id FROM swarm_cluster_rosters LIMIT 16")
        .map_err(|e|e.to_string())?;
    let groups=st.query_map([],|r|r.get::<_,String>(0)).map_err(|e|e.to_string())?
        .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    Ok(groups)
}
pub async fn campaign(paths:&DaemonPaths,group:&str,term:u64,now:i64)
    ->Result<serde_json::Value,String>{
    let mut store=DaemonStore::open(paths)?;
    let config=roster(&store,group)?;
    if store.kill_switch()?{
        expire_leader(&store,group)?;
        return Err("Global kill switch prevents local cluster election and renewal".into());
    }
    let req=VoteRequest{group:group.to_string(),term,
        candidate_fingerprint:config.local().fingerprint.clone(),
        roster_sha256:config.digest()};
    let local=vote(&mut store,&req,"@local",now)?;
    let mut receipts=vec![local];
    drop(store);
    for member in config.members.iter().filter(|m|m.device_id!="@local"){
        let Some(alias)=member.alias.as_deref() else{continue;};
        // This signs and validates the request using the existing peer
        // controller secret, authenticated sequence and pinned remote TLS cert.
        let bytes=serde_json::to_vec(&req).map_err(|e|e.to_string())?;
        let response=super::remote::signed_swarm_vote(
            paths,alias,&member.fingerprint,bytes,now as u64).await;
        let Ok(value)=response else{continue;};
        let Ok(reply)=serde_json::from_value::<VoteReceipt>(value) else{continue;};
        if reply.voter_fingerprint==member.fingerprint{receipts.push(reply);}
    }
    let store=DaemonStore::open(paths)?;
    match persist_quorum(&store,&req,&receipts,now){
        Ok(expires)=>Ok(serde_json::json!({"status":"elected","cluster":group,
            "term":term,"leader":req.candidate_fingerprint,
            "quorum":receipts.len(),"members":config.members.len(),"expires_at_ms":expires})),
        Err(error)=>{
            expire_leader(&store,group)?;
            Err(error)
        },
    }
}
/// Only operator-configured clusters have a worker, and they retain their
/// frozen membership. A failed probe does not release a previous vote early.
pub fn spawn_lease_worker(paths:DaemonPaths){
    tauri::async_runtime::spawn(async move{
        let mut interval=tokio::time::interval(std::time::Duration::from_secs(5));
        loop{
            interval.tick().await;
            let Ok(store)=DaemonStore::open(&paths) else{continue;};
            let Ok(groups)=all_groups(&store) else{continue;};
            for group in groups{
                let Ok(conf)=roster(&store,&group) else{continue;};
                let Ok(now_u)=super::now_ms() else{continue;};
                let now=now_u as i64;
                let is_leader=may_schedule(&store,&group,now).unwrap_or(false);
                let existing=store.connection.query_row(
                    "SELECT expires_at_ms FROM swarm_cluster_leases WHERE group_id=?1",
                    [&group],|r|r.get::<_,i64>(0)
                ).optional().ok().flatten().unwrap_or(0);
                let voted=store.connection.query_row(
                    "SELECT expires_at_ms,term FROM swarm_cluster_votes WHERE group_id=?1",
                    [&group],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))
                ).optional().ok().flatten();
                let rank=conf.members.iter().filter(|m|m.fingerprint<conf.local().fingerprint).count();
                let (eligible,term)=if is_leader&&existing<now+10_000{
                    (true,voted.as_ref().map(|x|x.1 as u64).unwrap_or(1))
                }else if !is_leader && existing<=now &&
                    voted.map(|x|x.0+rank as i64*4_000<=now).unwrap_or(true) {
                    (true,next_term(&store,&group).unwrap_or(1))
                }else{(false,0)};
                if !eligible{continue;}
                // No SQLite statements or mutexes survive across network await.
                let _=campaign(&paths,&group,term,now).await;
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    fn members()->ClusterRoster{
        ClusterRoster{version:1,group:"team".into(),members:vec![
            ClusterMember{fingerprint:"a".repeat(64),device_id:"@local".into(),alias:None},
            ClusterMember{fingerprint:"b".repeat(64),device_id:"signed-b".into(),alias:Some("b".into())},
            ClusterMember{fingerprint:"c".repeat(64),device_id:"signed-c".into(),alias:Some("c".into())},
        ]}
    }
    fn req(c:&ClusterRoster,term:u64,candidate:&str)->VoteRequest{
        VoteRequest{group:"team".into(),term,candidate_fingerprint:candidate.into(),roster_sha256:c.digest()}
    }
    #[test]
    fn durable_one_vote_per_term_and_no_overlap_even_after_reopen(){
        let mut store=DaemonStore::open_in_memory().unwrap();
        let config=members();configure(&mut store,&config,1000).unwrap();
        let first=vote(&mut store,&req(&config,1,&"a".repeat(64)),"@local",1000).unwrap();
        assert!(vote(&mut store,&req(&config,1,&"b".repeat(64)),"signed-b",1200).is_err());
        assert!(vote(&mut store,&req(&config,2,&"b".repeat(64)),"signed-b",1200).is_err());
        assert!(vote(&mut store,&req(&config,1,&"a".repeat(64)),"@local",1400).is_ok());
        assert!(vote(&mut store,&req(&config,2,&"b".repeat(64)),"signed-b",25000).is_ok());
        assert!(vote(&mut store,&req(&config,1,&"a".repeat(64)),"@local",26000).is_err());
        assert_eq!(first.voter_fingerprint,"a".repeat(64));
    }
    #[test]
    fn only_authenticated_identity_and_same_roster_can_vote(){
        let mut store=DaemonStore::open_in_memory().unwrap();
        let config=members();configure(&mut store,&config,1000).unwrap();
        assert!(vote(&mut store,&req(&config,1,&"b".repeat(64)),"wrong-id",1000).is_err());
        let mut invalid=req(&config,1,&"b".repeat(64));
        invalid.roster_sha256="0".repeat(64);
        assert!(vote(&mut store,&invalid,"signed-b",1000).is_err());
    }
    #[test]
    fn rejects_epoch_exhaustion_and_nonfinite_lease_arithmetic(){
        let mut store=DaemonStore::open_in_memory().unwrap();
        let config=members();configure(&mut store,&config,1000).unwrap();
        let mut req=req(&config,u64::MAX,&"a".repeat(64));
        assert!(vote(&mut store,&req,"@local",1000).is_err());
        req.term=1;
        let good=vote(&mut store,&req,"@local",1000).unwrap();
        let mut malicious=good.clone();
        malicious.voter_fingerprint="b".repeat(64);
        malicious.expires_at_ms=i64::MIN;
        assert!(persist_quorum(&store,&req,&[good,malicious],1000).is_err());
    }
    #[test]
    fn strict_quorum_fences_new_job_dispatch_and_rejects_fake_votes(){
        let mut store=DaemonStore::open_in_memory().unwrap();
        let config=members();configure(&mut store,&config,1000).unwrap();
        let request=req(&config,1,&"a".repeat(64));
        let local=vote(&mut store,&request,"@local",1000).unwrap();
        assert!(!may_schedule(&store,"team",1000).unwrap());
        assert!(persist_quorum(&store,&request,&[local.clone()],1000).is_err());
        let mut remote=local.clone();remote.voter_fingerprint="b".repeat(64);
        assert!(persist_quorum(&store,&request,&[local.clone(),remote.clone()],1000).is_ok());
        assert!(may_schedule(&store,"team",2000).unwrap());
        assert!(!may_schedule(&store,"team",17000).unwrap());
        remote.roster_sha256="0".repeat(64);
        assert!(persist_quorum(&store,&request,&[local,remote],1000).is_err());
    }
}


pub fn status(paths:&DaemonPaths,group:&str,now:i64)->Result<serde_json::Value,String>{
    let store=DaemonStore::open(paths)?;
    let config=roster(&store,group)?;
    let lease=store.connection.query_row(
        "SELECT term,leader,expires_at_ms FROM swarm_cluster_leases WHERE group_id=?1",
        [group],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?))
    ).optional().map_err(|e|e.to_string())?;
    Ok(serde_json::json!({
        "group":group,"roster_sha256":config.digest(),
        "configured_members":config.members.len(),
        "local_fingerprint":config.local().fingerprint,
        "lease":lease.map(|(term,leader,expires)|serde_json::json!({
            "term":term,"leader":leader,"expires_at_ms":expires,
            "active":expires>now
        })),
        "may_schedule":may_schedule(&store,group,now)?,
    }))
}
