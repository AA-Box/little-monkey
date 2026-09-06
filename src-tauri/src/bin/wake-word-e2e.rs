//! Wake-word acceptance driver.
//!
//! The manual acceptance script for this feature spans two runtimes: the
//! keyword spotter, Whisper and the configuration validator are Rust, while the
//! state machine that decides what any of it means is `TalkSession` in
//! TypeScript. Testing either half alone leaves the join — the part an operator
//! actually experiences — unproven.
//!
//! This binary is the Rust half, exposed as newline-delimited JSON on stdin and
//! stdout so `src/lib/wakeWordWalkthrough.e2e.test.ts` can drive the *real*
//! `TalkSession` against the *real* native runtime. Every operation here goes
//! through production code: `WakeWordManager` for arming and detection,
//! `m7_companion::validate_config` for the settings the operator toggles, and
//! `local_whisper::transcribe` with the bundled model for the command.
//!
//! It deliberately holds no policy of its own. If this file starts deciding
//! when to arm, what a detection means, or when to re-arm, the walkthrough has
//! stopped testing the product.
//!
//! PCM crosses the pipe as base64 little-endian `f32`, which is a transport
//! detail and not a retention one: nothing here writes audio anywhere except
//! the one temporary WAV that Whisper must be handed, which is removed as soon
//! as it has been read.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use little_monkey_lib::local_wake_word::{self, WakeWordManager};
use little_monkey_lib::local_whisper;
use little_monkey_lib::m7_companion::{CompanionConfig, TranscriptionBackendKind};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn resource_root() -> PathBuf {
    std::env::var_os("LITTLE_MONKEY_RESOURCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("resources"))
}

fn decode_pcm(value: Option<&Value>) -> Result<Vec<f32>, String> {
    let encoded = value
        .and_then(Value::as_str)
        .ok_or_else(|| "pcm must be a base64 string".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("decode pcm: {error}"))?;
    if bytes.len() % 4 != 0 {
        return Err("pcm must be little-endian f32".to_string());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn encode_pcm(samples: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn string_field(request: &Value, name: &str) -> Result<String, String> {
    request
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{name} is required"))
}

/// One of the two pinned upstream fixtures, handed over as PCM so the
/// TypeScript side needs no WAV decoder of its own.
fn fixture(name: &str) -> Result<Value, String> {
    if name.contains('/') || name.contains('\\') || !name.ends_with(".wav") {
        return Err("fixture must be a bare .wav name".to_string());
    }
    let path = resource_root()
        .join("local-wake-word")
        .join("test_wavs")
        .join(name);
    let wave = sherpa_onnx::Wave::read(
        path.to_str()
            .ok_or_else(|| "fixture path is not UTF-8".to_string())?,
    )
    .ok_or_else(|| format!("read fixture {name}; run `pnpm stage:wake-word` first"))?;
    Ok(json!({
        "sampleRate": wave.sample_rate(),
        "samples": wave.samples().len(),
        "pcm": encode_pcm(wave.samples()),
    }))
}

/// The operator's real save-time validator, over a real default configuration.
/// Steps 1-3 and 14 of the acceptance script are settings changes, and a
/// harness that only pretended to apply them would prove nothing about whether
/// the product accepts or refuses them.
fn configure(request: &Value) -> Result<Value, String> {
    let mut config = CompanionConfig::default();
    config.voice.backend = match request.get("transcriptionBackend").and_then(Value::as_str) {
        None | Some("local_whisper") => TranscriptionBackendKind::LocalWhisper,
        Some("provider") => {
            config.voice.provider_id = Some("acceptance-provider".to_string());
            TranscriptionBackendKind::Provider
        }
        Some(other) => return Err(format!("unknown transcription backend {other}")),
    };
    if let Some(phrase) = request.get("phrase").and_then(Value::as_str) {
        config.voice.wake_phrase = phrase.to_string();
    }
    if let Some(backend) = request.get("wakeWordBackend").and_then(Value::as_str) {
        config.voice.wake_word_backend = backend.to_string();
    }
    if let Some(sensitivity) = request.get("sensitivity").and_then(Value::as_u64) {
        config.voice.wake_word_sensitivity = u8::try_from(sensitivity).unwrap_or(u8::MAX);
    }
    config.voice.wake_phrase_enabled = request
        .get("wakePhraseEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    config.voice.always_listening = request
        .get("alwaysListening")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match little_monkey_lib::m7_companion::validate_config(&config) {
        Ok(()) => Ok(json!({
            "accepted": true,
            "wakePhraseEnabled": config.voice.wake_phrase_enabled,
            "alwaysListening": config.voice.always_listening,
            "phrase": config.voice.wake_phrase,
            "sensitivity": config.voice.wake_word_sensitivity,
        })),
        Err(refusal) => Ok(json!({ "accepted": false, "refusal": refusal })),
    }
}

/// Real bundled Whisper over the command PCM the state machine captured.
///
/// The WAV exists only because whisper.cpp is handed a path, and it is removed
/// before this returns whether the transcription succeeded or not.
async fn transcribe(request: &Value) -> Result<Value, String> {
    let samples = decode_pcm(request.get("pcm"))?;
    if samples.is_empty() {
        return Err("nothing to transcribe".to_string());
    }
    let root = std::env::temp_dir().join(format!(
        "little-monkey-wake-walkthrough-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).map_err(|error| format!("create scratch: {error}"))?;
    let path = root.join("command.wav");
    let wrote = path
        .to_str()
        .is_some_and(|path| sherpa_onnx::write(path, &samples, local_wake_word::SAMPLE_RATE));
    if !wrote {
        let _ = std::fs::remove_dir_all(&root);
        return Err("could not write the command WAV".to_string());
    }
    let transcript = local_whisper::transcribe(
        &root,
        &path,
        "en",
        local_whisper::DEFAULT_MODEL_ID,
        None,
        CancellationToken::new(),
    )
    .await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(json!({ "text": transcript?.text }))
}

async fn dispatch(manager: &WakeWordManager, request: &Value) -> Result<Value, String> {
    match string_field(request, "op")?.as_str() {
        "fixture" => fixture(&string_field(request, "name")?),
        "configure" => configure(request),
        "status" => Ok(serde_json::to_value(manager.status()?).map_err(|e| e.to_string())?),
        "arm" => {
            let phrase = string_field(request, "phrase")?;
            let sensitivity = request
                .get("sensitivity")
                .and_then(Value::as_f64)
                .unwrap_or(f64::from(local_wake_word::DEFAULT_SENSITIVITY))
                as f32;
            Ok(serde_json::to_value(manager.start(&phrase, sensitivity)?)
                .map_err(|e| e.to_string())?)
        }
        "push" => {
            let session_id = string_field(request, "sessionId")?;
            let samples = decode_pcm(request.get("pcm"))?;
            let detection = manager.push(&session_id, local_wake_word::SAMPLE_RATE, &samples)?;
            Ok(json!({ "detection": detection }))
        }
        "disarm" => {
            let session_id = string_field(request, "sessionId")?;
            let dropped = request
                .get("droppedFrames")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Ok(json!({ "stopped": manager.stop(&session_id, dropped)? }))
        }
        "transcribe" => transcribe(request).await,
        "reportFalseTrigger" => {
            Ok(serde_json::to_value(manager.report_false_trigger()?).map_err(|e| e.to_string())?)
        }
        other => Err(format!("unknown op {other}")),
    }
}

#[tokio::main]
async fn main() {
    let root = resource_root();
    local_wake_word::set_resource_dir(Some(&root));
    local_whisper::set_resource_dir(Some(&root));
    let manager = WakeWordManager::default();

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // Ready before the first request, so the driver waits for the native
    // runtime's own startup rather than racing it with a sleep.
    let _ = writeln!(stdout, "{}", json!({ "ready": true }));
    let _ = stdout.flush();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => match dispatch(&manager, &request).await {
                Ok(result) => json!({ "ok": true, "result": result }),
                Err(error) => json!({ "ok": false, "error": error }),
            },
            Err(error) => json!({ "ok": false, "error": format!("bad request: {error}") }),
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}
