//! Turning what happens to a run into a notification on someone's phone.
//!
//! **Why a watcher and not a call site.** A run's state changes in several
//! places — the scheduler admits it, a child process reports it, a reconciler
//! repairs it after a crash — and every one of those is a place a notification
//! could be forgotten. Reading the state the daemon already durably records
//! means a transition raises its notification no matter which code path caused
//! it, including the ones that happen while nothing is looking.
//!
//! **Edges, not levels.** The daemon store is polled, so the same
//! `waiting_approval` job is seen on every tick. `remote_push_watch` records
//! the last state a device was told about, so a phone is woken once per
//! transition rather than once per poll — and a daemon restart does not re-send
//! what was already sent.
//!
//! **Nothing private travels.** The payload is a [`PushKind`] and an id; the
//! run's text, its prompt and its output stay where they are and are read back
//! over a signed request once someone unlocks the device.

use std::time::Duration;

use super::push::{notify_all, PushKind, PushNotification};
use super::store::{KeyringRemoteSecrets, RemoteStore};
use crate::daemon::store::{DaemonPaths, DaemonStore, JobState};

/// How often the daemon's job table is read. A notification is not worth a
/// tighter loop than this, and every tick is one small indexed query.
const TICK: Duration = Duration::from_secs(2);
/// How long a notified-state row is kept. Comfortably longer than any run this
/// watcher could still be about to see finish.
const WATCH_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
/// Bound on one tick's work, so a machine that has just replayed a thousand
/// jobs does not send a thousand notifications.
const BATCH: u32 = 64;

/// What a device is told about this state, if anything.
///
/// Deliberately partial: `queued`, `running` and `paused` raise nothing. A
/// notification is for something a person has to do or would want to know, and
/// "your run is still running" is neither.
fn kind_for(state: JobState) -> Option<PushKind> {
    match state {
        JobState::WaitingApproval => Some(PushKind::ApprovalRequested),
        JobState::Succeeded => Some(PushKind::RunCompleted),
        JobState::Failed => Some(PushKind::RunFailed),
        _ => None,
    }
}

/// Starts the watcher. Never fails the daemon: a machine with no paired device
/// simply has nothing to notify, and this loop keeps costing one query a tick
/// either way.
pub fn spawn(paths: DaemonPaths) {
    tokio::spawn(async move {
        let mut since_ms = now_ms();
        loop {
            tokio::time::sleep(TICK).await;
            match tick(&paths, since_ms, now_ms()).await {
                Ok(next) => since_ms = next,
                Err(error) => {
                    // Logged, never fatal. A watcher that exits on a transient
                    // database lock is a watcher that silently stops notifying.
                    eprintln!("run notification watcher: {error}");
                }
            }
        }
    });
}

/// One sweep. Returns the cursor for the next one.
///
/// Reads two sets: jobs that changed since the last sweep, and every job that
/// is currently active. The second is what makes a restart correct — an
/// approval that has been waiting since before this process started is still
/// waiting, and is still worth one notification.
pub async fn tick(paths: &DaemonPaths, since_ms: u64, now_ms: u64) -> Result<u64, String> {
    let daemon = DaemonStore::open(paths)?;
    let mut changed = daemon.jobs_updated_since(since_ms, BATCH)?;
    changed.extend(daemon.active_jobs()?);
    drop(daemon);

    let cursor = latest(&changed, since_ms);
    let observed = changed
        .iter()
        .map(|job| (job.job_id.clone(), job.state, job.run_id.clone()))
        .collect::<Vec<_>>();
    let pending = {
        let mut store = RemoteStore::open(&paths.root)?;
        // The other periodic sweep that needs doing whether or not any device
        // is currently connected: a microphone left open by a phone that
        // vanished is closed by the runner's own clock, not by the next lease.
        super::voice::expire(&mut store, now_ms)?;
        pending_notifications(&mut store, &observed, now_ms)?
    };

    // Sent outside the store borrow: delivery reaches a push service over the
    // network, and holding a SQLite connection open across it would block the
    // daemon's own writes for as long as that takes.
    for (kind, run_id) in pending {
        let _ = notify_all(
            paths,
            &PushNotification {
                kind,
                target_id: run_id,
                detail: None,
            },
            &KeyringRemoteSecrets,
        )
        .await;
    }
    Ok(cursor)
}

/// Decides which of the observed job states are worth waking a device for, and
/// records that decision.
///
/// Split out from [`tick`] so the *decision* — which is all the interesting
/// behaviour — is testable without a push service, a keychain or a phone.
/// Delivery is `push`'s business and is tested there.
pub fn pending_notifications(
    store: &mut RemoteStore,
    observed: &[(String, JobState, Option<String>)],
    now_ms: u64,
) -> Result<Vec<(PushKind, Option<String>)>, String> {
    // Nothing is reachable, so nothing is worth deciding. This is also what
    // keeps a machine with no paired phone from touching the keychain every
    // couple of seconds.
    if store.push_registrations()?.is_empty() {
        return Ok(Vec::new());
    }
    let chat_jobs = mobile_chat_job_ids(store, now_ms)?;
    let mut pending = Vec::new();
    for (job_id, state, run_id) in observed {
        let Some(mut kind) = kind_for(*state) else {
            continue;
        };
        // A finished chat turn is a reply someone is waiting to read, not a
        // "run finished" they have to go and interpret.
        if kind == PushKind::RunCompleted && chat_jobs.contains(job_id) {
            kind = PushKind::NewResponse;
        }
        if store.mark_push_notified(job_id, state.token(), now_ms)? {
            pending.push((kind, run_id.clone()));
        }
    }
    store.prune_push_watch(now_ms.saturating_sub(WATCH_RETENTION_MS))?;
    Ok(pending)
}

fn latest(jobs: &[crate::daemon::store::DaemonJob], floor: u64) -> u64 {
    jobs.iter()
        .map(|job| job.updated_at_ms)
        .fold(floor, u64::max)
}

/// The job ids that belong to recent mobile chat turns.
///
/// Derived forwards from the message ids rather than parsed out of the job id,
/// because the job id is a digest and cannot be inverted. Bounded to the recent
/// past: an older chat turn has long since finished.
fn mobile_chat_job_ids(
    store: &RemoteStore,
    now_ms: u64,
) -> Result<std::collections::HashSet<String>, String> {
    let since = now_ms.saturating_sub(6 * 60 * 60 * 1_000);
    Ok(store
        .recent_mobile_message_ids(since, 256)?
        .iter()
        .map(|message_id| crate::daemon::mobile_chat_job_id(message_id))
        .collect())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The states that wake a phone, and the ones that deliberately do not.
    #[test]
    fn only_states_a_person_would_act_on_raise_a_notification() {
        assert_eq!(
            kind_for(JobState::WaitingApproval),
            Some(PushKind::ApprovalRequested)
        );
        assert_eq!(kind_for(JobState::Succeeded), Some(PushKind::RunCompleted));
        assert_eq!(kind_for(JobState::Failed), Some(PushKind::RunFailed));
        for quiet in [
            JobState::Queued,
            JobState::Preparing,
            JobState::Running,
            JobState::Paused,
            JobState::Cancelling,
            JobState::Cancelled,
        ] {
            assert_eq!(
                kind_for(quiet),
                None,
                "{quiet:?} would wake a phone for nothing"
            );
        }
    }

    /// The edge-not-level rule, which is the whole reason the watch table
    /// exists: a job seen in the same state on every tick is notified once.
    #[test]
    fn a_state_is_notified_once_however_many_times_it_is_seen() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-watch-{}",
            super::super::protocol::random_token_id(12).unwrap()
        ));
        let mut store = RemoteStore::open(&root).unwrap();
        assert!(store
            .mark_push_notified("job-a", "waiting_approval", 10)
            .unwrap());
        assert!(!store
            .mark_push_notified("job-a", "waiting_approval", 20)
            .unwrap());
        assert!(!store
            .mark_push_notified("job-a", "waiting_approval", 30)
            .unwrap());
        // A real transition is a new edge and is notified.
        assert!(store.mark_push_notified("job-a", "succeeded", 40).unwrap());
        assert!(!store.mark_push_notified("job-a", "succeeded", 50).unwrap());
        // Pruning is by age, and a row inside the window survives.
        assert_eq!(store.prune_push_watch(20).unwrap(), 0);
        assert_eq!(store.prune_push_watch(1_000).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[derive(Default)]
    struct FakeSecrets(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

    impl super::super::store::RemoteSecretStore for FakeSecrets {
        fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
            self.0
                .lock()
                .unwrap()
                .get(slot)
                .cloned()
                .ok_or_else(|| "missing".to_string())
        }
        fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(slot.to_string(), secret.to_vec());
            Ok(())
        }
        fn delete(&self, slot: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    /// What the whole watcher is for: a run that stops for an approval, and one
    /// that finishes, each raise exactly one notification — and a finished
    /// *chat* turn says "new response" rather than "run finished", because the
    /// person waiting on it is waiting to read a reply.
    ///
    /// Also pins the quiet case: with no device registered for push there is
    /// nothing to decide, and the watcher must not spend a keychain read per
    /// tick working that out.
    #[test]
    fn a_transition_raises_one_notification_and_a_chat_turn_raises_a_reply() {
        use std::collections::BTreeSet;
        let root = std::env::temp_dir().join(format!(
            "little-monkey-watch-{}",
            super::super::protocol::random_token_id(12).unwrap()
        ));
        let mut store = RemoteStore::open(&root).unwrap();
        let scopes = super::super::protocol::RemoteScopes {
            actions: BTreeSet::from([super::super::protocol::RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".to_string()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1024,
        };

        // No device: nothing is decided and nothing is recorded, so the next
        // tick after a phone pairs still sees the transition.
        assert!(pending_notifications(
            &mut store,
            &[(
                "job-a".into(),
                JobState::WaitingApproval,
                Some("run-a".into())
            )],
            1_000,
        )
        .unwrap()
        .is_empty());

        let invitation = store.create_invitation(&scopes, 1, 1_000_000).unwrap();
        let device = store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "phone",
                "runner-one",
                1,
                &FakeSecrets::default(),
            )
            .unwrap()
            .device_id;
        store
            .save_push_registration(&device, "web_push", "{\"endpoint\":\"x\"}", 2)
            .unwrap();

        let observed = vec![
            (
                "job-a".to_string(),
                JobState::WaitingApproval,
                Some("run-a".to_string()),
            ),
            (
                "job-b".to_string(),
                JobState::Succeeded,
                Some("run-b".to_string()),
            ),
            (
                "job-c".to_string(),
                JobState::Running,
                Some("run-c".to_string()),
            ),
        ];
        let raised = pending_notifications(&mut store, &observed, 2_000).unwrap();
        assert_eq!(
            raised,
            vec![
                (PushKind::ApprovalRequested, Some("run-a".to_string())),
                (PushKind::RunCompleted, Some("run-b".to_string())),
            ],
            "a running job is not worth a notification"
        );

        // The same states seen again on the next tick raise nothing.
        assert!(pending_notifications(&mut store, &observed, 3_000)
            .unwrap()
            .is_empty());

        // A mobile chat turn, whose job id is derived from the message id the
        // device sent. Finishing it is a reply, not a run report.
        let message_id = "msg-abcdefghijkl".to_string();
        store
            .insert_mobile_message(&super::super::store::MobileMessageRecord {
                message_id: message_id.clone(),
                session_id: "session-one".to_string(),
                device_id: device.clone(),
                role: "user".to_string(),
                text: "hello".to_string(),
                request_sha256: "c".repeat(64),
                task_state: "queued".to_string(),
                created_at_ms: 4_000,
            })
            .unwrap();
        let chat_job = crate::daemon::mobile_chat_job_id(&message_id);
        let raised = pending_notifications(
            &mut store,
            &[(chat_job, JobState::Succeeded, Some("run-chat".into()))],
            5_000,
        )
        .unwrap();
        assert_eq!(
            raised,
            vec![(PushKind::NewResponse, Some("run-chat".to_string()))]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
