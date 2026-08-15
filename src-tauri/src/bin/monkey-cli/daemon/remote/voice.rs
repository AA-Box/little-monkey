//! Live microphone streams from a paired device.
//!
//! **Why this is not a device command with a big result.** Every other physical
//! capability answers one question once: take a photograph, read a fix, speak a
//! line. A stream has no single answer — it has a beginning, an unbounded
//! middle and an end — so the queue carries only the *control* command ("open
//! the microphone for session S") while the audio arrives on its own routes,
//! chunk by chunk, for as long as the command stays `running`. Cancelling that
//! command is what closes the microphone, so a stream inherits the truthful
//! cancellation the queue already has instead of inventing a second one.
//!
//! **Bounded at both ends.** The device stops at the duration it was given; the
//! runner closes the session at its own deadline regardless, and refuses a
//! chunk that would take the session past its byte ceiling. A phone that walks
//! into a tunnel with the microphone open leaves a closed session behind, not
//! an open one.
//!
//! **Exactly-once appends.** Chunks carry a sequence number. The runner writes
//! the one it is expecting, answers "already have it" to anything lower, and
//! refuses anything higher rather than writing a hole into the audio. The
//! counter moves only after the bytes reach the disk.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::protocol::{
    DeviceCapability, DeviceCommandState, VoiceChunkRequest, VoiceSessionState,
    MAX_VOICE_SESSION_BYTES, MAX_VOICE_SESSION_MS,
};
use super::store::{DeviceCommandRequest, RemoteStore, VoiceSessionRecord};
use crate::daemon::store::{restrict_file, DaemonPaths};

/// Default length of a stream an operator does not bound explicitly.
pub const DEFAULT_STREAM_MS: u64 = 60_000;
/// How often the device is asked to post what it has recorded. Short enough
/// that a cancellation reaches the microphone quickly — the runner's answer to
/// each chunk is where the device learns it was stopped.
pub const DEFAULT_CHUNK_MS: u64 = 1_000;
/// Grace beyond the requested duration before the runner closes a session the
/// device never closed. Covers one slow upload, not a tunnel.
const DEADLINE_GRACE_MS: u64 = 30_000;

pub fn audio_dir(root: &Path) -> PathBuf {
    root.join("voice-sessions")
}

/// Where one session's audio is appended. The session id is minted by this
/// module from the same alphabet as every other id here, so it cannot traverse.
pub fn audio_path(root: &Path, session_id: &str) -> PathBuf {
    audio_dir(root).join(format!("{session_id}.audio"))
}

/// Opens a stream on a device and queues the control command that starts it.
///
/// The order matters: the command is queued first so the session row can name
/// it, and the session id travels inside the command's arguments — the device
/// never invents a session, it is told which one to post to.
pub async fn start(
    paths: &DaemonPaths,
    device_id: Option<&str>,
    duration_ms: u64,
    source_run_id: Option<&str>,
    source_session_id: Option<&str>,
    now_ms: u64,
) -> Result<VoiceSessionRecord, String> {
    let duration_ms = duration_ms.clamp(1_000, MAX_VOICE_SESSION_MS);
    let mut store = RemoteStore::open(&paths.root)?;
    let device_id =
        super::device::resolve_target(&store, DeviceCapability::VoiceStream, device_id)?;
    let session_id = format!("vs-{}", super::protocol::random_token_id(18)?);
    let deadline_ms = now_ms
        .saturating_add(duration_ms)
        .saturating_add(DEADLINE_GRACE_MS);
    let command = store.enqueue_device_command(
        &DeviceCommandRequest {
            device_id: device_id.clone(),
            capability: DeviceCapability::VoiceStream,
            arguments: serde_json::json!({
                "session_id": session_id,
                "duration_ms": duration_ms,
                "chunk_ms": DEFAULT_CHUNK_MS,
            }),
            source_run_id: source_run_id.map(str::to_string),
            source_session_id: source_session_id.map(str::to_string),
            source_tool_call_id: None,
            // No invocation identity: a stream is opened by an operator asking
            // for one, and two deliberate asks are two streams.
            invocation_id: None,
            // The same clock as the session's, deliberately. A discrete command
            // expires in five minutes so a camera cannot fire long after the
            // conversation moved on; a stream's control command has to outlive
            // the stream itself, or the queue would fail a microphone that is
            // working exactly as asked.
            expires_at_ms: deadline_ms,
        },
        now_ms,
    )?;
    let record = store.open_voice_session(
        &session_id,
        &device_id,
        &command.command_id,
        source_run_id,
        source_session_id,
        deadline_ms,
        now_ms,
    )?;
    drop(store);

    // Same reason as a queued photograph: a phone with its screen off is not
    // long-polling, and the stream would sit queued until someone opened the
    // controller. Best effort — the command is durable either way.
    let _ = super::push::notify_device(
        paths,
        &record.device_id,
        &super::push::PushNotification {
            kind: super::push::PushKind::DeviceActionAwaiting,
            target_id: Some(record.command_id.clone()),
            detail: Some("a voice stream is waiting for you".to_string()),
        },
        &super::store::KeyringRemoteSecrets,
    )
    .await;
    Ok(record)
}

/// Asks for a stream to stop.
///
/// Cancels the control command rather than closing the session directly: the
/// device learns it was stopped from the answer to its next chunk, stops the
/// microphone, and closes the session itself. The session is left open until it
/// does, because "the runner stopped listening" and "the microphone is closed"
/// are not the same statement and only the device can make the second one.
pub fn stop(
    paths: &DaemonPaths,
    session_id: &str,
    now_ms: u64,
) -> Result<VoiceSessionRecord, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let record = store
        .voice_session(session_id)?
        .ok_or_else(|| format!("Unknown voice session '{session_id}'"))?;
    if record.state != VoiceSessionState::Open {
        return Ok(record);
    }
    store.request_device_cancel(&record.command_id, now_ms)?;
    // A command still queued or leased is cancelled outright by the call above,
    // which means no microphone was ever opened and nothing will close the
    // session. Close it here so it does not sit open until its deadline.
    let command = store.device_command(&record.command_id)?;
    if command.is_some_and(|command| command.state.terminal()) {
        return store.close_voice_session(
            session_id,
            Some("Stopped before the device opened the microphone"),
            now_ms,
        );
    }
    store
        .voice_session(session_id)?
        .ok_or_else(|| "Voice session disappeared".to_string())
}

pub fn sessions(
    paths: &DaemonPaths,
    device_id: Option<&str>,
    limit: u32,
) -> Result<Vec<VoiceSessionRecord>, String> {
    RemoteStore::open(&paths.root)?.voice_sessions(device_id, limit)
}

pub fn session(
    paths: &DaemonPaths,
    session_id: &str,
) -> Result<Option<VoiceSessionRecord>, String> {
    RemoteStore::open(&paths.root)?.voice_session(session_id)
}

/// What the device is told after each chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkOutcome {
    /// False when this sequence was already held — the device retried, and the
    /// runner did not append a second copy.
    pub accepted: bool,
    pub next_sequence: u64,
    pub bytes: u64,
    /// The device's stop signal. It travels on the reply to a request the
    /// device is already making, so no extra poll is needed to notice a
    /// cancellation.
    pub stop: bool,
}

/// Writes one chunk and moves the counter.
///
/// `store` is passed in already locked by the caller and stays locked across the
/// disk write: that lock is what makes the check-then-append-then-commit
/// sequence atomic, and therefore what stops two concurrent posts of the same
/// sequence from appending the audio twice.
pub fn accept_chunk(
    root: &Path,
    store: &mut RemoteStore,
    device_id: &str,
    session_id: &str,
    request: &VoiceChunkRequest,
    audio: &[u8],
    now_ms: u64,
) -> Result<ChunkOutcome, (u16, String)> {
    request.validate().map_err(|error| (400, error))?;
    let record = store
        .voice_session(session_id)
        .map_err(|error| (500, error))?
        .ok_or((404, "Unknown voice session".to_string()))?;
    if record.device_id != device_id {
        // Not "forbidden" with an explanation of whose it is: a device that
        // guessed a session id learns only that it does not own one.
        return Err((404, "Unknown voice session".to_string()));
    }
    if record.state != VoiceSessionState::Open {
        return Err((409, "The voice session is closed".to_string()));
    }
    let command = store
        .device_command(&record.command_id)
        .map_err(|error| (500, error))?;
    let stop = command.as_ref().is_some_and(|command| {
        command.cancel_requested || command.state != DeviceCommandState::Running
    });
    if record.next_sequence > request.sequence {
        // Already held. Answering 200 rather than an error is what makes a
        // retry over a flaky link harmless.
        return Ok(ChunkOutcome {
            accepted: false,
            next_sequence: record.next_sequence,
            bytes: record.bytes,
            stop,
        });
    }
    if record.next_sequence < request.sequence {
        return Err((
            409,
            format!(
                "Expected voice chunk {} but was sent {}",
                record.next_sequence, request.sequence
            ),
        ));
    }
    if now_ms >= record.deadline_ms {
        store
            .close_voice_session(
                session_id,
                Some("The stream passed its deadline without being closed"),
                now_ms,
            )
            .map_err(|error| (500, error))?;
        return Err((409, "The voice session passed its deadline".to_string()));
    }
    let bytes = audio.len() as u64;
    if record.bytes.saturating_add(bytes) > MAX_VOICE_SESSION_BYTES {
        store
            .close_voice_session(
                session_id,
                Some("The stream reached this runner's size ceiling"),
                now_ms,
            )
            .map_err(|error| (500, error))?;
        return Err((413, "The voice session is full".to_string()));
    }
    if record.media_type.is_none() && request.media_type.is_none() {
        return Err((
            400,
            "The first voice chunk must declare its media type".to_string(),
        ));
    }
    append(root, session_id, audio).map_err(|error| (500, error))?;
    let updated = store
        .commit_voice_chunk(session_id, bytes, request.media_type.as_deref(), now_ms)
        .map_err(|error| (500, error))?;
    Ok(ChunkOutcome {
        accepted: true,
        next_sequence: updated.next_sequence,
        bytes: updated.bytes,
        stop,
    })
}

fn append(root: &Path, session_id: &str, audio: &[u8]) -> Result<(), String> {
    let directory = audio_dir(root);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create the voice session directory: {error}"))?;
    let path = audio_path(root, session_id);
    let existed = path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("Could not open the voice session file: {error}"))?;
    file.write_all(audio)
        .map_err(|error| format!("Could not write the voice session file: {error}"))?;
    // Durable before the counter moves — see this module's header.
    file.sync_data()
        .map_err(|error| format!("Could not flush the voice session file: {error}"))?;
    drop(file);
    if !existed {
        // Recorded audio from someone's room. Same treatment as every other
        // secret this daemon writes.
        restrict_file(&path)?;
    }
    Ok(())
}

/// Closes a stream on the device's word, and finishes the control command with
/// it. Returns the closed record.
pub fn close(
    store: &mut RemoteStore,
    device_id: &str,
    session_id: &str,
    error: Option<&str>,
    now_ms: u64,
) -> Result<VoiceSessionRecord, (u16, String)> {
    let record = store
        .voice_session(session_id)
        .map_err(|error| (500, error))?
        .ok_or((404, "Unknown voice session".to_string()))?;
    if record.device_id != device_id {
        return Err((404, "Unknown voice session".to_string()));
    }
    let closed = store
        .close_voice_session(session_id, error, now_ms)
        .map_err(|error| (500, error))?;
    Ok(closed)
}

/// Closes every stream whose deadline has passed and fails the control command
/// each one was riding on.
///
/// Called from the same places that expire device commands. Failing the command
/// is the honest report: the runner cannot know whether the microphone is still
/// open, and the error says exactly that.
pub fn expire(store: &mut RemoteStore, now_ms: u64) -> Result<usize, String> {
    // A session whose control command is already over is over too, whatever its
    // deadline says. This is the path a cancellation from the desktop's device
    // card takes: it cancels the command, and the stream it was carrying closes
    // here rather than sitting open until its clock runs out.
    let mut orphaned = 0;
    for record in store.voice_sessions(None, 64)? {
        if record.state != VoiceSessionState::Open {
            continue;
        }
        let over = store
            .device_command(&record.command_id)?
            .is_some_and(|command| command.state.terminal());
        if over {
            store.close_voice_session(
                &record.session_id,
                Some("The command carrying this stream ended"),
                now_ms,
            )?;
            orphaned += 1;
        }
    }
    let expired = store.expire_voice_sessions(now_ms)?;
    for session_id in &expired {
        let Some(record) = store.voice_session(session_id)? else {
            continue;
        };
        let Some(command) = store.device_command(&record.command_id)? else {
            continue;
        };
        if command.state.terminal() {
            continue;
        }
        // Ignored rather than propagated: the session is already closed, and a
        // command that reached a terminal state in the meantime is not an error
        // worth failing an expiry sweep over.
        let _ = store.complete_device_command(
            &record.device_id,
            &record.command_id,
            DeviceCommandState::Failed,
            None,
            None,
            Some(
                "The stream passed its deadline without the device closing it; whether the \
                 microphone is still open is unproven",
            ),
            None,
            now_ms,
        );
    }
    Ok(expired.len() + orphaned)
}

/// The summary an operator or an agent reads.
pub fn session_json(record: &VoiceSessionRecord) -> serde_json::Value {
    serde_json::json!({
        "session_id": record.session_id,
        "device_id": record.device_id,
        "command_id": record.command_id,
        "state": record.state.as_str(),
        "media_type": record.media_type,
        "chunks": record.next_sequence,
        "bytes": record.bytes,
        "created_at_ms": record.created_at_ms,
        "closed_at_ms": record.closed_at_ms,
        "error": record.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "lm-voice-{}",
            super::super::protocol::random_token_id(12).unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The property the whole module exists for: a device that retries a chunk
    /// it already delivered must not have it appended twice, and a device that
    /// skips one must not be able to leave a hole.
    #[test]
    fn a_retried_chunk_is_not_appended_twice_and_a_gap_is_refused() {
        let root = temporary_root();
        let mut store = RemoteStore::open(&root).unwrap();
        let (device_id, command_id, session_id) = fixture(&mut store, &root);

        let chunk = |sequence: u64, audio: &str| VoiceChunkRequest {
            protocol_version: super::super::protocol::REMOTE_PROTOCOL_VERSION,
            sequence,
            audio_base64: audio.to_string(),
            media_type: Some("audio/webm".to_string()),
            last: false,
        };

        let first = accept_chunk(
            &root,
            &mut store,
            &device_id,
            &session_id,
            &chunk(0, "AAA"),
            b"one",
            10,
        )
        .unwrap();
        assert!(first.accepted);
        assert_eq!(first.next_sequence, 1);
        assert_eq!(first.bytes, 3);

        // The same sequence again: accepted as "already held", nothing written.
        let retry = accept_chunk(
            &root,
            &mut store,
            &device_id,
            &session_id,
            &chunk(0, "AAA"),
            b"one",
            11,
        )
        .unwrap();
        assert!(!retry.accepted);
        assert_eq!(retry.bytes, 3);

        // A skipped sequence is refused rather than silently leaving a hole.
        let gap = accept_chunk(
            &root,
            &mut store,
            &device_id,
            &session_id,
            &chunk(7, "AAA"),
            b"seven",
            12,
        );
        assert_eq!(gap.unwrap_err().0, 409);

        let second = accept_chunk(
            &root,
            &mut store,
            &device_id,
            &session_id,
            &chunk(1, "BBB"),
            b"two",
            13,
        )
        .unwrap();
        assert!(second.accepted);
        assert_eq!(
            std::fs::read(audio_path(&root, &session_id)).unwrap(),
            b"onetwo",
            "the retried chunk was appended a second time"
        );

        // Another device's id must not reach this session at all.
        assert_eq!(
            accept_chunk(
                &root,
                &mut store,
                "device-other",
                &session_id,
                &chunk(2, "C"),
                b"x",
                14
            )
            .unwrap_err()
            .0,
            404
        );

        assert!(!command_id.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stream nobody closed does not stay open, and the command it rode on is
    /// failed with an honest reason rather than left running for ever.
    #[test]
    fn a_stream_the_device_abandons_is_closed_by_the_runner() {
        let root = temporary_root();
        let mut store = RemoteStore::open(&root).unwrap();
        let (_device_id, command_id, session_id) = fixture(&mut store, &root);

        assert_eq!(expire(&mut store, 100).unwrap(), 0);
        let expired = expire(&mut store, 10_000_000).unwrap();
        assert_eq!(expired, 1);

        let closed = store.voice_session(&session_id).unwrap().unwrap();
        assert_eq!(closed.state, VoiceSessionState::Failed);
        let command = store.device_command(&command_id).unwrap().unwrap();
        assert_eq!(command.state, DeviceCommandState::Failed);
        assert!(command.error.unwrap().contains("unproven"));
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

    /// Builds a paired device, a running voice command and its open session.
    fn fixture(store: &mut RemoteStore, _root: &Path) -> (String, String, String) {
        use std::collections::BTreeSet;
        let scopes = super::super::protocol::RemoteScopes {
            actions: BTreeSet::from([super::super::protocol::RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".to_string()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1024 * 1024,
        };
        let invitation = store.create_invitation(&scopes, 1, 1_000_000).unwrap();
        let device = store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "stream phone",
                "runner-one",
                1,
                &FakeSecrets::default(),
            )
            .unwrap()
            .device_id;
        let command = store
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: device.clone(),
                    capability: DeviceCapability::VoiceStream,
                    arguments: serde_json::json!({ "duration_ms": 5_000 }),
                    source_run_id: None,
                    source_session_id: None,
                    source_tool_call_id: None,
                    invocation_id: None,
                    expires_at_ms: 1_000_000,
                },
                1,
            )
            .unwrap();
        store
            .lease_device_command(&device, 30_000, 2)
            .unwrap()
            .unwrap();
        assert!(
            store
                .start_device_command(&device, &command.command_id, Some("exec-voice"), 3)
                .unwrap()
                .started
        );
        let session_id = format!(
            "vs-{}",
            super::super::protocol::random_token_id(18).unwrap()
        );
        store
            .open_voice_session(
                &session_id,
                &device,
                &command.command_id,
                None,
                None,
                5_000,
                4,
            )
            .unwrap();
        (device, command.command_id, session_id)
    }
}
