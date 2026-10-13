//! Explicitly shared, multi-writer vector memory over paired, signed peers.
//!
//! Memory bytes are never exposed by ordinary peer standing. Both the peer's
//! memory-specific capability and a durable per-namespace ACL are required.
//! Each author has an isolated monotonic revision stream; idempotent replays
//! do not overwrite newer versions. Tombstones replicate deletions. The model
//! identifier and vector dimensions are pinned per namespace, rejecting
//! semantically incompatible embedding spaces. This is bounded vector storage,
//! not an embedding-generation service or a general public vector database.
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::store::DaemonStore;

pub const MAX_RECORDS: usize = 128;
/// Supports the maximum 2048-dimensional finite f32 vector after JSON encoding.
pub const MAX_WIRE_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_DIM: usize = 2048;
const MAX_TEXT: usize = 2048;
pub const LOCAL_AUTHOR: &str = "@local";

/// Bind imported writer identity to the pinned remote certificate, not its
/// human-editable alias. Re-pairing a different installation under the same
/// alias must never grant it the ability to advance an old author's revision
/// stream. Certificate rotation creates a new isolated stream.
pub fn remote_author(certificate_sha256: &str) -> Result<String,String> {
    if certificate_sha256.len() != 64 ||
       !certificate_sha256.bytes().all(|byte|byte.is_ascii_hexdigit()) {
        return Err("Invalid pinned peer certificate fingerprint".into());
    }
    Ok(format!("peer-{}",certificate_sha256.to_ascii_lowercase()))
}


#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VectorDocument {
    pub id: String,
    pub revision: u64,
    pub model: String,
    pub text: String,
    pub vector: Vec<f32>,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorEntry {
    pub author: String,
    #[serde(flatten)]
    pub document: VectorDocument,
}

pub fn valid_label(value: &str) -> bool {
    !value.is_empty() && value.len() <= 120 &&
        value.as_bytes()[0].is_ascii_alphanumeric() &&
        value.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-'|b'_'|b'.'))
}
fn valid_model(value: &str) -> bool {
    !value.is_empty() && value.len() <= 160 &&
        value.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c,b'-'|b'_'|b'.'|b'/'|b':'|b'@'))
}
pub fn validate(document: &VectorDocument) -> Result<(), String> {
    if serde_json::to_vec(document).map_err(|error|error.to_string())?.len() > MAX_WIRE_DOCUMENT_BYTES {
        return Err("Vector document exceeds signed peer transport limit".into());
    }
    if !valid_label(&document.id) || !valid_model(&document.model) ||
        document.revision == 0 || document.revision > i64::MAX as u64 ||
        document.text.len() > MAX_TEXT || (!document.deleted && document.text.trim().is_empty()) ||
        document.vector.len() > MAX_DIM ||
        (!document.deleted && document.vector.is_empty()) ||
        document.vector.iter().any(|v| !v.is_finite() || v.abs() > 1_000_000.0) ||
        (!document.deleted && document.vector.iter().all(|v| *v == 0.0)) ||
        (document.deleted && (!document.vector.is_empty() || !document.text.is_empty()))
    {
        return Err("Invalid bounded memory document: id, model, revision, text or finite vector".into());
    }
    Ok(())
}
fn schema(store: &DaemonStore) -> Result<(), String> {
    store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS peer_vector_shares (
           device_id TEXT NOT NULL, namespace TEXT NOT NULL,
           can_read INTEGER NOT NULL, can_write INTEGER NOT NULL,
           PRIMARY KEY(device_id,namespace)
         );
         CREATE TABLE IF NOT EXISTS peer_vector_spaces (
           namespace TEXT PRIMARY KEY, model TEXT NOT NULL, dimensions INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS peer_vector_records (
           namespace TEXT NOT NULL, author TEXT NOT NULL, record_id TEXT NOT NULL,
           revision INTEGER NOT NULL, deleted INTEGER NOT NULL,
           text TEXT NOT NULL, vector_json TEXT NOT NULL,
           PRIMARY KEY(namespace,author,record_id)
         );
         CREATE INDEX IF NOT EXISTS idx_peer_vectors_namespace
           ON peer_vector_records(namespace);
         CREATE TABLE IF NOT EXISTS peer_vector_sync_config (
           alias TEXT NOT NULL, namespace TEXT NOT NULL,
           enabled INTEGER NOT NULL,
           PRIMARY KEY(alias,namespace)
         );"
    ).map_err(|e| e.to_string())
}
pub fn grant(store: &DaemonStore, device_id: &str, namespace: &str, read: bool, write: bool) -> Result<(), String> {
    schema(store)?;
    if !valid_label(device_id) || !valid_label(namespace) {
        return Err("Invalid peer or memory namespace".into());
    }
    store.connection.execute("INSERT INTO peer_vector_shares(device_id,namespace,can_read,can_write)
        VALUES (?1,?2,?3,?4)
        ON CONFLICT(device_id,namespace) DO UPDATE SET
        can_read=excluded.can_read,can_write=excluded.can_write",
        params![device_id,namespace, i64::from(read),i64::from(write)])
        .map_err(|e| e.to_string())?;
    Ok(())
}
pub fn authorized(store: &DaemonStore, device_id: &str, namespace: &str, write: bool) -> Result<bool,String> {
    schema(store)?;
    if !valid_label(namespace) { return Ok(false); }
    let grant: Option<i64> = store.connection.query_row(
        if write {"SELECT can_write FROM peer_vector_shares WHERE device_id=?1 AND namespace=?2"}
        else {"SELECT can_read FROM peer_vector_shares WHERE device_id=?1 AND namespace=?2"},
        params![device_id,namespace],|row|row.get(0),
    ).optional().map_err(|e| e.to_string())?;
    Ok(grant == Some(1))
}
pub fn put(store: &mut DaemonStore, namespace: &str, author: &str, doc: &VectorDocument) -> Result<bool,String> {
    schema(store)?;
    if !valid_label(namespace) || !(author == LOCAL_AUTHOR || valid_label(author)) {
        return Err("Invalid memory namespace or author".into());
    }
    let tx=store.connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e|e.to_string())?;
    let changed=put_one(&tx,namespace,author,doc)?;
    tx.commit().map_err(|e|e.to_string())?;
    Ok(changed)
}

/// Sync an entire authenticated peer snapshot atomically. A malformed record,
/// incompatible embedding space, revision conflict, or capacity error rolls
/// back *every* row from that response; it can never leave a half-applied
/// imported namespace that requires manual repair.
pub fn put_batch(store:&mut DaemonStore,namespace:&str,author:&str,docs:&[VectorDocument])
    -> Result<usize,String>
{
    schema(store)?;
    if !valid_label(namespace) || !(author == LOCAL_AUTHOR || valid_label(author)) ||
       docs.len()>MAX_RECORDS {
        return Err("Invalid bounded vector sync batch".into());
    }
    let mut seen=std::collections::BTreeSet::new();
    for doc in docs {
        validate(doc)?;
        if !seen.insert(doc.id.as_str()) {
            return Err("Duplicate record identifier in vector sync batch".into());
        }
    }
    let tx=store.connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e|e.to_string())?;
    let mut changed=0;
    for doc in docs {
        if put_one(&tx,namespace,author,doc)? {changed+=1;}
    }
    tx.commit().map_err(|e|e.to_string())?;
    Ok(changed)
}

fn put_one(tx:&rusqlite::Transaction<'_>, namespace:&str,author:&str,doc:&VectorDocument)
    -> Result<bool,String>
{
    validate(doc)?;
    let space: Option<(String,i64)> = tx.query_row(
        "SELECT model,dimensions FROM peer_vector_spaces WHERE namespace=?1",
        [namespace],|row|Ok((row.get(0)?,row.get(1)?)),
    ).optional().map_err(|e| e.to_string())?;
    if let Some((model, dimensions))=&space {
        if model != &doc.model || (!doc.deleted && usize::try_from(*dimensions).ok() != Some(doc.vector.len())) {
            return Err("Embedding model or dimension does not match this namespace".into());
        }
    } else {
        if doc.deleted { return Err("Cannot delete from an uninitialized embedding namespace".into()); }
        tx.execute("INSERT INTO peer_vector_spaces(namespace,model,dimensions) VALUES (?1,?2,?3)",
            params![namespace,doc.model,i64::try_from(doc.vector.len()).unwrap_or(0)])
            .map_err(|e| e.to_string())?;
    }
    let previous: Option<(i64, String, String, i64)> = tx.query_row(
        "SELECT revision,text,vector_json,deleted FROM peer_vector_records
         WHERE namespace=?1 AND author=?2 AND record_id=?3",
        params![namespace,author,doc.id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).optional().map_err(|e| e.to_string())?;
    let vector=serde_json::to_string(&doc.vector).map_err(|e| e.to_string())?;
    if let Some((rev,old_text,old_vector,old_deleted))=&previous {
        if doc.revision < *rev as u64 { return Err("Stale memory revision rejected".into()); }
        if doc.revision == *rev as u64 {
            if old_text == &doc.text && old_vector == &vector &&
               (*old_deleted == 1) == doc.deleted { return Ok(false); }
            return Err("Conflicting content for an existing memory revision".into());
        }
    } else {
        let total:i64=tx.query_row("SELECT COUNT(*) FROM peer_vector_records WHERE namespace=?1",
            [namespace],|row|row.get(0)).map_err(|e|e.to_string())?;
        if total >= MAX_RECORDS as i64 { return Err("Memory namespace capacity reached".into()); }
    }
    tx.execute("INSERT INTO peer_vector_records
        (namespace,author,record_id,revision,deleted,text,vector_json)
        VALUES (?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(namespace,author,record_id) DO UPDATE SET
          revision=excluded.revision,deleted=excluded.deleted,
          text=excluded.text,vector_json=excluded.vector_json",
        params![namespace,author,doc.id,doc.revision as i64,i64::from(doc.deleted),doc.text,vector])
        .map_err(|e|e.to_string())?;
    Ok(true)
}
pub fn list(store: &DaemonStore, namespace: &str, local_only: bool) -> Result<Vec<VectorEntry>,String> {
    schema(store)?;
    if !valid_label(namespace) { return Err("Invalid memory namespace".into()); }
    let sql=if local_only {
        "SELECT author,record_id,revision,text,vector_json,deleted,sp.model
         FROM peer_vector_records r JOIN peer_vector_spaces sp ON sp.namespace=r.namespace
         WHERE r.namespace=?1 AND author='@local' ORDER BY record_id LIMIT 129"
    } else {
        "SELECT author,record_id,revision,text,vector_json,deleted,sp.model
         FROM peer_vector_records r JOIN peer_vector_spaces sp ON sp.namespace=r.namespace
         WHERE r.namespace=?1 ORDER BY author,record_id LIMIT 129"
    };
    let mut stmt=store.connection.prepare(sql).map_err(|e|e.to_string())?;
    let rows=stmt.query_map([namespace],|row|{
        let vector_json:String=row.get(4)?;
        Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,
            row.get::<_,i64>(2)?,row.get::<_,String>(3)?,vector_json,
            row.get::<_,i64>(5)?,row.get::<_,String>(6)?))
    }).map_err(|e|e.to_string())?;
    let mut result=Vec::new();
    for row in rows {
        let (author,id,rev,text,vector_json,deleted,model)=row.map_err(|e|e.to_string())?;
        result.push(VectorEntry {author,document:VectorDocument {
            id,revision:rev as u64,model,text,deleted:deleted!=0,
            vector:serde_json::from_str(&vector_json).map_err(|e|e.to_string())?,
        }});
    }
    Ok(result)
}
pub fn search(store: &DaemonStore, namespace: &str, model: &str, vector: &[f32], top_k: usize)
    -> Result<Vec<(f64,VectorEntry)>,String>
{
    if vector.is_empty() || vector.len()>MAX_DIM || top_k==0 || top_k>10 ||
       !valid_model(model) || vector.iter().any(|x| !x.is_finite()) {
        return Err("Invalid bounded vector query".into());
    }
    let needle_norm=vector.iter().map(|v|f64::from(*v).powi(2)).sum::<f64>().sqrt();
    if needle_norm==0.0 { return Err("Zero search vector".into()); }
    let mut scored=Vec::new();
    for entry in list(store,namespace,false)? {
        if entry.document.deleted { continue; }
        if entry.document.model!=model || entry.document.vector.len()!=vector.len() {
            return Err("Embedding model/dimension mismatch during search".into());
        }
        let norm=entry.document.vector.iter().map(|v|f64::from(*v).powi(2)).sum::<f64>().sqrt();
        if norm==0.0 { continue; }
        let dot=entry.document.vector.iter().zip(vector).map(|(a,b)|f64::from(*a)*f64::from(*b)).sum::<f64>();
        scored.push((dot/(norm*needle_norm),entry));
    }
    scored.sort_by(|a,b|b.0.total_cmp(&a.0).then_with(||a.1.author.cmp(&b.1.author))
        .then_with(||a.1.document.id.cmp(&b.1.document.id)));
    scored.truncate(top_k);
    Ok(scored)
}

/// Explicit per-peer autosync opt-in. A disabled row is retained so an
/// operator can see its previous target without silently re-enabling it.
pub fn configure_sync(store:&DaemonStore,alias:&str,namespace:&str,enabled:bool) -> Result<(),String> {
    schema(store)?;
    if !valid_label(alias) || !valid_label(namespace) {
        return Err("Invalid sync peer alias or namespace".into());
    }
    store.connection.execute(
        "INSERT INTO peer_vector_sync_config(alias,namespace,enabled) VALUES (?1,?2,?3)
         ON CONFLICT(alias,namespace) DO UPDATE SET enabled=excluded.enabled",
         params![alias,namespace,i64::from(enabled)],
    ).map_err(|e|e.to_string())?;
    Ok(())
}
pub fn sync_targets(store:&DaemonStore) -> Result<Vec<(String,String)>,String> {
    schema(store)?;
    let mut stmt=store.connection.prepare(
        "SELECT alias,namespace FROM peer_vector_sync_config WHERE enabled=1
         ORDER BY alias,namespace LIMIT 20",
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))
        .map_err(|e|e.to_string())?;
    rows.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())
}

/// Resident service loop: explicitly configured targets only, never guesses a
/// peer or shares a namespace by default. Errors never stop other queue work.
pub fn spawn_sync_worker(paths:super::store::DaemonPaths) {
    tokio::spawn(async move {
        let mut timer=tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            timer.tick().await;
            let targets=match DaemonStore::open(&paths).and_then(|store| {
                if store.kill_switch()? { return Ok(Vec::new()); }
                sync_targets(&store)
            }) {
                Ok(targets)=>targets,
                Err(error)=>{
                    eprintln!("Peer memory sync state unavailable: {error}");
                    continue;
                }
            };
            for (alias,namespace) in targets {
                if let Err(error)=crate::peers_cli::sync_vectors(&paths,&alias,&namespace).await {
                    eprintln!("Peer memory sync for {alias}/{namespace} deferred: {error}");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc(id:&str,revision:u64,text:&str) -> VectorDocument {
        VectorDocument{id:id.into(),revision,model:"embed-model-v1".into(),text:text.into(),
            vector:vec![0.3,0.4,0.5],deleted:false}
    }
    #[test]
    fn approved_shares_vectors_and_sync_opt_in_survive_daemon_restart() {
        let root=std::env::temp_dir().join(format!("peer-memory-restart-{}",uuid::Uuid::new_v4()));
        let paths=super::super::store::DaemonPaths::under(&root);
        {
            let mut store=DaemonStore::open(&paths).unwrap();
            grant(&store,"paired-device-a","team-review",true,false).unwrap();
            grant(&store,"paired-device-b","team-review",true,true).unwrap();
            grant(&store,"paired-device-b","private",false,false).unwrap();
            configure_sync(&store,"outbound-a","team-review",true).unwrap();
            configure_sync(&store,"outbound-b","team-review",false).unwrap();
            assert!(put(&mut store,"team-review",LOCAL_AUTHOR,&doc("finding",1,"before restart")).unwrap());
        }
        // A new process opens the authoritative SQLite state. No JS state,
        // cached pairing list or in-memory vector index is involved.
        {
            let store=DaemonStore::open(&paths).unwrap();
            assert!(authorized(&store,"paired-device-a","team-review",false).unwrap());
            assert!(!authorized(&store,"paired-device-a","team-review",true).unwrap());
            assert!(authorized(&store,"paired-device-b","team-review",true).unwrap());
            assert!(!authorized(&store,"paired-device-b","private",false).unwrap());
            assert!(!authorized(&store,"paired-device-a","another-namespace",false).unwrap());
            assert_eq!(sync_targets(&store).unwrap(),vec![("outbound-a".into(),"team-review".into())]);
            assert_eq!(list(&store,"team-review",true).unwrap()[0].document.text,"before restart");
        }
        {
            let mut store=DaemonStore::open(&paths).unwrap();
            // Recover the same revision unchanged, then delete at rev 2.
            assert!(!put(&mut store,"team-review",LOCAL_AUTHOR,
                &doc("finding",1,"before restart")).unwrap());
            let mut tomb=doc("finding",2,"");
            tomb.text.clear(); tomb.vector.clear(); tomb.deleted=true;
            assert!(put(&mut store,"team-review",LOCAL_AUTHOR,&tomb).unwrap());
            configure_sync(&store,"outbound-a","team-review",false).unwrap();
        }
        let store=DaemonStore::open(&paths).unwrap();
        assert!(sync_targets(&store).unwrap().is_empty());
        assert!(list(&store,"team-review",true).unwrap()[0].document.deleted);
        assert!(search(&store,"team-review","embed-model-v1",&[0.3,0.4,0.5],5).unwrap().is_empty());
        drop(store);
        let _=std::fs::remove_dir_all(root);
    }

    #[test]
    fn standard_2048_dimension_embedding_fits_signed_transport_and_is_searchable() {
        let mut store=DaemonStore::open_in_memory().unwrap();
        let vector=vec![0.01234568_f32;2048];
        let record=VectorDocument {
            id:"full-sized".into(),revision:1,model:"embed-2048".into(),
            text:"A verified project fact".into(),vector:vector.clone(),deleted:false,
        };
        let bytes=serde_json::to_vec(&record).unwrap();
        assert!(bytes.len()>16*1024,"regression fixture exercises the prior 16KiB cap");
        assert!(bytes.len()<=MAX_WIRE_DOCUMENT_BYTES);
        assert!(put(&mut store,"project",LOCAL_AUTHOR,&record).unwrap());
        let hits=search(&store,"project","embed-2048",&vector,3).unwrap();
        assert_eq!(hits.len(),1);
        assert_eq!(hits[0].1.document.id,"full-sized");
    }

    #[test]
    fn an_alias_change_or_repairing_cannot_impersonate_a_previous_vector_author() {
        let old=remote_author(&"a".repeat(64)).unwrap();
        let new=remote_author(&"b".repeat(64)).unwrap();
        assert_ne!(old,new);
        assert_eq!(remote_author(&"A".repeat(64)).unwrap(),old);
        assert!(remote_author("user-alias").is_err());
        let mut store=DaemonStore::open_in_memory().unwrap();
        put(&mut store,"team",&old,&doc("review",1,"original")).unwrap();
        put(&mut store,"team",&new,&doc("review",1,"new installation")).unwrap();
        assert_eq!(list(&store,"team",false).unwrap().len(),2);
        assert!(put(&mut store,"team",&new,&doc("review",2,"separate stream")).unwrap());
        assert_eq!(list(&store,"team",false).unwrap().iter()
            .find(|entry|entry.author==old).unwrap().document.text,"original");
    }

    #[test]
    fn peer_snapshot_is_atomic_if_a_later_record_conflicts_or_is_malformed() {
        let mut store=DaemonStore::open_in_memory().unwrap();
        let first=doc("first",1,"accepted only with full batch");
        let mut invalid=doc("second",1,"wrong embedding model");
        invalid.model="incompatible-model".into();
        let failure=put_batch(&mut store,"shared","paired-alice",&[first.clone(),invalid]);
        assert!(failure.unwrap_err().contains("Embedding model"));
        assert!(list(&store,"shared",false).unwrap().is_empty(),
            "one rejected record must roll back earlier records and namespace metadata");
        assert_eq!(put_batch(&mut store,"shared","paired-alice",&[first.clone()]).unwrap(),1);
        assert_eq!(put_batch(&mut store,"shared","paired-alice",&[first.clone()]).unwrap(),0);
        let mut same_revision=first.clone();
        same_revision.text="conflicting same-version payload".into();
        assert!(put_batch(&mut store,"shared","paired-alice", &[doc("new",1,"new"),same_revision]).is_err());
        assert_eq!(list(&store,"shared",false).unwrap().len(),1,
            "a conflicting revision cannot partially import a peer snapshot");
        assert!(put_batch(&mut store,"shared","paired-alice",&[first.clone(),first])
            .unwrap_err().contains("Duplicate"));
        let mut newer=doc("first",2,"revised");
        assert_eq!(put_batch(&mut store,"shared","paired-alice",&[newer.clone()]).unwrap(),1);
        newer.deleted=true;newer.revision=3;newer.vector.clear();newer.text.clear();
        assert_eq!(put_batch(&mut store,"shared","paired-alice",&[newer]).unwrap(),1);
        assert!(search(&store,"shared","embed-model-v1",&[0.3,0.4,0.5],5).unwrap().is_empty());
    }

    #[test]
    fn partitioned_multiwriter_revisions_converge_without_confusing_writer_identities() {
        // Simulate two independently persisted installation databases whose
        // network is unavailable for several revision epochs. This checks
        // conflict resolution and capacity under load; real TLS partitions
        // remain part of operator-owned two-host acceptance.
        let mut a=DaemonStore::open_in_memory().unwrap();
        let mut b=DaemonStore::open_in_memory().unwrap();
        let author_a=remote_author(&"a".repeat(64)).unwrap();
        let author_b=remote_author(&"b".repeat(64)).unwrap();
        let first_a=(0..48).map(|i|doc(&format!("a-{i:02}"),1,"origin A"))
            .collect::<Vec<_>>();
        let first_b=(0..48).map(|i|doc(&format!("b-{i:02}"),1,"origin B"))
            .collect::<Vec<_>>();
        assert_eq!(put_batch(&mut a,"partition",LOCAL_AUTHOR,&first_a).unwrap(),48);
        assert_eq!(put_batch(&mut b,"partition",LOCAL_AUTHOR,&first_b).unwrap(),48);
        // Both peers were offline and then recover. The transport converts
        // each publisher's @local stream to its pinned certificate identity.
        assert_eq!(put_batch(&mut b,"partition",&author_a,&first_a).unwrap(),48);
        assert_eq!(put_batch(&mut a,"partition",&author_b,&first_b).unwrap(),48);
        assert_eq!(list(&a,"partition",false).unwrap().len(),96);
        assert_eq!(list(&b,"partition",false).unwrap().len(),96);
        assert_eq!(put_batch(&mut b,"partition",&author_a,&first_a).unwrap(),0);
        assert_eq!(put_batch(&mut a,"partition",&author_b,&first_b).unwrap(),0);
        // Another disconnected interval: independent revisions, including
        // deletions. Deliver the batches in both directions after reconnect.
        let updates_a=(0..16).map(|i|{
            let mut item=doc(&format!("a-{i:02}"),2,"amended on A");
            if i<8 {item.revision=3;item.deleted=true;item.text.clear();item.vector.clear();}
            item
        }).collect::<Vec<_>>();
        let updates_b=(0..16).map(|i|{
            let mut item=doc(&format!("b-{i:02}"),2,"amended on B");
            if i<8 {item.revision=3;item.deleted=true;item.text.clear();item.vector.clear();}
            item
        }).collect::<Vec<_>>();
        assert_eq!(put_batch(&mut a,"partition",LOCAL_AUTHOR,&updates_a).unwrap(),16);
        assert_eq!(put_batch(&mut b,"partition",LOCAL_AUTHOR,&updates_b).unwrap(),16);
        assert_eq!(put_batch(&mut a,"partition",&author_b,&updates_b).unwrap(),16);
        assert_eq!(put_batch(&mut b,"partition",&author_a,&updates_a).unwrap(),16);
        assert_eq!(put_batch(&mut a,"partition",&author_b,&updates_b).unwrap(),0);
        assert_eq!(put_batch(&mut b,"partition",&author_a,&updates_a).unwrap(),0);
        let records_a=list(&a,"partition",false).unwrap();
        let records_b=list(&b,"partition",false).unwrap();
        assert_eq!(records_a.len(),96);
        assert_eq!(records_b.len(),96);
        for (left,right) in records_a.iter().zip(&records_b) {
            assert_eq!(left.author,right.author);
            assert_eq!(left.document,right.document);
        }
        assert_eq!(records_a.iter().filter(|item|item.document.deleted).count(),16);
        let mut old=doc("a-00",1,"stale replay");
        assert!(put_batch(&mut b,"partition",&author_a,&[old.clone()]).is_err());
        old.revision=3;
        assert!(put_batch(&mut b,"partition",&author_a,&[old]).is_err(),
            "same revision may never resurrect a tombstone");
        assert_eq!(search(&a,"partition","embed-model-v1",&[0.3,0.4,0.5],10)
            .unwrap().len(),10);
    }

    #[test]
    fn durable_vector_revisions_and_tombstones_cannot_be_replayed_or_cross_models() {
        let mut store=DaemonStore::open_in_memory().unwrap();
        grant(&store,"paired-alice","shared",true,false).unwrap();
        assert!(authorized(&store,"paired-alice","shared",false).unwrap());
        assert!(!authorized(&store,"paired-alice","shared",true).unwrap());
        assert!(!authorized(&store,"paired-bob","shared",false).unwrap());
        assert!(put(&mut store,"shared",LOCAL_AUTHOR,&doc("r",1,"original")).unwrap());
        assert!(!put(&mut store,"shared",LOCAL_AUTHOR,&doc("r",1,"original")).unwrap());
        assert!(put(&mut store,"shared","paired-alice",&doc("r",1,"foreign")).unwrap());
        assert_eq!(list(&store,"shared",false).unwrap().len(),2);
        assert_eq!(list(&store,"shared",true).unwrap().len(),1);
        assert!(put(&mut store,"shared",LOCAL_AUTHOR,&doc("r",1,"tampered")).is_err());
        assert!(put(&mut store,"shared",LOCAL_AUTHOR,&doc("r",0,"old")).is_err());
        let mut bad=doc("other",1,"invalid");bad.model="another".into();
        assert!(put(&mut store,"shared",LOCAL_AUTHOR,&bad).is_err());
        let mut tomb=doc("r",2,"");tomb.deleted=true;tomb.vector.clear();
        assert!(put(&mut store,"shared",LOCAL_AUTHOR,&tomb).unwrap());
        assert_eq!(search(&store,"shared","embed-model-v1",&[0.3,0.4,0.5],5).unwrap().len(),1);
        assert!(put(&mut store,"shared",LOCAL_AUTHOR,&doc("r",1,"reanimated")).is_err());
    }
}
