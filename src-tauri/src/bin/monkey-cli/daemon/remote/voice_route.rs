//! Host-authoritative Talk routing across local and paired-device endpoints.
//!
//! This module is deliberately a control plane, not an audio transport. Paired
//! endpoints reuse the existing physical capability grants, durable command
//! queue and authenticated Talk WebSocket. The route row carries only endpoint
//! identity, generation and bounded coordination events; raw microphone or TTS
//! audio never lands here.

use super::protocol::{
    capability_block, legacy_capabilities, validate_id, DeviceCapability, DeviceReadiness, OsPermission,
};
use super::store::{DeviceCommandRequest, RemoteStore, VoiceRouteEventRecord, VoiceRouteRecord};
use crate::daemon::store::DaemonPaths;

pub const LOCAL_INPUT_DEFAULT: &str = "local:input:default";
pub const LOCAL_OUTPUT_DEFAULT: &str = "local:output:default";
const ROUTE_COMMAND_TTL_MS: u64 = 60 * 60 * 1_000;
/// Paired controllers long-poll for at most 25 seconds and retry network failures
/// after 5 seconds. Past this bound the host no longer has evidence that the
/// endpoint is currently reachable, so VoiceRoute fails closed instead of
/// queueing a microphone start on an offline phone.
const ENDPOINT_ONLINE_WINDOW_MS: u64 = 45_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEndpoint {
    LocalInput(String),
    LocalOutput(String),
    PairedInput(String),
    PairedOutput(String),
}

impl AudioEndpoint {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(id) = value.strip_prefix("local:input:") {
            if id.is_empty() || id.len() > 384 || id.chars().any(char::is_control) {
                return Err("Local input endpoint id is invalid".to_string());
            }
            return Ok(Self::LocalInput(id.to_string()));
        }
        if let Some(id) = value.strip_prefix("local:output:") {
            if id.is_empty() || id.len() > 384 || id.chars().any(char::is_control) {
                return Err("Local output endpoint id is invalid".to_string());
            }
            return Ok(Self::LocalOutput(id.to_string()));
        }
        if let Some(rest) = value.strip_prefix("paired:") {
            if let Some(device_id) = rest.strip_suffix(":input") {
                validate_id(device_id)?;
                return Ok(Self::PairedInput(device_id.to_string()));
            }
            if let Some(device_id) = rest.strip_suffix(":output") {
                validate_id(device_id)?;
                return Ok(Self::PairedOutput(device_id.to_string()));
            }
        }
        Err(format!("Unknown audio endpoint '{value}'"))
    }

    pub fn token(&self) -> String {
        match self {
            Self::LocalInput(id) => format!("local:input:{id}"),
            Self::LocalOutput(id) => format!("local:output:{id}"),
            Self::PairedInput(id) => format!("paired:{id}:input"),
            Self::PairedOutput(id) => format!("paired:{id}:output"),
        }
    }

    pub fn paired_device(&self) -> Option<&str> {
        match self {
            Self::PairedInput(id) | Self::PairedOutput(id) => Some(id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EndpointDescriptor {
    pub id: String,
    pub label: String,
    pub direction: &'static str,
    pub locality: &'static str,
    pub device_id: Option<String>,
    /// Capability truth is advertised by the endpoint surface. It is never
    /// guessed from a platform or device name.
    pub input_supported: bool,
    pub output_supported: bool,
    pub voice_stream_supported: bool,
    pub os_permission: Option<OsPermission>,
    pub readiness: Option<DeviceReadiness>,
    pub foreground_required: bool,
    pub interaction_required: bool,
    /// Whether the host has recent signed contact from the paired controller.
    /// Local endpoints are process-local and therefore always online here;
    /// hardware/OS permission is still resolved by the WebView itself.
    pub online: bool,
    pub last_seen_at_ms: Option<u64>,
    /// Reserved for measured route setup/media latency. `None` means unknown,
    /// never zero.
    pub latency_ms: Option<u64>,
    pub ready: bool,
    pub blocked_code: Option<String>,
    pub blocked_by: Option<String>,
}

fn device_online(last_seen_at_ms: Option<u64>, surface_seen_at_ms: Option<u64>, now_ms: u64) -> bool {
    last_seen_at_ms
        .into_iter()
        .chain(surface_seen_at_ms)
        .max()
        .is_some_and(|seen| now_ms.saturating_sub(seen) <= ENDPOINT_ONLINE_WINDOW_MS)
}

pub fn endpoints(paths: &DaemonPaths) -> Result<Vec<EndpointDescriptor>, String> {
    let store = RemoteStore::open(&paths.root)?;
    let now_ms = super::now_ms_public()?;
    let mut result = vec![
        EndpointDescriptor {
            id: LOCAL_INPUT_DEFAULT.to_string(),
            label: "This computer — default microphone".to_string(),
            direction: "input",
            locality: "local",
            device_id: None,
            input_supported: true,
            output_supported: false,
            voice_stream_supported: true,
            os_permission: None,
            readiness: None,
            foreground_required: false,
            interaction_required: false,
            online: true,
            last_seen_at_ms: None,
            latency_ms: None,
            ready: true,
            blocked_code: None,
            blocked_by: None,
        },
        EndpointDescriptor {
            id: LOCAL_OUTPUT_DEFAULT.to_string(),
            label: "This computer — default speaker".to_string(),
            direction: "output",
            locality: "local",
            device_id: None,
            input_supported: false,
            output_supported: true,
            voice_stream_supported: false,
            os_permission: None,
            readiness: None,
            foreground_required: false,
            interaction_required: false,
            online: true,
            last_seen_at_ms: None,
            latency_ms: None,
            ready: true,
            blocked_code: None,
            blocked_by: None,
        },
    ];
    for device in store.devices()?.into_iter().filter(|device| device.active()) {
        let surface = store.device_surface(&device.device_id)?;
        let granted = if device.capabilities.is_empty() {
            legacy_capabilities(&device.scopes)
        } else {
            device.capabilities.clone()
        };
        let surface_seen = surface.as_ref().map(|value| value.reported_at_ms);
        let last_seen = device.last_seen_at_ms.into_iter().chain(surface_seen).max();
        let online = device_online(device.last_seen_at_ms, surface_seen, now_ms);
        for (direction, capability) in [
            ("input", DeviceCapability::VoiceStream),
            ("output", DeviceCapability::AudioPlayback),
        ] {
            let endpoint = if direction == "input" {
                format!("paired:{}:input", device.device_id)
            } else {
                format!("paired:{}:output", device.device_id)
            };
            let permission = surface.as_ref().map(|value| value.permission(capability));
            let readiness = surface.as_ref().map(|value| value.readiness(capability));
            let block = capability_block(&granted, surface.as_ref(), capability);
            let (blocked_code, blocked_by) = if !online {
                (Some("offline".to_string()), Some("Paired device is offline or has not made signed contact recently.".to_string()))
            } else if let Some(block) = block {
                (Some(block.as_str().to_string()), Some(block.explain(capability)))
            } else {
                (None, None)
            };
            let input_supported = surface.as_ref().is_some_and(|value| value.capabilities.contains(&DeviceCapability::VoiceStream));
            let output_supported = surface.as_ref().is_some_and(|value| value.capabilities.contains(&DeviceCapability::AudioPlayback));
            result.push(EndpointDescriptor {
                id: endpoint,
                label: format!("{} — {}", device.device_name, if direction == "input" { "microphone" } else { "speaker" }),
                direction,
                locality: "paired",
                device_id: Some(device.device_id.clone()),
                input_supported,
                output_supported,
                voice_stream_supported: input_supported,
                os_permission: permission,
                readiness,
                foreground_required: matches!(readiness, Some(DeviceReadiness::ForegroundRequired)),
                interaction_required: matches!(readiness, Some(DeviceReadiness::InteractionRequired)),
                online,
                last_seen_at_ms: last_seen,
                latency_ms: None,
                ready: online && block.is_none(),
                blocked_code,
                blocked_by,
            });
        }
    }
    Ok(result)
}

fn validate_endpoint(store: &RemoteStore, endpoint: &AudioEndpoint, now_ms: u64) -> Result<(), String> {
    match endpoint {
        AudioEndpoint::LocalInput(_) | AudioEndpoint::LocalOutput(_) => Ok(()),
        AudioEndpoint::PairedInput(device_id) | AudioEndpoint::PairedOutput(device_id) => {
            let capability = if matches!(endpoint, AudioEndpoint::PairedInput(_)) {
                DeviceCapability::VoiceStream
            } else {
                DeviceCapability::AudioPlayback
            };
            super::device::resolve_target(store, capability, Some(device_id))?;
            let device = store
                .device(device_id)?
                .ok_or_else(|| format!("Paired device '{device_id}' no longer exists"))?;
            let surface_seen = store.device_surface(device_id)?.map(|value| value.reported_at_ms);
            if !device_online(device.last_seen_at_ms, surface_seen, now_ms) {
                return Err("Paired VoiceRoute endpoint is offline or has not made signed contact recently".to_string());
            }
            Ok(())
        }
    }
}

fn cancel_command(store: &mut RemoteStore, command_id: Option<&str>, now_ms: u64) {
    if let Some(command_id) = command_id {
        let _ = store.request_device_cancel(command_id, now_ms);
    }
}

fn retire_command(
    store: &mut RemoteStore,
    command_id: Option<&str>,
    now_ms: u64,
    role: &str,
) -> Result<(), String> {
    let Some(command_id) = command_id else { return Ok(()); };
    store.request_device_cancel(command_id, now_ms)?;
    // The physical executor watches `cancel_requested` while a routed role is
    // running. A handoff does not commit its next generation until the previous
    // owner has acknowledged termination. That keeps microphone ownership
    // exclusive and also prevents old speaker work from bleeding into the new
    // route after the UI already says the move completed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let Some(command) = store.device_command(command_id)? else { return Ok(()); };
        if command.state.terminal() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "The previous VoiceRoute {role} did not release within 5 seconds; the new route was not committed"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn wait_for_prepare(store: &RemoteStore, command_id: &str, role: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let Some(command) = store.device_command(command_id)? else {
            return Err(format!("VoiceRoute {role} preparation disappeared before acknowledgement"));
        };
        if command.state.terminal() {
            return if command.state == super::protocol::DeviceCommandState::Succeeded {
                Ok(())
            } else {
                Err(command.error.unwrap_or_else(|| format!("VoiceRoute {role} preparation was not accepted by the device")))
            };
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("VoiceRoute {role} preparation was not acknowledged within 8 seconds"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn prepare_paired_endpoint(
    store: &mut RemoteStore,
    endpoint: &AudioEndpoint,
    session_id: &str,
    route_id: &str,
    generation: u64,
    engine: &str,
    now_ms: u64,
) -> Result<(), String> {
    let (device_id, capability, role) = match endpoint {
        AudioEndpoint::PairedInput(device_id) => (device_id.as_str(), DeviceCapability::VoiceStream, "input"),
        AudioEndpoint::PairedOutput(device_id) => (device_id.as_str(), DeviceCapability::AudioPlayback, "output"),
        _ => return Ok(()),
    };
    validate_endpoint(store, endpoint, now_ms)?;
    let command = store.enqueue_device_command(
        &DeviceCommandRequest {
            device_id: device_id.to_string(),
            capability,
            arguments: serde_json::json!({
                "mode": "talk_route_prepare",
                "role": role,
                "session_id": session_id,
                "route_id": route_id,
                "route_generation": generation,
                "engine": engine,
            }),
            source_run_id: None,
            source_session_id: Some(session_id.to_string()),
            source_tool_call_id: None,
            invocation_id: Some(format!("voice-route-prepare:{session_id}:{generation}:{role}:{device_id}")),
            expires_at_ms: now_ms.saturating_add(15_000),
        },
        now_ms,
    )?;
    wait_for_prepare(store, &command.command_id, role)
}

fn enqueue_role(
    store: &mut RemoteStore,
    device_id: &str,
    capability: DeviceCapability,
    session_id: &str,
    route_id: &str,
    generation: u64,
    role: &str,
    engine: &str,
    now_ms: u64,
) -> Result<String, String> {
    let expires_at_ms = now_ms.saturating_add(ROUTE_COMMAND_TTL_MS);
    let command = store.enqueue_device_command(
        &DeviceCommandRequest {
            device_id: device_id.to_string(),
            capability,
            arguments: serde_json::json!({
                "mode": "talk_route",
                "role": role,
                "session_id": session_id,
                "route_id": route_id,
                "route_generation": generation,
                "engine": engine,
                "duration_ms": ROUTE_COMMAND_TTL_MS,
            }),
            source_run_id: None,
            source_session_id: Some(session_id.to_string()),
            source_tool_call_id: None,
            invocation_id: Some(format!("voice-route:{session_id}:{generation}:{role}:{device_id}")),
            expires_at_ms,
        },
        now_ms,
    )?;
    Ok(command.command_id)
}

/// Validates the destination first, commits a fresh generation, retires the old
/// roles and then activates the new roles. Validation is the prepare phase: it
/// checks the existing grant + advertised support + OS permission + readiness
/// without opening a second microphone. Therefore an input handoff never has two
/// live microphones at once.
pub fn set_route(
    paths: &DaemonPaths,
    session_id: &str,
    engine: &str,
    input: &str,
    output: &str,
    now_ms: u64,
) -> Result<VoiceRouteRecord, String> {
    validate_id(session_id)?;
    if !matches!(engine, "pipeline" | "realtime") {
        return Err("Voice route engine must be pipeline or realtime".to_string());
    }
    let input = AudioEndpoint::parse(input)?;
    let output = AudioEndpoint::parse(output)?;
    if !matches!(input, AudioEndpoint::LocalInput(_) | AudioEndpoint::PairedInput(_)) {
        return Err("Voice route input endpoint is not an input".to_string());
    }
    if !matches!(output, AudioEndpoint::LocalOutput(_) | AudioEndpoint::PairedOutput(_)) {
        return Err("Voice route output endpoint is not an output".to_string());
    }

    let mut store = RemoteStore::open(&paths.root)?;
    validate_endpoint(&store, &input, now_ms)?;
    validate_endpoint(&store, &output, now_ms)?;
    let previous = store.voice_route(session_id)?;
    // Prepare -> retire -> commit. The new endpoints are validated above while
    // the old route is untouched. We then wait for the old microphone command
    // to acknowledge cancellation before minting the new generation, so there
    // is never an intentional two-microphone ownership window.
    if let Some(previous) = previous.as_ref() {
        retire_command(
            &mut store,
            previous.input_command_id.as_deref(),
            now_ms,
            "microphone",
        )?;
        if previous.output_command_id != previous.input_command_id {
            retire_command(
                &mut store,
                previous.output_command_id.as_deref(),
                now_ms,
                "speaker",
            )?;
        }
    }
    store.replace_voice_route(
        session_id,
        engine,
        &input.token(),
        &output.token(),
        None,
        None,
        now_ms,
    )
}

pub fn activate_route(paths: &DaemonPaths, session_id: &str, now_ms: u64) -> Result<VoiceRouteRecord, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let route = store
        .voice_route(session_id)?
        .ok_or_else(|| "No voice route exists for this conversation".to_string())?;
    if route.state != "active" {
        return Err("Voice route is stopped".to_string());
    }
    if route.input_command_id.is_some() || route.output_command_id.is_some() {
        return Ok(route);
    }
    let input = AudioEndpoint::parse(&route.input_endpoint)?;
    let output = AudioEndpoint::parse(&route.output_endpoint)?;
    validate_endpoint(&store, &input, now_ms)?;
    validate_endpoint(&store, &output, now_ms)?;

    let paired_input = match &input { AudioEndpoint::PairedInput(id) => Some(id.as_str()), _ => None };
    let paired_output = match &output { AudioEndpoint::PairedOutput(id) => Some(id.as_str()), _ => None };
    let mut input_command_id = None;
    let mut output_command_id = None;

    if let Some(device_id) = paired_input {
        let role = if paired_output == Some(device_id) { "duplex" } else { "input" };
        input_command_id = Some(enqueue_role(
            &mut store,
            device_id,
            DeviceCapability::VoiceStream,
            session_id,
            &route.route_id,
            route.generation,
            role,
            &route.engine,
            now_ms,
        )?);
    }
    if let Some(device_id) = paired_output {
        if paired_input != Some(device_id) {
            match enqueue_role(
                &mut store,
                device_id,
                DeviceCapability::AudioPlayback,
                session_id,
                &route.route_id,
                route.generation,
                "output",
                &route.engine,
                now_ms,
            ) {
                Ok(command_id) => output_command_id = Some(command_id),
                Err(error) => {
                    cancel_command(&mut store, input_command_id.as_deref(), now_ms);
                    return Err(error);
                }
            }
        } else {
            output_command_id = input_command_id.clone();
        }
    }
    match store.set_voice_route_commands(
        session_id,
        route.generation,
        input_command_id.as_deref(),
        output_command_id.as_deref(),
        now_ms,
    ) {
        Ok(route) => Ok(route),
        Err(error) => {
            cancel_command(&mut store, input_command_id.as_deref(), now_ms);
            if output_command_id != input_command_id {
                cancel_command(&mut store, output_command_id.as_deref(), now_ms);
            }
            Err(error)
        }
    }
}

/// Changes a route that may already be live. Selection itself stays privacy
/// preserving — an inactive route never opens a microphone — but when an old
/// generation already owns a paired role this performs the full handoff before
/// returning. A failed destination activation rolls forward to a fresh
/// generation containing the previous endpoints and re-acquires those roles.
/// Reusing the old generation would make delayed frames from the failed move
/// indistinguishable from current media, so rollback is deliberately another
/// generation rather than a database rewind.
pub fn move_route(
    paths: &DaemonPaths,
    session_id: &str,
    input: Option<&str>,
    output: Option<&str>,
    now_ms: u64,
) -> Result<VoiceRouteRecord, String> {
    let previous = route(paths, session_id)?
        .ok_or_else(|| format!("No active voice route for '{session_id}'"))?;
    let was_live = previous.input_command_id.is_some() || previous.output_command_id.is_some();
    let next_input = AudioEndpoint::parse(input.unwrap_or(&previous.input_endpoint))?;
    let next_output = AudioEndpoint::parse(output.unwrap_or(&previous.output_endpoint))?;
    // Real prepare phase: while the working route is still untouched, require
    // each changed paired destination to execute a bounded, non-media command
    // and report its current surface readiness. Only after those acknowledgements
    // do we retire the old capture owner and commit the new generation.
    if was_live {
        let mut store = RemoteStore::open(&paths.root)?;
        let next_generation = previous.generation.checked_add(1)
            .ok_or_else(|| "Voice route generation is exhausted".to_string())?;
        if next_input.token() != previous.input_endpoint {
            prepare_paired_endpoint(&mut store, &next_input, session_id, &previous.route_id, next_generation, &previous.engine, now_ms)?;
        }
        if next_output.token() != previous.output_endpoint {
            prepare_paired_endpoint(&mut store, &next_output, session_id, &previous.route_id, next_generation, &previous.engine, now_ms)?;
        }
    }
    let selected = set_route(
        paths,
        session_id,
        &previous.engine,
        input.unwrap_or(&previous.input_endpoint),
        output.unwrap_or(&previous.output_endpoint),
        now_ms,
    )?;
    if !was_live {
        return Ok(selected);
    }
    match activate_route(paths, session_id, now_ms) {
        Ok(active) => Ok(active),
        Err(activation_error) => {
            // Best effort retirement of anything the failed activation managed
            // to publish. `activate_route` also cancels its local command ids
            // on failure, but reading the authoritative row here closes the
            // race where command ids were committed immediately before another
            // endpoint rejected.
            if let Ok(mut store) = RemoteStore::open(&paths.root) {
                if let Ok(Some(failed)) = store.voice_route(session_id) {
                    cancel_command(&mut store, failed.input_command_id.as_deref(), now_ms);
                    if failed.output_command_id != failed.input_command_id {
                        cancel_command(&mut store, failed.output_command_id.as_deref(), now_ms);
                    }
                }
                let _ = store.replace_voice_route(
                    session_id,
                    &previous.engine,
                    &previous.input_endpoint,
                    &previous.output_endpoint,
                    None,
                    None,
                    now_ms,
                );
            }
            match activate_route(paths, session_id, now_ms) {
                Ok(_) => Err(format!(
                    "VoiceRoute handoff failed ({activation_error}); the previous endpoints were restored under a fresh generation"
                )),
                Err(rollback_error) => Err(format!(
                    "VoiceRoute handoff failed ({activation_error}); restoring the previous endpoints also failed ({rollback_error})"
                )),
            }
        }
    }
}

pub fn deactivate_route(paths: &DaemonPaths, session_id: &str, now_ms: u64) -> Result<Option<VoiceRouteRecord>, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let Some(route) = store.voice_route(session_id)? else { return Ok(None); };
    cancel_command(&mut store, route.input_command_id.as_deref(), now_ms);
    if route.output_command_id != route.input_command_id {
        cancel_command(&mut store, route.output_command_id.as_deref(), now_ms);
    }
    if route.state != "active" {
        return Ok(Some(route));
    }
    store
        .set_voice_route_commands(session_id, route.generation, None, None, now_ms)
        .map(Some)
}

pub fn stop_route(paths: &DaemonPaths, session_id: &str, now_ms: u64) -> Result<Option<VoiceRouteRecord>, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let previous = store.voice_route(session_id)?;
    if let Some(previous) = previous.as_ref() {
        cancel_command(&mut store, previous.input_command_id.as_deref(), now_ms);
        if previous.output_command_id != previous.input_command_id {
            cancel_command(&mut store, previous.output_command_id.as_deref(), now_ms);
        }
    }
    store.stop_voice_route(session_id, now_ms)
}

pub fn route(paths: &DaemonPaths, session_id: &str) -> Result<Option<VoiceRouteRecord>, String> {
    RemoteStore::open(&paths.root)?.voice_route(session_id)
}

pub fn append_event(
    paths: &DaemonPaths,
    session_id: &str,
    generation: u64,
    kind: &str,
    payload: &serde_json::Value,
    now_ms: u64,
) -> Result<VoiceRouteEventRecord, String> {
    RemoteStore::open(&paths.root)?.append_voice_route_event(session_id, generation, kind, payload, now_ms)
}

pub fn events(
    paths: &DaemonPaths,
    session_id: &str,
    after: u64,
    limit: u32,
) -> Result<Vec<VoiceRouteEventRecord>, String> {
    RemoteStore::open(&paths.root)?.voice_route_events(session_id, after, limit)
}

pub fn route_json(route: &VoiceRouteRecord) -> serde_json::Value {
    serde_json::json!({
        "session_id": route.session_id,
        "route_id": route.route_id,
        "generation": route.generation,
        "engine": route.engine,
        "input_endpoint": route.input_endpoint,
        "output_endpoint": route.output_endpoint,
        "state": route.state,
        "input_command_id": route.input_command_id,
        "output_command_id": route.output_command_id,
        "created_at_ms": route.created_at_ms,
        "updated_at_ms": route.updated_at_ms,
    })
}

pub fn event_json(event: &VoiceRouteEventRecord) -> serde_json::Value {
    serde_json::json!({
        "event_id": event.event_id,
        "session_id": event.session_id,
        "generation": event.generation,
        "kind": event.kind,
        "payload": event.payload,
        "created_at_ms": event.created_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_ids_are_directional_and_round_trip() {
        for value in [
            LOCAL_INPUT_DEFAULT,
            LOCAL_OUTPUT_DEFAULT,
            "paired:device-phone:input",
            "paired:device-phone:output",
            "local:input:BuiltInMic",
            "local:output:BuiltInSpeaker",
        ] {
            assert_eq!(AudioEndpoint::parse(value).unwrap().token(), value);
        }
        assert!(AudioEndpoint::parse("paired:device-phone").is_err());
    }
}
