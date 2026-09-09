//! Host-authoritative Talk routing across local and paired-device endpoints.
//!
//! This module is deliberately a control plane, not an audio transport. Paired
//! endpoints reuse the existing physical capability grants, durable command
//! queue and authenticated Talk WebSocket. The route row carries only endpoint
//! identity, generation and bounded coordination events; raw microphone or TTS
//! audio never lands here.

use super::protocol::{validate_id, DeviceCapability};
use super::store::{DeviceCommandRequest, RemoteStore, VoiceRouteEventRecord, VoiceRouteRecord};
use crate::daemon::store::DaemonPaths;

pub const LOCAL_INPUT_DEFAULT: &str = "local:input:default";
pub const LOCAL_OUTPUT_DEFAULT: &str = "local:output:default";
const ROUTE_COMMAND_TTL_MS: u64 = 60 * 60 * 1_000;

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
    pub ready: bool,
    pub blocked_by: Option<String>,
}

pub fn endpoints(paths: &DaemonPaths) -> Result<Vec<EndpointDescriptor>, String> {
    let store = RemoteStore::open(&paths.root)?;
    let mut result = vec![
        EndpointDescriptor {
            id: LOCAL_INPUT_DEFAULT.to_string(),
            label: "This computer — default microphone".to_string(),
            direction: "input",
            locality: "local",
            device_id: None,
            ready: true,
            blocked_by: None,
        },
        EndpointDescriptor {
            id: LOCAL_OUTPUT_DEFAULT.to_string(),
            label: "This computer — default speaker".to_string(),
            direction: "output",
            locality: "local",
            device_id: None,
            ready: true,
            blocked_by: None,
        },
    ];
    for device in store.devices()?.into_iter().filter(|device| device.active()) {
        for (direction, capability) in [
            ("input", DeviceCapability::VoiceStream),
            ("output", DeviceCapability::AudioPlayback),
        ] {
            let endpoint = if direction == "input" {
                format!("paired:{}:input", device.device_id)
            } else {
                format!("paired:{}:output", device.device_id)
            };
            let readiness = super::device::resolve_target(&store, capability, Some(&device.device_id));
            let (ready, blocked_by) = match readiness {
                Ok(_) => (true, None),
                Err(error) => (false, Some(error)),
            };
            result.push(EndpointDescriptor {
                id: endpoint,
                label: format!("{} — {}", device.device_name, if direction == "input" { "microphone" } else { "speaker" }),
                direction,
                locality: "paired",
                device_id: Some(device.device_id.clone()),
                ready,
                blocked_by,
            });
        }
    }
    Ok(result)
}

fn validate_endpoint(store: &RemoteStore, endpoint: &AudioEndpoint) -> Result<(), String> {
    match  endpoint {
        AudioEndpoint::LocalInput(_) | AudioEndpoint::LocalOutput(_) => Ok(()),
        AudioEndpoint::PairedInput(device_id) => {
            super::device::resolve_target(store, DeviceCapability::VoiceStream, Some(device_id))?;
            Ok(())
        }
        AudioEndpoint::PairedOutput(device_id) => {
            super::device::resolve_target(store, DeviceCapability::AudioPlayback, Some(device_id))?;
            Ok(())
        }
    }
}

fn cancel_command(store: &mut RemoteStore, command_id: Option<&str>, now_ms: u64) {
    if let Some(command_id) = command_id {
        let _ = store.request_device_cancel(command_id, now_ms);
    }
}

fn retire_input_command(
    store: &mut RemoteStore,
    command_id: Option<&str>,
    now_ms: u64,
) -> Result<(), String> {
    let Some(command_id) = command_id else { return Ok(()); };
    store.request_device_cancel(command_id, now_ms)?;
    // The device's physical executor watches `cancel_requested` while a routed
    // microphone is running. Do not return control to a handoff until that old
    // owner has acknowledged termination, otherwise a newly selected endpoint
    // could overlap it for one lease tick.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let Some(command) = store.device_command(command_id)? else { return Ok(()); };
        if command.state.terminal() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("The previous VoiceRoute microphone did not release within 5 seconds; the new microphone was not activated".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn enqueue_role(
    store: &mut RemoteStore,
    device_id: &str,
    capability: DeviceCapability,
    session_id: &str,
    route_id: &str,
    generation: u64,
    role: &str,
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
    let input = AudioEndpoint::parse(input)?;
    let output = AudioEndpoint::parse(output)?;
    if !matches!(input, AudioEndpoint::LocalInput(_) | AudioEndpoint::PairedInput(_)) {
        return Err("Voice route input endpoint is not an input".to_string());
    }
    if !matches!(output, AudioEndpoint::LocalOutput(_) | AudioEndpoint::PairedOutput(_)) {
        return Err("Voice route output endpoint is not an output".to_string());
    }

    let mut store = RemoteStore::open(&paths.root)?;
    validate_endpoint(&store, &input)?;
    validate_endpoint(&store, &output)?;
    let previous = store.voice_route(session_id)?;
    // Prepare -> retire -> commit. The new endpoints are validated above while
    // the old route is untouched. We then wait for the old microphone command
    // to acknowledge cancellation before minting the new generation, so there
    // is never an intentional two-microphone ownership window.
    if let Some(previous) = previous.as_ref() {
        retire_input_command(&mut store, previous.input_command_id.as_deref(), now_ms)?;
        if previous.output_command_id != previous.input_command_id {
            cancel_command(&mut store, previous.output_command_id.as_deref(), now_ms);
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
    if route.engine == "realtime"
        && (route.input_endpoint.starts_with("paired:") || route.output_endpoint.starts_with("paired:"))
    {
        return Err("Paired Realtime Voice needs a direct media bridge; use local endpoints or Pipeline".to_string());
    }
    if route.input_command_id.is_some() || route.output_command_id.is_some() {
        return Ok(route);
    }
    let input = AudioEndpoint::parse(&route.input_endpoint)?;
    let output = AudioEndpoint::parse(&route.output_endpoint)?;
    validate_endpoint(&store, &input)?;
    validate_endpoint(&store, &output)?;

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
    store.set_voice_route_commands(
        session_id,
        route.generation,
        input_command_id.as_deref(),
        output_command_id.as_deref(),
        now_ms,
    )
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
