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
    capability_block, effective_capabilities, legacy_capabilities, validate_id, DeviceCapability,
    DeviceCommandState, MAX_DEVICE_COMMAND_ARG_BYTES,
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
    /// The durable identity of the invocation asking, when there is one.
    /// See [`invocation_identity`].
    pub invocation_id: Option<String>,
}

/// The identity of one durable tool invocation, in the same shape the channel
/// send path already uses: the daemon's job id from the environment it set on
/// its own child, plus the agent loop's tool-call id.
///
/// Both halves come from the runtime — a model cannot supply, repeat or omit
/// either — which is what makes the pair safe to key a physical action on. A
/// turn that is replayed reaches the same pair and therefore the same command;
/// an operator running the CLI twice has no pair at all and gets two commands,
/// which is what they asked for.
pub fn invocation_identity(tool_call_id: Option<&str>) -> Option<String> {
    let job_id = std::env::var("LITTLE_MONKEY_DAEMON_JOB_ID")
        .ok()
        .filter(|id| !id.is_empty())?;
    let tool_call_id = tool_call_id.map(str::trim).filter(|id| !id.is_empty())?;
    Some(format!("{job_id}:{tool_call_id}"))
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
    let normalized =
        match capability {
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
            // Two shapes, because "play this" and "say this" are different asks and
            // collapsing them would mean one of the two is a lie. An artifact plays
            // the recorded bytes; `text` is spoken by the device's own synthesizer.
            // Never both: a device would have to choose, and whichever it chose
            // would be a surprise.
            DeviceCapability::AudioPlayback => {
                let artifact_id = field("artifact_id")
                    .map(str::trim)
                    .filter(|v| !v.is_empty());
                let run_id = field("run_id").map(str::trim).filter(|v| !v.is_empty());
                let text = field("text")
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && value.len() <= 1_024);
                match (artifact_id, run_id, text) {
                    (Some(_), Some(_), Some(_)) => {
                        return Err(
                            "Audio playback takes either an artifact to play or 'text' to speak, \
                         not both"
                                .to_string(),
                        )
                    }
                    (Some(artifact_id), Some(run_id), None) => {
                        validate_id(artifact_id)?;
                        validate_id(run_id)?;
                        serde_json::json!({ "artifact_id": artifact_id, "run_id": run_id })
                    }
                    (Some(_), None, _) | (None, Some(_), _) => return Err(
                        "Playing an artifact needs both 'run_id' and 'artifact_id' — the device \
                         reads it back through the run it belongs to"
                            .to_string(),
                    ),
                    (None, None, Some(text)) => serde_json::json!({ "text": text }),
                    (None, None, None) => return Err(
                        "Audio playback needs either 'text' of 1-1024 characters to speak, or a \
                         'run_id' and 'artifact_id' to play"
                            .to_string(),
                    ),
                }
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
    // Every active device, with the one thing standing between it and this
    // capability. Kept rather than discarded, because "which of the four axes
    // said no" is the entire content of a useful failure here.
    let considered = store
        .devices()?
        .into_iter()
        .filter(|device| device.active())
        .map(|device| {
            let surface = store.device_surface(&device.device_id).ok().flatten();
            let granted = if device.capabilities.is_empty() {
                legacy_capabilities(&device.scopes)
            } else {
                device.capabilities.clone()
            };
            let block = capability_block(&granted, surface.as_ref(), capability);
            (device.device_id, device.device_name, block)
        })
        .collect::<Vec<_>>();
    if let Some(requested) = requested {
        let Some((device_id, _, block)) = considered
            .into_iter()
            .find(|(device_id, _, _)| device_id == requested)
        else {
            return Err(format!("There is no active paired device '{requested}'."));
        };
        return match block {
            None => Ok(device_id),
            Some(block) => Err(block.explain(capability)),
        };
    }
    let eligible = considered
        .iter()
        .filter(|(_, _, block)| block.is_none())
        .collect::<Vec<_>>();
    match eligible.len() {
        0 => Err(match considered.len() {
            0 => format!(
                "No device is paired with this runner, so '{}' cannot be performed. Pair a device \
                 first.",
                capability_token(capability)
            ),
            // One device: say exactly what that device is missing rather than a
            // generic sentence that fits every case and helps in none.
            1 => considered[0]
                .2
                .expect("no eligible device means this one is blocked")
                .explain(capability),
            _ => format!(
                "No paired device can do this right now: {}",
                considered
                    .iter()
                    .filter_map(|(device_id, name, block)| block
                        .map(|block| format!("{name} ({device_id}) — {}", block.as_str())))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }),
        1 => Ok(eligible[0].0.clone()),
        _ => Err(format!(
            "{} paired devices can do this — name one with 'device_id': {}",
            eligible.len(),
            eligible
                .iter()
                .map(|(device_id, name, _)| format!("{device_id} ({name})"))
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
    if any_extension_device_provider() {
        return true;
    }
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

// --- executable-extension device providers -----------------------------------
//
// A paired phone is not the only thing that can hold a camera or a speaker. An
// extension may contribute its own devices — a lab instrument, a smart-home
// bridge, a second machine on the desk — and they route through this same
// module rather than a parallel one, so the permission prompt, the argument
// validation, the action vocabulary and the shape of the result are identical
// whichever kind of device answered.
//
// The device id is namespaced with the owning extension and capability. That
// is not decoration: it is what makes it structurally impossible for one
// extension to name a device belonging to another, or to collide with a
// paired device's id.

/// Prefix marking a device id as belonging to an extension provider.
pub const EXTENSION_DEVICE_PREFIX: &str = "ext:";
/// How many devices one provider may advertise.
const MAX_EXTENSION_DEVICES: usize = 128;

/// One device an extension provider currently offers.
#[derive(Debug, Clone)]
pub struct ExtensionDevice {
    pub device_id: String,
    pub device_name: String,
    pub extension_id: String,
    pub capability_id: String,
    pub actions: BTreeSet<DeviceCapability>,
}

#[derive(serde::Deserialize)]
struct ExtensionDeviceList {
    #[serde(default)]
    devices: Vec<ExtensionDeviceEntry>,
}

#[derive(serde::Deserialize)]
struct ExtensionDeviceEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    actions: Vec<String>,
}

/// Split a namespaced device id back into owner, capability and the device's
/// own id. `None` for every id that is not an extension device.
pub fn extension_device_target(device_id: &str) -> Option<(String, String, String)> {
    let rest = device_id.strip_prefix(EXTENSION_DEVICE_PREFIX)?;
    let mut parts = rest.splitn(3, ':');
    let extension_id = parts.next()?;
    let capability_id = parts.next()?;
    let local_id = parts.next()?;
    if extension_id.is_empty() || capability_id.is_empty() || local_id.is_empty() {
        return None;
    }
    Some((
        extension_id.to_string(),
        capability_id.to_string(),
        local_id.to_string(),
    ))
}

fn extension_manager(
    app_data: &std::path::Path,
) -> Result<little_monkey_lib::executable_extensions::ExtensionManager, String> {
    little_monkey_lib::executable_extensions::ExtensionManager::new(app_data)
}

fn ambient_app_data() -> Option<std::path::PathBuf> {
    little_monkey_lib::app_paths::data_dir()
}

/// Ask every healthy device-provider extension what devices it currently has.
///
/// Discovery is live rather than cached: a device that has gone away, or an
/// extension that has been disabled, stops being a candidate immediately, and
/// that is the whole point of routing through the registry instead of a
/// remembered list.
pub async fn extension_devices(app_data: &std::path::Path) -> Result<Vec<ExtensionDevice>, String> {
    let manager = match extension_manager(app_data) {
        Ok(manager) => manager,
        // No extension store at all is "no extension devices", not an error:
        // the paired-device path must still work on a machine that has never
        // installed one.
        Err(_) => return Ok(Vec::new()),
    };
    let capabilities = manager
        .active_capabilities(Some(
            little_monkey_lib::executable_extensions::CapabilityKind::DeviceProvider,
        ))
        .unwrap_or_default();
    let mut devices = Vec::new();
    for capability in capabilities {
        if capability.extension_id.contains(':') || capability.capability_id.contains(':') {
            continue;
        }
        let result = manager
            .invoke_owned_active_capability(
                little_monkey_lib::executable_extensions::CapabilityKind::DeviceProvider,
                &capability.extension_id,
                &capability.capability_id,
                serde_json::json!({ "query": "devices" }).to_string(),
                None,
                Vec::new(),
            )
            .await;
        // One broken provider must not take the rest of them — or the paired
        // devices — down with it. It simply contributes nothing.
        let Ok(result) = result else { continue };
        let Ok(listed) = serde_json::from_str::<ExtensionDeviceList>(&result.output_json) else {
            continue;
        };
        for entry in listed.devices.into_iter().take(MAX_EXTENSION_DEVICES) {
            if validate_id(&entry.id).is_err() || entry.id.contains(':') {
                continue;
            }
            let actions: BTreeSet<DeviceCapability> = entry
                .actions
                .iter()
                .filter_map(|action| capability_for_action(action).ok())
                .collect();
            if actions.is_empty() {
                continue;
            }
            let name = entry
                .name
                .filter(|name| !name.trim().is_empty() && name.len() <= 128)
                .unwrap_or_else(|| entry.id.clone());
            devices.push(ExtensionDevice {
                device_id: format!(
                    "{EXTENSION_DEVICE_PREFIX}{}:{}:{}",
                    capability.extension_id, capability.capability_id, entry.id
                ),
                device_name: name,
                extension_id: capability.extension_id.clone(),
                capability_id: capability.capability_id.clone(),
                actions,
            });
        }
    }
    Ok(devices)
}

/// Whether any installed extension contributes device actions at all.
///
/// Answered from the registry rather than by invoking every provider: this
/// gates whether the `device_action` tool is offered on a turn, and paying a
/// sandbox start-up per turn to find out would be a poor trade for a question
/// whose answer is "is such a provider installed and healthy".
pub fn any_extension_device_provider() -> bool {
    ambient_app_data()
        .ok_or_else(|| "no app data".to_string())
        .and_then(extension_manager_owned)
        .and_then(|manager| {
            manager.active_capabilities(Some(
                little_monkey_lib::executable_extensions::CapabilityKind::DeviceProvider,
            ))
        })
        .is_ok_and(|capabilities| !capabilities.is_empty())
}

fn extension_manager_owned(
    app_data: std::path::PathBuf,
) -> Result<little_monkey_lib::executable_extensions::ExtensionManager, String> {
    extension_manager(&app_data)
}

/// Run one action on an extension-provided device.
///
/// The action reaches the guest as a capability token the host resolved, never
/// as a free string the model wrote: an undeclared action cannot be smuggled
/// through, and a device the provider did not advertise is refused before the
/// sandbox is even started.
async fn dispatch_extension(
    app_data: &std::path::Path,
    request: &DeviceActionRequest,
    device: &ExtensionDevice,
    now_ms: u64,
) -> Result<DeviceCommandRecord, String> {
    if !device.actions.contains(&request.capability) {
        return Err(format!(
            "Device '{}' does not advertise '{}'",
            device.device_id,
            capability_token(request.capability)
        ));
    }
    let arguments = validate_arguments(request.capability, &request.arguments)?;
    let (_, _, local_id) = extension_device_target(&device.device_id)
        .ok_or_else(|| "Extension device id is malformed".to_string())?;
    let input_json = serde_json::json!({
        "query": "action",
        "device_id": local_id,
        "action": capability_token(request.capability),
        "arguments": arguments,
    })
    .to_string();
    let manager = extension_manager(app_data)?;
    // The same at-most-once identity a paired device's command carries, spent
    // through the extension runtime's own durable invocation ledger: a retried
    // tool call with the same invocation identity replays the cached result
    // rather than running the action a second time. Without one — an operator
    // driving this by hand — each call is its own action, which is what the
    // caller asked for.
    let command_id = match request.invocation_id.as_deref() {
        Some(invocation_id) => format!(
            "xdevice-{}",
            &little_monkey_lib::executable_extensions::stable_invocation_suffix(&[
                invocation_id,
                &device.device_id,
                capability_token(request.capability).as_str(),
            ])
        ),
        None => format!("xdevice-{}", uuid::Uuid::new_v4().simple()),
    };
    let result = manager
        .invoke_owned_active_capability(
            little_monkey_lib::executable_extensions::CapabilityKind::DeviceProvider,
            &device.extension_id,
            &device.capability_id,
            input_json,
            Some(command_id.clone()),
            Vec::new(),
        )
        .await?;
    let output: serde_json::Value = serde_json::from_str(&result.output_json)
        .map_err(|error| format!("The device extension returned invalid output: {error}"))?;
    let error = output
        .get("error")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    // An artifact a provider returns has to be one it wrote during this same
    // invocation, for the same reason every other consumer checks: naming a
    // content-addressed id proves nothing about who owns the content.
    let artifact = match output.get("artifact_id").and_then(|value| value.as_str()) {
        Some(artifact_id)
            if result
                .written_artifact_ids
                .iter()
                .any(|id| id == artifact_id) =>
        {
            Some(super::store::DeviceArtifact {
                sha256: artifact_id.to_string(),
                bytes: output
                    .get("artifact_bytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                media_type: output
                    .get("media_type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("application/octet-stream")
                    .to_string(),
            })
        }
        Some(_) => {
            return Err(
                "The device extension named an artifact it did not write; result refused"
                    .to_string(),
            )
        }
        None => None,
    };
    let terminal_digest = {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(result.output_json.as_bytes());
        format!("{:x}", digest.finalize())
    };
    Ok(DeviceCommandRecord {
        command_id: command_id.clone(),
        device_id: device.device_id.clone(),
        capability: request.capability,
        arguments,
        arguments_sha256: String::new(),
        source_run_id: request.source_run_id.clone(),
        source_session_id: request.source_session_id.clone(),
        source_tool_call_id: request.source_tool_call_id.clone(),
        state: if error.is_some() {
            DeviceCommandState::Failed
        } else {
            DeviceCommandState::Succeeded
        },
        attempt: 1,
        cancel_requested: false,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        expires_at_ms: now_ms.saturating_add(DEFAULT_COMMAND_TTL_MS),
        lease_expires_at_ms: None,
        started_at_ms: Some(now_ms),
        completed_at_ms: Some(now_ms),
        result: output.get("result").cloned(),
        artifact,
        error,
        // The sandbox invocation *is* the execution the host authorized: there
        // is no separate runner lease to hand out, because nothing left this
        // machine to be leased.
        execution_id: Some(command_id.clone()),
        // This command reached a terminal state inside this call, so its
        // report is the only one there will ever be. Recording its digest
        // keeps the column's meaning — "the terminal report already accepted"
        // — true for extension devices as well as paired ones.
        terminal_sha256: Some(terminal_digest),
        invocation_id: request.invocation_id.clone(),
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
    // Extension-provided devices are resolved first, because their ids are the
    // only ones that can be recognised without touching the pairing database
    // and because an unnamed action has to see both kinds before it can say
    // whether the choice was ambiguous.
    let app_data = paths.app_data()?.to_path_buf();
    let extension_candidates: Vec<ExtensionDevice> = extension_devices(&app_data)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|device| device.actions.contains(&request.capability))
        .collect();
    if let Some(requested) = request.device_id.as_deref() {
        if extension_device_target(requested).is_some() {
            let device = extension_candidates
                .iter()
                .find(|device| device.device_id == requested)
                .ok_or_else(|| {
                    format!(
                        "Device '{requested}' cannot do this: no healthy extension provider \
                         advertises that device with this action."
                    )
                })?;
            return dispatch_extension(&app_data, request, device, now_ms).await;
        }
    }
    let arguments = validate_arguments(request.capability, &request.arguments)?;
    let wait_ms = request.wait_ms.clamp(1_000, MAX_WAIT_MS);
    let mut store = RemoteStore::open(&paths.root)?;
    let device_id = match resolve_target(&store, request.capability, request.device_id.as_deref()) {
        Ok(device_id) if request.device_id.is_some() || extension_candidates.is_empty() => {
            device_id
        }
        // One paired device and one extension device can both do this, and the
        // caller named neither. Guessing is worse than asking: an agent
        // photographing whichever sorted first is the outcome this refuses.
        Ok(device_id) => {
            return Err(format!(
                "{} devices can do this — name one with 'device_id': {}",
                extension_candidates.len() + 1,
                std::iter::once(device_id)
                    .chain(
                        extension_candidates
                            .iter()
                            .map(|device| format!("{} ({})", device.device_id, device.device_name)),
                    )
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        Err(error) => match extension_candidates.len() {
            0 => return Err(error),
            1 => {
                let device = extension_candidates.first().expect("length checked");
                return dispatch_extension(&app_data, request, device, now_ms).await;
            }
            _ => {
                return Err(format!(
                    "{} extension devices can do this — name one with 'device_id': {}",
                    extension_candidates.len(),
                    extension_candidates
                        .iter()
                        .map(|device| format!("{} ({})", device.device_id, device.device_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        },
    };
    // Playing a stored artifact means the device fetches it over the ordinary
    // signed artifact route, under the run scope it was already paired with.
    // Refused here rather than left to fail on the phone, because "the speaker
    // stayed silent" is a poor way to learn a grant is missing. The run scope
    // itself is not re-checked here: that is the artifact route's job, and a
    // second copy of it could only ever disagree.
    if arguments.get("artifact_id").is_some()
        && !store.device(&device_id)?.is_some_and(|device| {
            device
                .capabilities
                .contains(&DeviceCapability::ReadArtifacts)
        })
    {
        return Err(
            "Playing a stored artifact also requires the read_artifacts grant, which this device \
             does not have"
                .to_string(),
        );
    }
    let queued = store.enqueue_device_command(
        &DeviceCommandRequest {
            device_id,
            capability: request.capability,
            arguments,
            source_run_id: request.source_run_id.clone(),
            source_session_id: request.source_session_id.clone(),
            source_tool_call_id: request.source_tool_call_id.clone(),
            // A durable tool call names itself; a manual invocation does not,
            // and must not be deduplicated against an earlier one that happened
            // to look the same.
            invocation_id: request.invocation_id.clone().or_else(|| {
                DeviceCommandRequest::invocation_id_for(
                    request.source_run_id.as_deref(),
                    request.source_tool_call_id.as_deref(),
                )
            }),
            expires_at_ms: now_ms.saturating_add(DEFAULT_COMMAND_TTL_MS),
        },
        now_ms,
    )?;
    drop(store);

    // Wake the device. This is what makes the queue work on a phone whose
    // screen is off: the long poll only reconnects once something wakes the
    // app, so without this a command sits queued until someone happens to open
    // the controller. Best-effort by design — a run must never fail because a
    // notification could not be delivered, and the command is durable either
    // way.
    let _ = super::push::notify_device(
        paths,
        &queued.device_id,
        &super::push::PushNotification {
            kind: super::push::PushKind::DeviceActionAwaiting,
            target_id: Some(queued.command_id.clone()),
            detail: Some(format!(
                "{} is waiting for you",
                capability_token(request.capability).replace('_', " ")
            )),
        },
        &super::store::KeyringRemoteSecrets,
    )
    .await;

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
