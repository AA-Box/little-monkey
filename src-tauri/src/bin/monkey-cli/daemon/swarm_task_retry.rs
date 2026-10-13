//! Durable logical identity for bounded, read-only resident DAG reassignment.
//!
//! A node is the logical task. Its first attempt retains the historic stable
//! job ID; subsequent attempts use deterministic, distinct IDs. A failed job
//! must be *terminal* in the authoritative local daemon ledger before the
//! cursor advances in one SQLite transaction. No cancelled, running,
//! approval-pending or needs-reconciliation job is ever retried here.
//!
//! This is not distributed exactly-once execution: other machines have
//! independent SQLite stores, and arbitrary side effects need a transactional
//! or idempotent destination. The cluster's quorum/epoch gates still apply.
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use super::store::DaemonStore;

const MAX_READ_ONLY_REASSIGNMENTS: i64 = 2;

fn schema(store: &DaemonStore) -> Result<(), String> {
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS resident_swarm_task_reassignments (
            graph_id TEXT NOT NULL,
            node_id TEXT NOT NULL,
            attempts INTEGER NOT NULL CHECK(attempts BETWEEN 1 AND 2),
            previous_job_id TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY(graph_id,node_id)
         );"
    ).map_err(|error| error.to_string())
}

/// Number of already persisted reassignments. Zero keeps all pre-existing
/// node/job identities unchanged, including after daemon upgrades.
pub fn count(store: &DaemonStore, graph: &str, node: &str) -> Result<i64, String> {
    schema(store)?;
    store.connection.query_row(
        "SELECT attempts FROM resident_swarm_task_reassignments
         WHERE graph_id=?1 AND node_id=?2",
        params![graph,node], |row| row.get(0),
    ).optional().map(|value|value.unwrap_or(0)).map_err(|error|error.to_string())
}

/// Atomic CAS: only a terminal *failed* daemon job that is still the current
/// submitted primary attempt may be replaced. A delayed completion of an old
/// job, a failed fallback or a second coordinator tick cannot win the CAS.
pub fn reassign_failed_primary(
    store: &mut DaemonStore, graph: &str, node: &str, job: &str, now: i64,
) -> Result<bool, String> {
    schema(store)?;
    if now<=0 {return Err("Reassignment requires a positive clock".into());}
    let tx=store.connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error|error.to_string())?;
    let job_state:Option<String>=tx.query_row(
        "SELECT state FROM daemon_jobs WHERE job_id=?1",
        [job], |row|row.get(0)
    ).optional().map_err(|error|error.to_string())?;
    if job_state.as_deref()!=Some("failed"){return Ok(false);}
    let existing:Option<i64>=tx.query_row(
        "SELECT attempts FROM resident_swarm_task_reassignments
         WHERE graph_id=?1 AND node_id=?2",
        params![graph,node],|row|row.get(0),
    ).optional().map_err(|error|error.to_string())?;
    let previous=existing.unwrap_or(0);
    if previous>=MAX_READ_ONLY_REASSIGNMENTS {return Ok(false);}
    let changed=tx.execute(
        "UPDATE resident_swarm_nodes
         SET state='pending',job_id=NULL,error=?4
         WHERE graph_id=?1 AND node_id=?2 AND job_id=?3 AND state='submitted'",
        params![graph,node,job,format!(
            "Read-only primary failed; new durable attempt {}/{} scheduled",
            previous+1,MAX_READ_ONLY_REASSIGNMENTS
        )]
    ).map_err(|error|error.to_string())?;
    if changed!=1{return Ok(false);}
    let recorded=tx.execute(
        "INSERT INTO resident_swarm_task_reassignments
           (graph_id,node_id,attempts,previous_job_id,updated_at_ms)
         VALUES (?1,?2,?3,?4,?5)
         ON CONFLICT(graph_id,node_id) DO UPDATE SET
           attempts=excluded.attempts,
           previous_job_id=excluded.previous_job_id,
           updated_at_ms=excluded.updated_at_ms
         WHERE resident_swarm_task_reassignments.attempts=?6",
        params![graph,node,previous+1,job,now,previous]
    ).map_err(|error|error.to_string())?;
    if recorded!=1 {
        return Err("Reassignment cursor CAS failed; refusing to dispatch".into());
    }
    tx.commit().map_err(|error|error.to_string())?;
    Ok(true)
}

/// A result can win only when it belongs to the task's currently authorized
/// attempt and the corresponding daemon job is durably succeeded.
pub fn commit_current_success(
    store: &DaemonStore, graph: &str, node: &str, job: &str,
) -> Result<bool, String> {
    let changed=store.connection.execute(
        "UPDATE resident_swarm_nodes SET state='succeeded',error=NULL
         WHERE graph_id=?1 AND node_id=?2 AND job_id=?3
           AND state IN ('submitted','submitted_fallback')
           AND EXISTS (SELECT 1 FROM daemon_jobs WHERE job_id=?3 AND state='succeeded')",
        params![graph,node,job]
    ).map_err(|error|error.to_string())?;
    Ok(changed==1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> DaemonStore {
        let store=DaemonStore::open_in_memory().unwrap();
        store.connection.execute_batch(
            "CREATE TABLE resident_swarm_nodes (
               graph_id TEXT NOT NULL,node_id TEXT NOT NULL,
               state TEXT NOT NULL,job_id TEXT,error TEXT,
               PRIMARY KEY(graph_id,node_id)
             );
             INSERT INTO resident_swarm_nodes(graph_id,node_id,state,job_id)
               VALUES('graph','scan','submitted','job-first');"
        ).unwrap();
        store
    }
    fn job(store:&DaemonStore,id:&str,state:&str) {
        store.connection.execute(
            "INSERT INTO daemon_jobs
               (job_id,recipe_snapshot,state,priority,attempt,max_attempts,
                created_at_ms,updated_at_ms,max_runtime_ms,max_log_bytes)
             VALUES (?1,'/tmp/readonly.json',?2,0,1,1,1000,1000,1000,1024)",
            params![id,state]
        ).unwrap();
    }
    #[test]
    fn failed_read_only_work_is_reassigned_twice_but_never_dispatched_with_an_old_identity() {
        let mut s=state();
        job(&s,"job-first","failed");
        assert_eq!(count(&s,"graph","scan").unwrap(),0);
        assert!(reassign_failed_primary(&mut s,"graph","scan","job-first",2000).unwrap());
        assert_eq!(count(&s,"graph","scan").unwrap(),1);
        assert!(!reassign_failed_primary(&mut s,"graph","scan","job-first",2001).unwrap());
        s.connection.execute(
            "UPDATE resident_swarm_nodes SET state='submitted',job_id='job-second'
             WHERE graph_id='graph' AND node_id='scan'",[]
        ).unwrap();
        job(&s,"job-second","running");
        assert!(!reassign_failed_primary(&mut s,"graph","scan","job-second",2002).unwrap());
        // Even a wrongly delayed terminal success cannot win once its
        // logical attempt has been superseded.
        s.connection.execute("UPDATE daemon_jobs SET state='succeeded'
            WHERE job_id='job-first'",[]).unwrap();
        assert!(!commit_current_success(&s,"graph","scan","job-first").unwrap());
        s.connection.execute("UPDATE daemon_jobs SET state='failed'
            WHERE job_id='job-second'",[]).unwrap();
        assert!(reassign_failed_primary(&mut s,"graph","scan","job-second",2003).unwrap());
        assert_eq!(count(&s,"graph","scan").unwrap(),2);
        s.connection.execute(
            "UPDATE resident_swarm_nodes SET state='submitted',job_id='job-third'
             WHERE graph_id='graph' AND node_id='scan'",[]
        ).unwrap();
        job(&s,"job-third","failed");
        assert!(!reassign_failed_primary(&mut s,"graph","scan","job-third",2004).unwrap());
        let (cursor,job_id):(String,Option<String>)=s.connection.query_row(
            "SELECT state,job_id FROM resident_swarm_nodes WHERE graph_id='graph'",
            [],|r|Ok((r.get(0)?,r.get(1)?))
        ).unwrap();
        assert_eq!((cursor.as_str(),job_id.as_deref()),("submitted",Some("job-third")));
    }
    #[test]
    fn only_current_durable_success_is_committed_and_uncertain_jobs_never_retry(){
        let mut s=state();
        job(&s,"job-first","needs_reconciliation");
        assert!(!reassign_failed_primary(&mut s,"graph","scan","job-first",2000).unwrap());
        assert!(!commit_current_success(&s,"graph","scan","job-first").unwrap());
        s.connection.execute("UPDATE daemon_jobs SET state='succeeded'
            WHERE job_id='job-first'",[]).unwrap();
        assert!(commit_current_success(&s,"graph","scan","job-first").unwrap());
        assert!(!commit_current_success(&s,"graph","scan","job-first").unwrap());
        assert!(!reassign_failed_primary(&mut s,"graph","scan","job-first",2001).unwrap());
    }
    #[test]
    fn cancelled_and_inflight_work_cannot_be_reassigned(){
        for job_state in ["queued","running","waiting_approval","paused","cancelling","cancelled","succeeded"] {
            let mut s=state();
            job(&s,"job-first",job_state);
            assert!(!reassign_failed_primary(&mut s,"graph","scan","job-first",2000).unwrap());
            assert_eq!(count(&s,"graph","scan").unwrap(),0);
        }
    }
    #[test]
    fn retry_counter_and_single_winner_survive_a_real_sqlite_reopen(){
        let root=std::env::temp_dir().join(format!(
            "swarm-attempt-durability-{}",uuid::Uuid::new_v4()
        ));
        let paths=super::super::store::DaemonPaths::under(&root);
        {
            let mut s=DaemonStore::open(&paths).unwrap();
            s.connection.execute_batch(
                "CREATE TABLE resident_swarm_nodes (
                   graph_id TEXT NOT NULL,node_id TEXT NOT NULL,
                   state TEXT NOT NULL,job_id TEXT,error TEXT,
                   PRIMARY KEY(graph_id,node_id)
                 );
                 INSERT INTO resident_swarm_nodes(graph_id,node_id,state,job_id)
                   VALUES('graph','scan','submitted','job-first');"
            ).unwrap();
            job(&s,"job-first","failed");
            assert!(reassign_failed_primary(
                &mut s,"graph","scan","job-first",2000
            ).unwrap());
        }
        {
            let s=DaemonStore::open(&paths).unwrap();
            assert_eq!(count(&s,"graph","scan").unwrap(),1);
            s.connection.execute(
                "UPDATE resident_swarm_nodes
                 SET state='submitted',job_id='job-second'
                 WHERE graph_id='graph' AND node_id='scan'",[]
            ).unwrap();
            job(&s,"job-second","succeeded");
            // The daemon must not accept a prior job as the winning result.
            assert!(!commit_current_success(&s,"graph","scan","job-first").unwrap());
            assert!(commit_current_success(&s,"graph","scan","job-second").unwrap());
        }
        let _=std::fs::remove_dir_all(root);
    }

}
