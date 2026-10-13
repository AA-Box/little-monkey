//! Explicitly armed automatic READ-ONLY resident swarm failover.
//!
//! Both installations must be individually operator-enrolled in the same
//! signed peer cluster. The receiving operator pre-stages and hashes a known
//! plan-only portable bundle and an existing local workspace. It does NOT
//! accept runnable prompts from the network. Once a DIFFERENT peer's fresh
//! signed checkpoint exists and this node later gains a HIGHER-TERM strict
//! majority lease, a durable one-shot claim authorizes registering the
//! preapproved plan under a NEW graph ID. Every node starts pending and must
//! re-run; remote 'succeeded' rows have no scheduling authority.
//!
//! This is bounded read-only re-execution, not transparent exactly-once
//! migration or arbitrary remote repository mutation. A crash after claim
//! fails closed and requires explicit operator reconciliation.
use std::fs;
use std::path::{Path,PathBuf};
use rusqlite::{params,OptionalExtension};
use sha2::{Digest,Sha256};
use super::store::{DaemonPaths,DaemonStore};
use super::{swarm_cluster,swarm_replication,swarm_transfer};

const MAX_BUNDLE:u64=34*1024*1024;
const MAX_PLANS:i64=8;
const MAX_CHECKPOINT_AGE_MS:i64=10*60*1000;
#[derive(Debug,Clone)]
struct ArmedPlan{
    id:String,
    group:String,
    source_graph:String,
    source_fingerprint:String,
    new_graph:String,
    path:String,
    hash:String,
    workspace:String,
}
fn valid_id(value:&str)->bool{
    (1..=80).contains(&value.len()) &&
    value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) &&
    value.bytes().all(|c|c.is_ascii_alphanumeric()||matches!(c,b'_'|b'-'))
}
fn schema(store:&DaemonStore)->Result<(),String>{
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS resident_swarm_failover_plans(
          plan_id TEXT PRIMARY KEY NOT NULL,
          group_id TEXT NOT NULL, source_graph TEXT NOT NULL,
          source_fingerprint TEXT,
          new_graph TEXT NOT NULL,bundle_path TEXT NOT NULL,
          bundle_sha256 TEXT NOT NULL,workspace TEXT NOT NULL,
          state TEXT NOT NULL,created_at_ms INTEGER NOT NULL,
          claimed_at_ms INTEGER,message TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_failover_unique_graph
          ON resident_swarm_failover_plans(new_graph);"
    ).map_err(|e|e.to_string())?;
    // Older draft builds did not bind a pre-authorized source identity.
    // Migrated rows retain NULL and are NEVER eligible for auto failover.
    // An operator must enroll a fresh plan naming the exact predecessor.
    let mut stmt=store.connection.prepare("PRAGMA table_info(resident_swarm_failover_plans)")
        .map_err(|e|e.to_string())?;
    let cols=stmt.query_map([],|r|r.get::<_,String>(1))
        .map_err(|e|e.to_string())?
        .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    if !cols.iter().any(|name|name=="source_fingerprint"){
        store.connection.execute_batch(
            "ALTER TABLE resident_swarm_failover_plans ADD COLUMN source_fingerprint TEXT"
        ).map_err(|e|e.to_string())?;
    }
    Ok(())
}
fn bounded_file(path:&Path)->Result<Vec<u8>,String>{
    let m=fs::symlink_metadata(path).map_err(|e|e.to_string())?;
    if !m.file_type().is_file()||m.len()>MAX_BUNDLE||m.len()==0{
        return Err("Failover bundle must be a regular non-symlink file under 34 MiB".into());
    }
    fs::read(path).map_err(|e|e.to_string())
}
fn sha(bytes:&[u8])->String{format!("{:x}",Sha256::digest(bytes))}
pub fn arm(
    paths:&DaemonPaths,id:&str,group:&str,source_peer:&str,new_graph:&str,
    input:&Path,workspace:&Path,now:i64,
)->Result<serde_json::Value,String>{
    if !valid_id(id)||!valid_id(group)||!valid_id(new_graph)||now<=0||
       !input.is_absolute()||!workspace.is_absolute(){
        return Err("Invalid operator failover plan identities or absolute paths".into());
    }
    let bundle=bounded_file(input)?;
    let canonical_input=input.canonicalize().map_err(|e|e.to_string())?;
    let canonical_workspace=workspace.canonicalize().map_err(|e|e.to_string())?;
    let meta=fs::symlink_metadata(workspace).map_err(|e|e.to_string())?;
    if !meta.is_dir()||meta.file_type().is_symlink()||
       !canonical_workspace.is_dir(){
        return Err("Failover destination must be an existing real directory".into());
    }
    let (source_graph,source_cluster)=swarm_transfer::inspect_read_only_bundle(&canonical_input)?;
    if source_graph==new_graph||source_cluster.as_deref()!=Some(group){
        return Err("Prepared read-only bundle must be from this exact cluster, with a NEW target graph identity".into());
    }
    paths.ensure()?;
    let store=DaemonStore::open(paths)?;
    let roster=swarm_cluster::roster(&store,group)?;
    let source=roster.members.iter().find(|member|
        member.alias.as_deref()==Some(source_peer) && member.device_id!="@local"
    ).ok_or("Previous leader must be a specific operator-enrolled signed peer alias")?;
    let source_fingerprint=source.fingerprint.clone();
    schema(&store)?;
    let count:i64=store.connection.query_row(
        "SELECT COUNT(*) FROM resident_swarm_failover_plans",[],
        |r|r.get(0)
    ).map_err(|e|e.to_string())?;
    if count>=MAX_PLANS {
        return Err("Automatic failover plan capacity reached".into());
    }
    // Arm is a one-way, explicit local approval; no upsert and no implicit
    // replacing a previously claimed/failed plan.
    store.connection.execute(
        "INSERT INTO resident_swarm_failover_plans(
           plan_id,group_id,source_graph,source_fingerprint,new_graph,bundle_path,bundle_sha256,
           workspace,state,created_at_ms)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'armed',?9)",
        params![id,group,source_graph,source_fingerprint,new_graph,
            canonical_input.to_string_lossy().as_ref(),sha(&bundle),
            canonical_workspace.to_string_lossy().as_ref(),now],
    ).map_err(|e|format!("Unable to arm unique operator failover plan: {e}"))?;
    Ok(serde_json::json!({
        "plan":id,"cluster":group,"source_graph":source_graph,
        "source_fingerprint":source_fingerprint,
        "new_graph":new_graph,"state":"armed",
        "warning":"Requires a fresh previous-leader checkpoint and a NEW higher-term signed local majority; every task re-runs in plan mode."
    }))
}
pub fn status(paths:&DaemonPaths)->Result<serde_json::Value,String>{
    let store=DaemonStore::open(paths)?;
    schema(&store)?;
    let mut stmt=store.connection.prepare(
        "SELECT plan_id,group_id,source_graph,source_fingerprint,new_graph,state,
                created_at_ms,claimed_at_ms,message
         FROM resident_swarm_failover_plans ORDER BY created_at_ms DESC LIMIT 8"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|r|Ok(serde_json::json!({
        "id":r.get::<_,String>(0)?,"group":r.get::<_,String>(1)?,
        "source_graph":r.get::<_,String>(2)?,
        "source_fingerprint":r.get::<_,Option<String>>(3)?,
        "target_graph":r.get::<_,String>(4)?,
        "state":r.get::<_,String>(5)?,"created_at_ms":r.get::<_,i64>(6)?,
        "claimed_at_ms":r.get::<_,Option<i64>>(7)?,
        "message":r.get::<_,Option<String>>(8)?
    }))).map_err(|e|e.to_string())?
       .collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    Ok(serde_json::json!({"plans":rows}))
}
fn pending(store:&DaemonStore)->Result<Vec<ArmedPlan>,String>{
    schema(store)?;
    let mut stmt=store.connection.prepare(
        "SELECT plan_id,group_id,source_graph,source_fingerprint,new_graph,bundle_path,
                bundle_sha256,workspace
         FROM resident_swarm_failover_plans
         WHERE state='armed' AND source_fingerprint IS NOT NULL
         ORDER BY created_at_ms LIMIT 8"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|r|Ok(ArmedPlan{
        id:r.get(0)?,group:r.get(1)?,source_graph:r.get(2)?,
        source_fingerprint:r.get(3)?,new_graph:r.get(4)?,path:r.get(5)?,
        hash:r.get(6)?,workspace:r.get(7)?,
    })).map_err(|e|e.to_string())?;
    rows.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())
}
fn eligible(store:&DaemonStore,plan:&ArmedPlan,now:i64)->Result<bool,String>{
    if store.kill_switch()?||
       !swarm_cluster::may_schedule(store,&plan.group,now)?{
        return Ok(false);
    }
    let roster=swarm_cluster::roster(store,&plan.group)?;
    let current:i64=store.connection.query_row(
        "SELECT term FROM swarm_cluster_leases
         WHERE group_id=?1 AND leader=?2 AND expires_at_ms>?3",
        params![plan.group,roster.local().fingerprint,now],|r|r.get(0)
    ).map_err(|_|"No locally certified, unexpired majority term")?;
    // This recovery plan is permitted only after a DIFFERENT enrolled
    // original machine published the exact source graph, and a higher-term
    // election succeeded. An absent or old receipt does not grant execution.
    let peer_rows=swarm_replication::list_local(store,&plan.group,&plan.source_graph)?;
    // A local pending replica may be stale while another voter stored the
    // original leader's already-prepared execution. A freshly elected leader
    // MUST reconcile an intersecting signed majority before importing any
    // work. Never trust the newest copy visible on this machine alone.
    for snapshot in peer_rows.iter().filter(|snapshot|
        snapshot.source_fingerprint==plan.source_fingerprint &&
        snapshot.source_fingerprint!=roster.local().fingerprint &&
        snapshot.roster_sha256==roster.digest() &&
        snapshot.term < current as u64 &&
        snapshot.captured_at_ms<=now+5_000 &&
        now.saturating_sub(snapshot.captured_at_ms)<=MAX_CHECKPOINT_AGE_MS &&
        snapshot.nodes.iter().all(|node|node.state=="pending"&&node.job_id.is_none())
    ){
        if super::swarm_recovery_witness::majority_confirms_no_dispatch(
            store,&plan.group,&plan.source_graph,&plan.source_fingerprint,
            snapshot,now
        )?{return Ok(true);}
    }
    Ok(false)
}
fn mark(store:&DaemonStore,id:&str,state:&str,now:i64,message:&str)->Result<(),String>{
    store.connection.execute(
        "UPDATE resident_swarm_failover_plans
         SET state=?2,claimed_at_ms=COALESCE(claimed_at_ms,?3),message=?4
         WHERE plan_id=?1 AND state='claimed'",
        params![id,state,now,message.chars().take(240).collect::<String>()]
    ).map_err(|e|e.to_string())?;
    Ok(())
}
fn consume(paths:&DaemonPaths,plan:&ArmedPlan,now:i64)->Result<(),String>{
    let store=DaemonStore::open(paths)?;
    if !eligible(&store,plan,now)? {return Ok(());}
    let updated=store.connection.execute(
        "UPDATE resident_swarm_failover_plans
         SET state='claimed',claimed_at_ms=?2
         WHERE plan_id=?1 AND state='armed'",
        params![plan.id,now],
    ).map_err(|e|e.to_string())?;
    if updated!=1 {return Ok(());}
    drop(store);
    // The one-shot claim is COMMITTED before touching any imported files or
    // registering daemon work. A crash cannot launch the same graph twice.
    let operation=(||{
        let path=PathBuf::from(&plan.path);
        let actual=bounded_file(&path)?;
        if sha(&actual)!=plan.hash{
            return Err("Armed recovery bytes were modified after explicit approval".into());
        }
        let (source,group)=swarm_transfer::inspect_read_only_bundle(&path)?;
        if source!=plan.source_graph||group.as_deref()!=Some(plan.group.as_str()){
            return Err("Staged recovery identity or cluster changed".into());
        }
        // Re-check the majority lease just before registration. Dispatch
        // remains separately fenced by the swarm coordinator's own tick.
        let store=DaemonStore::open(paths)?;
        if !swarm_cluster::may_schedule(&store,&plan.group,now)?{
            return Err("Local majority lease was lost before recovery registration".into());
        }
        drop(store);
        // Read ONLY the bytes just validated, not the operator-controlled
        // pathname again. Copy to a fresh private, exclusive daemon snapshot:
        // otherwise a file replaced between SHA check and import could run a
        // different prompt after consent was granted (TOCTOU).
        let staged=paths.snapshots.join(format!(
            "failover-candidate-{}.json",uuid::Uuid::new_v4()
        ));
        let import=(||{
            use std::io::Write;
            let mut file=std::fs::OpenOptions::new()
                .create_new(true).write(true).open(&staged)
                .map_err(|e|e.to_string())?;
            file.write_all(&actual).map_err(|e|e.to_string())?;
            file.sync_all().map_err(|e|e.to_string())?;
            drop(file);
            super::store::restrict_file(&staged)?;
            swarm_transfer::import_read_only(paths,&staged,&plan.new_graph,
                Path::new(&plan.workspace))?;
            Ok::<_,String>(())
        })();
        let _=fs::remove_file(&staged);
        import?;
        Ok::<_,String>(())
    })();
    let store=DaemonStore::open(paths)?;
    match operation {
        Ok(())=>mark(&store,&plan.id,"imported",now,
            "New local graph registered; all source tasks are newly pending, never reused"),
        Err(reason)=>{
            mark(&store,&plan.id,"failed",now,
                "Automatic failover stopped; operator must inspect local plan and graph")?;
            Err(reason)
        }
    }
}
pub fn run_once(paths:&DaemonPaths,now:i64)->Result<(),String>{
    let store=DaemonStore::open(paths)?;
    let plans=pending(&store)?;
    drop(store);
    for plan in plans{
        // One malformed/stale/operator-revoked plan must not prevent other
        // independent clusters from progressing.
        if let Err(error)=consume(paths,&plan,now){
            eprintln!("Read-only failover {} halted: {}",plan.id,
                if error.contains("majority"){"lease changed"}
                else{"operator review required"});
        }
    }
    Ok(())
}
pub fn spawn_worker(paths:DaemonPaths){
    tokio::spawn(async move{
        let mut timer=tokio::time::interval(std::time::Duration::from_secs(10));
        loop{
            timer.tick().await;
            let now=super::now_ms().ok().and_then(|n|i64::try_from(n).ok());
            if let Some(now)=now{
                // Signed peer readback happens asynchronously BEFORE local
                // eligibility. A lost peer gives no certificate or implicit
                // permission to replay an uncertain in-flight task.
                if let Err(error)=super::swarm_recovery_witness::refresh(&paths,now).await{
                    eprintln!("Read-only swarm recovery quorum unavailable: {error}");
                }
                if let Err(error)=run_once(&paths,now){
                    eprintln!("Read-only swarm failover scan unavailable: {error}");
                }
            }
        }
    });
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn isolated_original_leader_is_recovered_by_the_other_two_voters_without_reusing_foreign_jobs(){
        use super::super::swarm_cluster::{ClusterMember,ClusterRoster,VoteRequest};
        use super::super::swarm_coordinator::{ResidentSwarmSpec,ResidentSwarmNode};
        let root=std::env::temp_dir().join(format!("swarm-three-host-handoff-{}",uuid::Uuid::new_v4()));
        let sender=DaemonPaths::under(&root.join("sender"));
        let receiver=DaemonPaths::under(&root.join("receiver"));
        let witness=DaemonPaths::under(&root.join("witness"));
        sender.ensure().unwrap();receiver.ensure().unwrap();witness.ensure().unwrap();
        let workspace_sender=root.join("sender-work");
        let workspace_receiver=root.join("receiver-work");
        fs::create_dir_all(&workspace_sender).unwrap();
        fs::create_dir_all(&workspace_receiver).unwrap();
        let afp="a".repeat(64);
        let bfp="b".repeat(64);
        let cfp="c".repeat(64);
        let a_roster=ClusterRoster{version:1,group:"swarm-team".into(),members:vec![
            ClusterMember{fingerprint:afp.clone(),device_id:"@local".into(),alias:None},
            ClusterMember{fingerprint:bfp.clone(),device_id:"signed-b".into(),alias:Some("bob".into())},
            ClusterMember{fingerprint:cfp.clone(),device_id:"signed-c".into(),alias:Some("charlie".into())}
        ]};
        let b_roster=ClusterRoster{version:1,group:"swarm-team".into(),members:vec![
            ClusterMember{fingerprint:bfp.clone(),device_id:"@local".into(),alias:None},
            ClusterMember{fingerprint:afp.clone(),device_id:"signed-a".into(),alias:Some("alice".into())},
            ClusterMember{fingerprint:cfp.clone(),device_id:"signed-c".into(),alias:Some("charlie".into())}
        ]};
        let c_roster=ClusterRoster{version:1,group:"swarm-team".into(),members:vec![
            ClusterMember{fingerprint:cfp.clone(),device_id:"@local".into(),alias:None},
            ClusterMember{fingerprint:afp.clone(),device_id:"signed-a".into(),alias:Some("alice".into())},
            ClusterMember{fingerprint:bfp.clone(),device_id:"signed-b".into(),alias:Some("bob".into())}
        ]};
        assert_eq!(a_roster.digest(),b_roster.digest());
        assert_eq!(a_roster.digest(),c_roster.digest());
        {
            let mut a=DaemonStore::open(&sender).unwrap();
            let mut b=DaemonStore::open(&receiver).unwrap();
            let mut c=DaemonStore::open(&witness).unwrap();
            swarm_cluster::configure(&mut a,&a_roster,1000).unwrap();
            swarm_cluster::configure(&mut b,&b_roster,1000).unwrap();
            swarm_cluster::configure(&mut c,&c_roster,1000).unwrap();
            let term1=VoteRequest{
                group:"swarm-team".into(),term:1,candidate_fingerprint:afp.clone(),
                roster_sha256:a_roster.digest()
            };
            let voted_a=swarm_cluster::vote(&mut a,&term1,"@local",1000).unwrap();
            let voted_b=swarm_cluster::vote(&mut b,&term1,"signed-a",1000).unwrap();
            swarm_cluster::persist_quorum(&a,&term1,&[voted_a,voted_b],1000).unwrap();
        }
        let recipe=root.join("sender-recipe.json");
        fs::write(&recipe,serde_json::json!({
            "version":1,"name":"safe-plan","target":{"ollama":"qwen2.5:7b"},
            "workspace":workspace_sender,"permission_mode":"plan",
            "prompt":"Read-only operator-approved fixture inspection","params":{}
        }).to_string()).unwrap();
        let spec=ResidentSwarmSpec{
            version:1,id:"original".into(),cluster_id:Some("swarm-team".into()),
            max_parallel:1,nodes:vec![ResidentSwarmNode{
                id:"audit".into(),recipe:recipe.to_string_lossy().to_string(),
                fallback_recipe:None,depends_on:Vec::new(),
                include_dependency_reports:false,max_runtime_seconds:120
            }]
        };
        let spec_file=root.join("original-spec.json");
        fs::write(&spec_file,serde_json::to_vec(&spec).unwrap()).unwrap();
        super::super::swarm_coordinator::submit(&sender,&spec_file).unwrap();
        let transfer=root.join("preapproved-transfer.json");
        swarm_transfer::export(&sender,"original",&transfer).unwrap();
        let record=swarm_replication::capture_local_leader(
            &sender,"swarm-team","original","bob",&bfp,2000
        ).unwrap();
        {
            let mut b=DaemonStore::open(&receiver).unwrap();
            assert!(swarm_replication::ingest(&mut b,"signed-a",&record,2001).unwrap());
        }
        arm(&receiver,"operator-recovery","swarm-team","alice","recovered",&transfer,
            &workspace_receiver,2100).unwrap();
        // The receiver cannot schedule prior to its own fresh signed quorum,
        // even though it has both plan bytes and the previous leader's status.
        run_once(&receiver,2200).unwrap();
        assert_eq!(status(&receiver).unwrap()["plans"][0]["state"],"armed");
        {
            // Simulate a physical partition: NO operations contact A after
            // it exported the verified read-only source checkpoint. B and C
            // independently form the new majority after the former lease.
            let mut b=DaemonStore::open(&receiver).unwrap();
            let mut c=DaemonStore::open(&witness).unwrap();
            let term2=VoteRequest{
                group:"swarm-team".into(),term:2,
                candidate_fingerprint:bfp.clone(),roster_sha256:b_roster.digest()
            };
            let vote_b=swarm_cluster::vote(&mut b,&term2,"@local",30_000).unwrap();
            let vote_c=swarm_cluster::vote(&mut c,&term2,"signed-b",30_000).unwrap();
            swarm_cluster::persist_quorum(&b,&term2,&[vote_b,vote_c],30_000).unwrap();
            assert!(swarm_cluster::may_schedule(&b,"swarm-team",30_001).unwrap());
            assert!(!swarm_cluster::may_schedule(&c,"swarm-team",30_001).unwrap());
        }
        // A different enrolled member can honestly publish the SAME graph
        // name, but that must not authorize re-running A's staged plan.
        {
            let mut store=DaemonStore::open(&receiver).unwrap();
            store.connection.execute(
                "DELETE FROM resident_swarm_peer_checkpoints
                 WHERE group_id=?1 AND graph_id=?2 AND source_fp=?3",
                params!["swarm-team","original",afp]
            ).unwrap();
            let mut not_predecessor=record.clone();
            not_predecessor.source_fingerprint=cfp.clone();
            not_predecessor.seal().unwrap();
            assert!(swarm_replication::ingest(
                &mut store,"signed-c",&not_predecessor,30_001
            ).unwrap());
        }
        // The new B+C majority's C witness independently queried its local
        // durable copy of A's latest checkpoint and found no prior attempt.
        // A signed pinned peer GET is mandatory in production; the unit test
        // injects the exact parsed signed reply at the record boundary.
        super::super::swarm_recovery_witness::record(
            &receiver,"swarm-team","original",&afp,&cfp,
            Some(&record),30_001
        ).unwrap();
        run_once(&receiver,30_001).unwrap();
        assert_eq!(status(&receiver).unwrap()["plans"][0]["state"],"armed",
            "an unrelated enrolled member must not trigger the reviewed predecessor's transfer");
        {
            let mut store=DaemonStore::open(&receiver).unwrap();
            assert!(swarm_replication::ingest(
                &mut store,"signed-a",&record,30_002
            ).unwrap());
        }
        run_once(&receiver,30_002).unwrap();
        assert_eq!(status(&receiver).unwrap()["plans"][0]["state"],"imported");
        let graph=super::super::swarm_coordinator::status(&receiver,Some("recovered")).unwrap();
        assert_eq!(graph["graphs"][0]["tasks"][0]["state"],"pending");
        assert!(graph["graphs"][0]["tasks"][0]["job_id"].is_null(),
            "foreign model successes and job identities cannot be transferred");
        run_once(&receiver,30_002).unwrap();
        assert_eq!(super::super::swarm_coordinator::status(&receiver,Some("recovered"))
            .unwrap()["graphs"].as_array().unwrap().len(),1);
        let reloaded=DaemonStore::open(&receiver).unwrap();
        let count:i64=reloaded.connection.query_row(
            "SELECT COUNT(*) FROM resident_swarm_failover_plans
             WHERE state='imported'",[],|r|r.get(0)
        ).unwrap();
        assert_eq!(count,1,"operator consent and durable one-shot claim survive reopen");
        drop(reloaded);
        // A predecessor that already dispatched even one job is ambiguous:
        // the new leader cannot know whether an external inference is still
        // running. It MUST NOT re-enqueue those jobs automatically.
        arm(&receiver,"ambiguous-recovery","swarm-team","alice",
            "do-not-replay",&transfer,&workspace_receiver,30_003).unwrap();
        let mut attempted=record.clone();
        attempted.sequence=2;
        attempted.nodes[0].state="submitted".into();
        attempted.nodes[0].job_id=Some("original-inflight-job".into());
        attempted.seal().unwrap();
        {
            let mut store=DaemonStore::open(&receiver).unwrap();
            assert!(swarm_replication::ingest(
                &mut store,"signed-a",&attempted,30_003
            ).unwrap());
        }
        run_once(&receiver,30_004).unwrap();
        let all=status(&receiver).unwrap();
        let blocked=all["plans"].as_array().unwrap().iter()
            .find(|plan|plan["id"]=="ambiguous-recovery").unwrap();
        assert_eq!(blocked["state"],"armed",
            "uncertain already-started remote work must never be blindly replayed");
        assert!(super::super::swarm_coordinator::status(&receiver,
            Some("do-not-replay")).unwrap()["graphs"].as_array().unwrap().is_empty());
        let _=fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_unapproved_or_malformed_read_only_bundle_and_ids(){
        let root=std::env::temp_dir().join(format!("failover-{}",uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let paths=DaemonPaths::under(&root);
        let source=root.join("bad.json");
        fs::write(&source,b"{}").unwrap();
        assert!(!valid_id("../bad"));
        assert!(!valid_id(""));
        assert!(arm(&paths,"review","cluster","previous-node","new-graph",&source,&root,1000).is_err());
        let _=fs::remove_dir_all(root);
    }
}
