//! Asking a paired physical device to do something, from the runner's side.
//!
//! The device plane in `api.rs` is the *device's* half of this: it leases,
//! starts and reports. This is the other half — validating what an agent or an
//! operator asked for, queueing it, and waiting for the answer.
//!
//! It talks to `RemoteStore` directly rather than through the daemon's HTTP
//! surface. The queue is a SQLite table in the runner's own state directory and
//! both processes already open it under WAL; going through the loopback would
//! add a hop, a second set of errors, and nothing else.

use std::collections::BTreeSet;

use super::protocol::{
    effective_capabilities, DeviceCapability, DeviceCommandState, MAX_DEVICE_COMMAND_ARG_BYTES,
};
use super::store::{DeviceCommandRecord, DeviceCommandRequest, RemoteStore};
use crate::daemon::store::DaemonPaths;

/// How long a queued command stays worth running if no device ever takes it.
/// Past this it expires rather than surprising someone by firing a camera an
/// hour after the conversation moved on.
pub const DEFAULT_COMMAND_TTL_MS: u64 = 5 * 60 * 1_000;
/// Ceiling on how long a caller may block waiting for a device.
pub const MAX_WAIT_MS: u64 = 120_000;
/// Ceiling on one bounded microphone recording.
pub const MAX_RECORDING_MS: u64 = 300_000;

/// One `device_action` request, already parsed out of the model's arguments.
#[derive(Debug, Clone)]
pub struct DeviceActionRequest {
    pub device_id: Option<String>,
    pub capability: DeviceCapability,
    pub arguments: serde_json::Value,
    pub wait_ms: u64,
    pub source_run_id: Option<String>,
    pub source_session_id: Option<String>,
    pub source_tool_call_id: Option<String>,
}

/// Maps the tool's `action` string onto a capability.
///
/// Only the capabilities a discrete command can express are here: `voice_stream`
/// is deliberately absent, because a continuous stream is not a queued command
/// and pretending otherwise would let a grant meant for Talk be spent through
/// this tool.
pub fn capability_for_action(action: &str) -> Result<DeviceCapability, String> {
    Ok(match action {
        "device_info" => DeviceCapability::DeviceInfo,
        "camera_capture" => DeviceCapability::CameraCapture,
        "microphone_capture" => DeviceCapability::MicrophoneCapture,
        "location_read" => DeviceCapability::LocationRead,
        "notification_post" => DeviceCapability::NotificationPost,
        "screen_capture" => DeviceCapability::ScreenCapture,
        "audio_playback" => DeviceCapability::AudioPlayback,
        other => {
            return Err(format!(
                "Unknown device action '{other}'. Valid actions are device_info, camera_capture, \
                 microphone_capture, location_read, notification_post, screen_capture and \
                 audio_playback."
            ))
        }
    })
}

/// Validates and normalizes the arguments for one action.
///
/// Server-side, always: the model's arguments are checked here rather than on
/// the phone, so a device build with a lenient parser cannot be talked into a
/// ten-minute recording. The device applies its own advertised bounds on top.
pub fn validate_arguments(
    capability: DeviceCapability,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let field = |name: &str| arguments.get(name).and_then(|value| value.as_str());
    let normalized = match capability {
        DeviceCapability::DeviceInfo | DeviceCapability::ScreenCapture => serde_json::json!({}),
        DeviceCapability::CameraCapture => {
            let position = field("position").unwrap_or("back");
            if !matches!(position, "front" | "back") {
                return Err("Camera position must be 'front' or 'back'".to_string());
            }
            serde_json::json!({ "position": position })
        }
        DeviceCapability::MicrophoneCapture => {
            let duration_ms = arguments
                .get("duration_ms")
                .and_then(|value| value.as_u64())
                .unwrap_or(10_000);
            if duration_ms == 0 || duration_ms > MAX_RECORDING_MS {
                return Err(format!(
                    "Recording duration must be between 1 ms and {MAX_RECORDING_MS} ms"
                ));
            }
            serde_json::json!({ "duration_ms": duration_ms })
        }
        DeviceCapability::LocationRead => {
            let accuracy = field("accuracy").unwrap_or("coarse");
            if !matches!(accuracy, "coarse" | "precise") {
                return Err("Location accuracy must be 'coarse' or 'precise'".to_string());
            }
            serde_json::json!({ "accuracy": accuracy })
        }
        DeviceCapability::NotificationPost => {
            let title = field("title")
                .map(str::trim)
                .filter(|value| !value.is_empty() && value.len() <= 128)
                .ok_or("A notification needs a 'title' of 1-128 characters")?;
            let body = field("body")
                .map(str::trim)
                .filter(|value| value.len() <= 512)
                .ok_or("A notification 'body' may be at most 512 characters")?;
            serde_json::json!({ "title": title, "body": body })
        }
        DeviceCapability::AudioPlayback => {
            let text = field("text")
                .map(str::trim)
                .filter(|value| !value.is_empty() && value.len() <= 1_024)
                .ok_or("Audio playback needs 'text' of 1-1024 characters to speak")?;
            serde_json::json!({ "text": text })
        }
        other => return Err(format!("'{other:?}' is not a discrete device command")),
    };
    if serde_json::to_vec(&normalized)
        .map(|encoded| encoded.len())
        .unwrap_or(usize::MAX)
        > MAX_DEVICE_COMMAND_ARG_BYTES
    {
        return Err("Device command arguments exceed 8 KiB".to_string());
    }
    Ok(normalized)
}

/// The device this action should go to, and why.
///
/// With no `device_id` the caller gets the single device for which the
/// capability is effective. Two candidates is an error rather than a guess: an
/// agent taking a photograph on whichever phone happened to sort first is a
/// worse outcome than being asked which one.
pub fn resolve_target(
    store: &RemoteStore,
    capability: DeviceCapability,
    requested: Option<&str>,
) -> Result<String, String> {
    let candidates = store
        .devices()?
        .into_iter()
        .filter(|device| device.active())
        .filter(|device| {
            let surface = store.device_surface(&device.device_id).ok().flatten();
            effective_capabilities(&device.capabilities, surface.as_ref()).contains(&capability)
        })
        .map(|device| (device.device_id, device.device_name))
        .collect::<Vec<_>>();
    if let Some(requested) = requested {
        return candidates
            .into_iter()
            .find(|(device_id, _)| device_id == requested)
            .map(|(device_id, _)| device_id)
            .ok_or_else(|| {
                format!(
                    "Device '{requested}' cannot do this: the capability is not granted, not \
                     advertised by that device, or not permitted by its operating system."
                )
            });
    }
    match candidates.len() {
        0 => Err(format!(
            "No paired device can do this. Grant '{}' to a device and make sure the device has \
             advertised it and been given the matching operating-system permission.",
            capability_token(capability)
        )),
        1 => Ok(candidates.into_iter().next().expect("length checked").0),
        _ => Err(format!(
            "{} paired devices can do this — name one with 'device_id': {}",
            candidates.len(),
            candidates
                .iter()
                .map(|(device_id, name)| format!("{device_id} ({name})"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn capability_token(capability: DeviceCapability) -> String {
    serde_json::to_value(capability)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Whether any paired device could actually perform any physical action right
/// now — the condition for offering the `device_action` tool at all.
///
/// Never fails: a machine with no daemon state, no pairing or an unreadable
/// database simply has no capable device, and a turn must not be blocked by
/// asking.
pub fn any_device_is_capable() -> bool {
    let Ok(paths) = DaemonPaths::resolve() else {
        return false;
    };
    let Ok(store) = RemoteStore::open(&paths.root) else {
        return false;
    };
    let Ok(devices) = store.devices() else {
        return false;
    };
    devices
        .into_iter()
        .filter(|device| device.active())
        .any(|device| {
            let surface = store.device_surface(&device.device_id).ok().flatten();
            effective_capabilities(&device.capabilities, surface.as_ref())
                .iter()
                .any(|capability| {
                    capability.is_physical() && *capability != DeviceCapability::VoiceStream
                })
        })
}

/// Queues one command and waits for the device to finish it.
///
/// Returns as soon as the command reaches a terminal state. On timeout the
/// command is left alone rather than cancelled: it may already be running, and
/// the honest answer to "did the photo happen" is "still running", not "no".
pub async fn dispatch(
    paths: &DaemonPaths,
    request: &DeviceActionRequest,
    now_ms: u64,
) -> Result<DeviceCommandRecord, String> {
    let arguments = validate_arguments(request.capability, &request.arguments)?;
    let wait_ms = request.wait_ms.clamp(1_000, MAX_WAIT_MS);
    let mut store = RemoteStore::open(&paths.root)?;
    let device_id = resolve_target(&store, request.capability, request.device_id.as_deref())?;
    let queued = store.enqueue_device_command(
        &DeviceCommandRequest {
            device_id,
            capability: request.capability,
            arguments,
            source_run_id: request.source_run_id.clone(),
            source_session_id: request.source_session_id.clone(),
            source_tool_call_id: request.source_tool_call_id.clone(),
            expires_at_ms: now_ms.saturating_add(DEFAULT_COMMAND_TTL_MS),
        },
        now_ms,
    )?;
    drop(store);

    let started = std::time::Instant::now();
    loop {
        // Reopened each poll rather than held: the daemon writes to this
        // database from another process, and a long-lived read transaction here
        // would be the one thing that could block it.
        let store = RemoteStore::open(&paths.root)?;
        let current = store
            .device_command(&queued.command_id)?
            .ok_or_else(|| "The queued device command disappeared".to_string())?;
        drop(store);
        if current.state.terminal() {
            return Ok(current);
        }
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if elapsed >= wait_ms {
            return Ok(current);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// The tool result an agent sees. Deliberately says what state the command
/// reached rather than only succeeding or failing, so a model reading it cannot
/// mistake "still running on the phone" for "done".
pub fn result_json(record: &DeviceCommandRecord) -> serde_json::Value {
    serde_json::json!({
        "command_id": record.command_id,
        "device_id": record.device_id,
        "action": capability_token(record.capability),
        "state": record.state.as_str(),
        "result": record.result,
        "artifact": record.artifact.as_ref().map(|artifact| serde_json::json!({
            "sha256": artifact.sha256,
            "bytes": artifact.bytes,
            "media_type": artifact.media_type,
        })),
        "error": record.error,
        "note": match record.state {
            DeviceCommandState::Running | DeviceCommandState::Leased | DeviceCommandState::Queued =>
                Some("The device has not reported back yet; the action may still be in progress."),
            _ => None,
        },
    })
}

/// Every capability an operator may grant to a physical device, for the CLI and
/// the desktop's grant editor. Ordered weakest-first so a picker reads sensibly.
pub fn grantable_physical_capabilities() -> Vec<DeviceCapability> {
    vec![
        DeviceCapability::DeviceInfo,
        DeviceCapability::NotificationPost,
        DeviceCapability::LocationRead,
        DeviceCapability::AudioPlayback,
        DeviceCapability::CameraCapture,
        DeviceCapability::MicrophoneCapture,
        DeviceCapability::ScreenCapture,
        DeviceCapability::VoiceStream,
    ]
}

/// Parses the `--capability` values an operator passed on the command line.
pub fn parse_capabilities(values: &[String]) -> Result<BTreeSet<DeviceCapability>, String> {
    values
        .iter()
        .map(|value| {
            let normalized = value.trim().replace('-', "_");
            serde_json::from_value::<DeviceCapability>(serde_json::Value::String(normalized))
                .map_err(|_| format!("Unknown device capability '{value}'"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_bounded_before_they_reach_a_phone() {
        assert!(validate_arguments(
            DeviceCapability::MicrophoneCapture,
            &serde_json::json!({ "duration_ms": 600_000 }),
        )
        .is_err());
        assert_eq!(
            validate_arguments(
                DeviceCapability::MicrophoneCapture,
                &serde_json::json!({ "duration_ms": 5_000 }),
            )
            .unwrap(),
            serde_json::json!({ "duration_ms": 5_000 })
        );
        // A defaulted argument is normalized, so what was queued is explicit in
        // the audit rather than implied.
        assert_eq!(
            validate_arguments(DeviceCapability::CameraCapture, &serde_json::json!({})).unwrap(),
            serde_json::json!({ "position": "back" })
        );
        assert!(validate_arguments(
            DeviceCapability::CameraCapture,
            &serde_json::json!({ "position": "ceiling" }),
        )
        .is_err());
        assert!(validate_arguments(
            DeviceCapability::NotificationPost,
            &serde_json::json!({ "body": "no title" }),
        )
        .is_err());
        // Unknown extra keys are dropped rather than forwarded: the phone only
        // ever sees the normalized object.
        assert_eq!(
            validate_arguments(
                DeviceCapability::LocationRead,
                &serde_json::json!({ "accuracy": "precise", "track_forever": true }),
            )
            .unwrap(),
            serde_json::json!({ "accuracy": "precise" })
        );
    }

    #[test]
    fn voice_stream_is_not_reachable_through_the_discrete_tool() {
        assert!(capability_for_action("voice_stream").is_err());
        assert!(validate_arguments(DeviceCapability::VoiceStream, &serde_json::json!({})).is_err());
    }

    #[test]
    fn capability_names_accept_both_spellings_an_operator_might_type() {
        assert_eq!(
            parse_capabilities(&["camera-capture".into(), "location_read".into()]).unwrap(),
            BTreeSet::from([
                DeviceCapability::CameraCapture,
                DeviceCapability::LocationRead
            ])
        );
        assert!(parse_capabilities(&["root-access".into()]).is_err());
    }
}
