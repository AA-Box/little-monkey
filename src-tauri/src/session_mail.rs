//! Durable, per-profile, local SQLite mailbox for ordinary chat sessions.
//!
//! Messages are untrusted text, never tool calls. WAL + an IMMEDIATE write
//! transaction atomically deduplicates retries across separate processes.
//! Reading does not consume: callers must first persist the recipient's
//! displayed transcript and only then acknowledge. Lost acknowledgements
//! redeliver the same ID, allowing an idempotent consumer to settle it.
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

const MAX_TEXT: usize = 1600;
const MAX_PENDING: i64 = 1000;
const MAX_BATCH: usize = 100;
const MAX_AGE_MS: i64 = 90 * 24 * 3600 * 1000;
const PEER_STALE_MS:i64=60 * 60 * 1000;
const MAX_PEER_LIST:usize=24;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mail {
    pub id: String,
    pub sender_session_id: String,
    pub recipient_session_id: String,
    pub workspace: String,
    pub text: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPeer {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailReceipt {
    pub message_id: String,
    pub recipient_session_id: String,
    pub persisted: bool,
    pub already_existed: bool,
}

fn now_ms() -> Result<i64, String> {
    let ms=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_err(|e|e.to_string())?.as_millis();
    i64::try_from(ms).map_err(|_|"System time exceeds mailbox range".into())
}
fn valid_id(s:&str)->bool{
    !s.is_empty() && s.len()<=160 &&
    s.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'_'|b'-'|b'.'|b':'))
}
fn open(path:&Path)->Result<Connection,String>{
    let folder=path.parent().ok_or("Mailbox has no directory")?;
    std::fs::create_dir_all(folder).map_err(|e|e.to_string())?;
    let db=Connection::open(path).map_err(|e|e.to_string())?;
    db.busy_timeout(Duration::from_secs(8)).map_err(|e|e.to_string())?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS local_session_peers (
           session_id TEXT PRIMARY KEY NOT NULL,
           workspace TEXT NOT NULL,
           title TEXT NOT NULL,
           updated_at_ms INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS local_session_peers_workspace
           ON local_session_peers(workspace,updated_at_ms);
         CREATE TABLE IF NOT EXISTS local_session_mail (
           id TEXT PRIMARY KEY NOT NULL,
           sender_session_id TEXT NOT NULL,
           recipient_session_id TEXT NOT NULL,
           workspace TEXT NOT NULL,
           text TEXT NOT NULL,
           created_at_ms INTEGER NOT NULL,
           delivered_at_ms INTEGER
         );
         CREATE INDEX IF NOT EXISTS local_session_mail_pending
           ON local_session_mail(recipient_session_id,workspace,delivered_at_ms,created_at_ms);
         CREATE INDEX IF NOT EXISTS local_session_mail_retention
           ON local_session_mail(delivered_at_ms);
        "
    ).map_err(|e|e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta=std::fs::metadata(path).map_err(|e|e.to_string())?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o600))
                .map_err(|e|e.to_string())?;
        }
    }
    Ok(db)
}

/// Presence is a profile-local, explicit-opt-in registration by a desktop
/// session already attached to a concrete workspace. It does NOT grant a
/// model prompt or mutate the recipient's chat.
pub fn register_impl(path:&Path,id:&str,workspace:&str,title:&str)
    ->Result<(),String>{
    if !valid_id(id)||workspace.trim().is_empty()||workspace.len()>4096||
        title.len()>200 {return Err("Invalid local peer registration".into());}
    let db=open(path)?;
    db.execute("INSERT INTO local_session_peers(session_id,workspace,title,updated_at_ms)
        VALUES(?1,?2,?3,?4)
        ON CONFLICT(session_id) DO UPDATE SET workspace=excluded.workspace,
            title=excluded.title,updated_at_ms=excluded.updated_at_ms",
        params![id,workspace,title,now_ms()?]).map_err(|e|e.to_string())?;
    Ok(())
}

/// Cross-process discovery. Only recent same-profile, same-workspace peers
/// appear. No network server or untrusted workspace enumeration is created.
pub fn list_peers_impl(path:&Path,requester:&str,workspace:&str)
    ->Result<Vec<SessionPeer>,String>{
    if !valid_id(requester)||workspace.trim().is_empty()||workspace.len()>4096 {
        return Err("Invalid local peer lookup".into());
    }
    let db=open(path)?;
    let now=now_ms()?;
    let requester_matches:i64=db.query_row(
        "SELECT COUNT(*) FROM local_session_peers WHERE session_id=?1
           AND workspace=?2 AND updated_at_ms>=?3",
        params![requester,workspace,now.saturating_sub(PEER_STALE_MS)],
        |row|row.get(0)).map_err(|e|e.to_string())?;
    if requester_matches!=1{return Err("Sender is not enrolled in this workspace".into());}
    let mut stmt=db.prepare(
        "SELECT session_id,title,workspace,updated_at_ms FROM local_session_peers
           WHERE session_id!=?1 AND workspace=?2 AND updated_at_ms>=?3
           ORDER BY updated_at_ms DESC,session_id LIMIT ?4"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![requester,workspace,
            now.saturating_sub(PEER_STALE_MS),MAX_PEER_LIST as i64],
        |r|Ok(SessionPeer{
            id:r.get(0)?,title:r.get(1)?,workspace:r.get(2)?,updated_at_ms:r.get(3)?
        })).map_err(|e|e.to_string())?;
    rows.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())
}

/// Durable, idempotent send. Reusing a tool identity with changed recipients,
/// workspace or text fails rather than disguising a second send as a replay.
pub fn send_impl(path:&Path, sender:&str, recipient:&str, workspace:&str,
    turn_id:&str, tool_call_id:&str, body:&str)->Result<MailReceipt,String>{
    if !valid_id(sender)||!valid_id(recipient)||!valid_id(turn_id)||
        !valid_id(tool_call_id)||sender==recipient{
        return Err("Invalid or self-referential local session identity".into());
    }
    if workspace.trim().is_empty() || workspace.len()>4096 || body.trim().is_empty() ||
        body.len()>MAX_TEXT || body.contains('\0'){
        return Err("Invalid workspace or bounded peer message (max 1600 bytes)".into());
    }
    let id=format!("{turn_id}:{tool_call_id}");
    if id.len()>330{return Err("Peer message identity too long".into());}
    let mut db=open(path)?;
    let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e|e.to_string())?;
    // Already-acknowledged 90-day history can be discarded, but unread mail
    // must never expire without explicit user acknowledgement.
    tx.execute("DELETE FROM local_session_mail
        WHERE delivered_at_ms IS NOT NULL AND delivered_at_ms<?1",
        params![now_ms()?.saturating_sub(MAX_AGE_MS)])
        .map_err(|e|e.to_string())?;
    // Both parties must be registered by the desktop profile in the same
    // workspace. Another process may discover them, but a model-supplied
    // arbitrary session ID alone is not authority to create an inbox.
    for party in [sender,recipient] {
        let accepted:i64=tx.query_row(
            "SELECT COUNT(*) FROM local_session_peers
             WHERE session_id=?1 AND workspace=?2 AND updated_at_ms>=?3",
            params![party,workspace,now_ms()?.saturating_sub(PEER_STALE_MS)],
            |r|r.get(0)).map_err(|e|e.to_string())?;
        if accepted!=1 {return Err("Sender or recipient is not an enrolled local peer".into());}
    }
    let present:Option<(String,String,String,String)>=tx.query_row(
        "SELECT sender_session_id,recipient_session_id,workspace,text
         FROM local_session_mail WHERE id=?1",
        params![id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
    ).optional().map_err(|e|e.to_string())?;
    let already_existed=present.is_some();
    if let Some(previous)=present {
        if previous != (sender.to_owned(),recipient.to_owned(),workspace.to_owned(),body.to_owned()){
            return Err("Idempotency identity reused with a different message".into());
        }
    }else{
        let count:i64=tx.query_row(
            "SELECT COUNT(*) FROM local_session_mail
             WHERE recipient_session_id=?1 AND delivered_at_ms IS NULL",
            params![recipient],|r|r.get(0)).map_err(|e|e.to_string())?;
        if count>=MAX_PENDING{return Err("Recipient inbox full; no message persisted".into());}
        tx.execute("INSERT INTO local_session_mail
             (id,sender_session_id,recipient_session_id,workspace,text,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![id,sender,recipient,workspace,body,now_ms()?])
            .map_err(|e|e.to_string())?;
    }
    tx.commit().map_err(|e|e.to_string())?;
    Ok(MailReceipt{
        message_id:id,recipient_session_id:recipient.to_owned(),
        persisted:true,already_existed
    })
}

/// No lease/claim: reading never makes a message disappear. Acknowledgment
/// occurs only AFTER successfully saving the recipient transcript.
pub fn pending_impl(path:&Path,recipient:&str,workspace:&str,limit:usize)
    ->Result<Vec<Mail>,String>{
    if !valid_id(recipient)||workspace.trim().is_empty()||workspace.len()>4096||
        limit==0||limit>MAX_BATCH{
        return Err("Invalid mailbox recipient, workspace or batch limit".into());
    }
    let db=open(path)?;
    let mut stmt=db.prepare("SELECT id,sender_session_id,recipient_session_id,workspace,text,
        created_at_ms FROM local_session_mail
        WHERE recipient_session_id=?1 AND workspace=?2 AND delivered_at_ms IS NULL
        ORDER BY created_at_ms,id LIMIT ?3").map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![recipient,workspace,limit as i64],|r|Ok(Mail{
        id:r.get(0)?,sender_session_id:r.get(1)?,recipient_session_id:r.get(2)?,
        workspace:r.get(3)?,text:r.get(4)?,created_at_ms:r.get(5)?,
    })).map_err(|e|e.to_string())?;
    rows.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())
}

/// Idempotent ack, additionally scoped to recipient and workspace. Unknown or
/// changed recipient identities never consume the message.
pub fn ack_impl(path:&Path,recipient:&str,workspace:&str,id:&str)->Result<bool,String>{
    if !valid_id(recipient)||id.len()>330||workspace.trim().is_empty(){
        return Err("Invalid acknowledgement identity".into());
    }
    let db=open(path)?;
    let changed=db.execute("UPDATE local_session_mail SET delivered_at_ms=?4
      WHERE id=?1 AND recipient_session_id=?2 AND workspace=?3
        AND delivered_at_ms IS NULL",params![id,recipient,workspace,now_ms()?])
        .map_err(|e|e.to_string())?;
    if changed>0{return Ok(true);}
    let known:Option<i64>=db.query_row("SELECT delivered_at_ms FROM local_session_mail
      WHERE id=?1 AND recipient_session_id=?2 AND workspace=?3",
      params![id,recipient,workspace],|r|r.get(0))
      .optional().map_err(|e|e.to_string())?;
    Ok(known.is_some())
}

/// Only acknowledged old records may be pruned; an unread message does not
/// vanish silently on timer expiration or daemon process restart.
pub fn prune_acknowledged_impl(path:&Path)->Result<usize,String>{
    let db=open(path)?;
    db.execute("DELETE FROM local_session_mail
       WHERE delivered_at_ms IS NOT NULL AND delivered_at_ms<?1",
       params![now_ms()?.saturating_sub(MAX_AGE_MS)])
       .map_err(|e|e.to_string())
}

fn profile_path(app:&tauri::AppHandle)->Result<std::path::PathBuf,String>{
    use crate::profiles::ProfileScopedPaths;
    let root=app.profile_data_dir().map_err(|e|e.to_string())?;
    Ok(root.join("local_session_mail.sqlite"))
}

#[tauri::command]
pub fn session_mail_register(app:tauri::AppHandle,session_id:String,
    workspace:String,title:String)->Result<(),String>{
    register_impl(&profile_path(&app)?,&session_id,&workspace,&title)
}
#[tauri::command]
pub fn session_mail_peers(app:tauri::AppHandle,sender_session_id:String,
    workspace:String)->Result<Vec<SessionPeer>,String>{
    list_peers_impl(&profile_path(&app)?,&sender_session_id,&workspace)
}

#[tauri::command]
pub fn session_mail_send(app:tauri::AppHandle,sender_session_id:String,
    recipient_session_id:String,workspace:String,turn_id:String,
    tool_call_id:String,text:String)->Result<MailReceipt,String>{
    send_impl(&profile_path(&app)?,&sender_session_id,&recipient_session_id,
        &workspace,&turn_id,&tool_call_id,&text)
}
#[tauri::command]
pub fn session_mail_pending(app:tauri::AppHandle,recipient_session_id:String,
    workspace:String,limit:usize)->Result<Vec<Mail>,String>{
    pending_impl(&profile_path(&app)?,&recipient_session_id,&workspace,limit)
}
#[tauri::command]
pub fn session_mail_ack(app:tauri::AppHandle,recipient_session_id:String,
    workspace:String,message_id:String)->Result<bool,String>{
    ack_impl(&profile_path(&app)?,&recipient_session_id,&workspace,&message_id)
}

#[cfg(test)]
mod tests{
    use super::*;
    use std::path::PathBuf;
    fn tmp()->PathBuf{
        let root=std::env::temp_dir().join(format!("lm-mail-{}",uuid::Uuid::new_v4()));
        root.join("box.sqlite")
    }
    #[test]fn durable_ack_after_restart_and_failed_replays(){
        let path=tmp();
        register_impl(&path,"from","/project","Sender").unwrap();
        register_impl(&path,"to","/project","Target").unwrap();
        let first=send_impl(&path,"from","to","/project","turn1","tool1","hello").unwrap();
        assert!(first.persisted && !first.already_existed);
        let retried=send_impl(&path,"from","to","/project","turn1","tool1","hello").unwrap();
        assert!(retried.already_existed);
        assert!(send_impl(&path,"from","to","/project","turn1","tool1","altered").is_err());
        // New Connection after writer has dropped: process restart durable state.
        let got=pending_impl(&path,"to","/project",10).unwrap();
        assert_eq!(got.len(),1);
        assert_eq!(got[0].text,"hello");
        assert!(pending_impl(&path,"to","/another",10).unwrap().is_empty());
        assert!(!ack_impl(&path,"else","/project",&first.message_id).unwrap());
        assert!(ack_impl(&path,"to","/project",&first.message_id).unwrap());
        assert!(ack_impl(&path,"to","/project",&first.message_id).unwrap());
        assert!(pending_impl(&path,"to","/project",10).unwrap().is_empty());
        drop(got);let _=std::fs::remove_dir_all(path.parent().unwrap());
    }
    #[test]fn serialized_ipc_contract_matches_desktop_consumer(){
        let peer=SessionPeer{id:"agent-1".into(),title:"Test".into(),
            workspace:"/root".into(),updated_at_ms:123};
        let json=serde_json::to_value(&peer).unwrap();
        assert_eq!(json["updatedAtMs"],123);
        assert_eq!(json["workspace"],"/root");
        let mail=Mail{id:"turn:tool".into(),sender_session_id:"a".into(),
            recipient_session_id:"b".into(),workspace:"/root".into(),
            text:"Hello".into(),created_at_ms:123};
        let json=serde_json::to_value(&mail).unwrap();
        assert_eq!(json["senderSessionId"],"a");
        assert_eq!(json["recipientSessionId"],"b");
        assert_eq!(json["createdAtMs"],123);
        let receipt=MailReceipt{message_id:"turn:tool".into(),
            recipient_session_id:"b".into(),persisted:true,already_existed:false};
        let json=serde_json::to_value(&receipt).unwrap();
        assert_eq!(json["messageId"],"turn:tool");
        assert_eq!(json["alreadyExisted"],false);
    }
    #[test]fn exact_workspace_peer_directory_and_stale_enrolment_refusal(){
        let path=tmp();
        register_impl(&path,"one","/shared","One").unwrap();
        assert!(list_peers_impl(&path,"one","/shared").unwrap().is_empty());
        register_impl(&path,"two","/shared","Two").unwrap();
        register_impl(&path,"third","/elsewhere","Third").unwrap();
        let found=list_peers_impl(&path,"one","/shared").unwrap();
        assert_eq!(found.len(),1);
        assert_eq!(found[0].id,"two");
        assert!(list_peers_impl(&path,"third","/shared").is_err());
        assert!(send_impl(&path,"one","third","/shared","try","call","wrong").is_err());
        let db=open(&path).unwrap();
        db.execute("UPDATE local_session_peers SET updated_at_ms=0
            WHERE session_id='two'",[]).unwrap();
        assert!(send_impl(&path,"one","two","/shared","try","call","stale").is_err());
        register_impl(&path,"two","/shared","Two").unwrap();
        assert!(send_impl(&path,"one","two","/shared","try","call","now live").unwrap().persisted);
        assert_eq!(pending_impl(&path,"two","/shared",10).unwrap().len(),1);
        drop(db);
        let _=std::fs::remove_dir_all(path.parent().unwrap());
    }
    #[test]fn concurrent_process_safe_exactly_one_inbox_record(){
        let path=tmp();
        register_impl(&path,"from","/project","Sender").unwrap();
        register_impl(&path,"to","/project","Target").unwrap();
        let handles:Vec<_>=(0..12).map(|_|{
            let p=path.clone();
            std::thread::spawn(move||{
                send_impl(&p,"from","to","/project","one","call","safe")
            })
        }).collect();
        let mut inserted=0;
        for handle in handles {
            let r=handle.join().unwrap().unwrap();
            if !r.already_existed {inserted+=1;}
        }
        assert_eq!(inserted,1);
        assert_eq!(pending_impl(&path,"to","/project",10).unwrap().len(),1);
        let _=std::fs::remove_dir_all(path.parent().unwrap());
    }
    #[test]fn child_writer_entry(){
        let Ok(path)=std::env::var("LITTLE_MONKEY_TEST_MAIL_CHILD") else{return};
        register_impl(Path::new(&path),"child","/acceptance","Child").unwrap();
        register_impl(Path::new(&path),"parent","/acceptance","Parent").unwrap();
        send_impl(Path::new(&path),"child","parent","/acceptance","turn","call","after reboot")
            .unwrap();
    }
    #[test]fn concurrent_independent_os_processes_replay_same_send_only_once(){
        let path=tmp();
        let mut children=Vec::new();
        for _ in 0..4 {
            children.push(std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact").arg("session_mail::tests::child_writer_entry")
                .env("LITTLE_MONKEY_TEST_MAIL_CHILD",&path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn().unwrap());
        }
        for mut child in children {
            assert!(child.wait().unwrap().success(),
                "Independent sender must successfully commit or replay");
        }
        let rows=pending_impl(&path,"parent","/acceptance",10).unwrap();
        assert_eq!(rows.len(),1,"Four distinct processes must commit one message");
        assert_eq!(rows[0].text,"after reboot");
        let _=std::fs::remove_dir_all(path.parent().unwrap());
    }
    #[test]fn distinct_os_process_can_send_and_parent_can_ack(){
        let path=tmp();
        let status=std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact").arg("session_mail::tests::child_writer_entry")
            .arg("--nocapture")
            .env("LITTLE_MONKEY_TEST_MAIL_CHILD",&path)
            .status().unwrap();
        assert!(status.success());
        let items=pending_impl(&path,"parent","/acceptance",10).unwrap();
        assert_eq!(items.len(),1,"writer must run in a different OS process");
        assert!(ack_impl(&path,"parent","/acceptance",&items[0].id).unwrap());
        assert!(pending_impl(&path,"parent","/acceptance",10).unwrap().is_empty());
        let _=std::fs::remove_dir_all(path.parent().unwrap());
    }
}
