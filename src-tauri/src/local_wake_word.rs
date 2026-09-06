//! Local streaming keyword spotting for Talk.
//!
//! Passive microphone PCM stops here. The browser sends bounded 16 kHz mono
//! frames, this module feeds them to sherpa-onnx's open-vocabulary KWS decoder,
//! and the only value returned on success is a wake event plus aggregate timing.
//! No audio, decoded tokens, or phrase text is persisted or logged.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use sentencepiece_rs::SentencePieceProcessor;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig, OnlineStream};

pub const BACKEND_ID: &str = "sherpa_onnx";
pub const RUNTIME_VERSION: &str = "1.13.3";
pub const MODEL_ID: &str = "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01";
pub const MODEL_LICENSE: &str = "Apache-2.0";
pub const SAMPLE_RATE: i32 = 16_000;
pub const DEFAULT_SENSITIVITY: f32 = 0.5;

const BUNDLED_DIRECTORY: &str = "local-wake-word";
const MAX_FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 5; // 200 ms
const MAX_PHRASE_BYTES: usize = 128;

#[derive(Clone, Copy)]
struct ModelFile {
    name: &'static str,
    bytes: u64,
    sha256: &'static str,
}

const MODEL_FILES: &[ModelFile] = &[
    ModelFile {
        name: "encoder-epoch-12-avg-2-chunk-16-left-64.onnx",
        bytes: 12_174_219,
        sha256: "063fbc1aeae8a9b574607a331a00e60371846ef9eaa3c1d9ea48176665dfc693",
    },
    ModelFile {
        name: "decoder-epoch-12-avg-2-chunk-16-left-64.onnx",
        bytes: 1_063_189,
        sha256: "f61ebd3eed3773a44d088d53dfae92dbb6aec4839f4dcaee2d402414741663a3",
    },
    ModelFile {
        name: "joiner-epoch-12-avg-2-chunk-16-left-64.onnx",
        bytes: 642_462,
        sha256: "0d7a37e749d8055223029318d6ffae82db1dae2d315d0892a68ba5dad17c1d2d",
    },
    ModelFile {
        name: "tokens.txt",
        bytes: 5_006,
        sha256: "fd2ded4050a55d2b1578870ba8697d02371980217806b7558bd0a5cc60f3ba53",
    },
    ModelFile {
        name: "bpe.model",
        bytes: 244_837,
        sha256: "c8a2a0129c4ab8e463164c142f82d25649661b122c8cd0b7aab5c9e80b90ad24",
    },
];

const MODEL_BYTES: u64 = 14_129_713;
static BUNDLED_MODEL: OnceLock<Option<ModelPaths>> = OnceLock::new();

#[derive(Clone)]
struct ModelPaths {
    root: PathBuf,
}

impl ModelPaths {
    fn file(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }
}

pub fn set_resource_dir(resource_dir: Option<&Path>) {
    let _ = BUNDLED_MODEL.set(resolve_bundled(resource_dir));
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("Read wake-word model: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn resolve_bundled(resource_dir: Option<&Path>) -> Option<ModelPaths> {
    let root = resource_dir?.join(BUNDLED_DIRECTORY);
    for expected in MODEL_FILES {
        let path = root.join(expected.name);
        let metadata = fs::metadata(&path).ok()?;
        if !metadata.is_file()
            || metadata.len() != expected.bytes
            || sha256_file(&path).ok()?.as_str() != expected.sha256
        {
            return None;
        }
    }
    Some(ModelPaths { root })
}

fn bundled_model() -> Option<ModelPaths> {
    BUNDLED_MODEL.get().cloned().flatten()
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WakeWordRuntimeStatus {
    pub backend: String,
    pub local: bool,
    pub runtime_version: String,
    pub model_id: String,
    pub model_license: String,
    pub available: bool,
    pub loaded: bool,
    pub accepting_audio: bool,
    pub sample_rate: i32,
    pub model_bytes: u64,
    pub model_memory_bytes: Option<u64>,
    pub idle_cpu_percent: Option<f64>,
    pub average_inference_ms: Option<f64>,
    pub average_detection_latency_ms: Option<f64>,
    pub detections: u64,
    pub dropped_frames: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WakeWordSessionStarted {
    pub session_id: String,
    pub status: WakeWordRuntimeStatus,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WakeWordDetection {
    pub detected: bool,
    pub session_id: String,
    /// Sample offset, relative to this KWS session, immediately after the
    /// decoded keyword. The browser uses it to keep an immediate command while
    /// excluding the wake phrase itself from Whisper input.
    pub keyword_end_sample: u64,
    pub inference_ms: f64,
}

struct WakeWordEngine {
    spotter: KeywordSpotter,
    tokenizer: SentencePieceProcessor,
    vocabulary: BTreeSet<String>,
}

impl WakeWordEngine {
    fn load(paths: &ModelPaths) -> Result<Self, String> {
        let actual_runtime = sherpa_onnx::version();
        if actual_runtime != RUNTIME_VERSION {
            return Err(format!(
                "Verified wake-word runtime mismatch: expected {RUNTIME_VERSION}, loaded {actual_runtime}"
            ));
        }
        let mut config = KeywordSpotterConfig::default();
        config.model_config.transducer.encoder = Some(paths.file(MODEL_FILES[0].name));
        config.model_config.transducer.decoder = Some(paths.file(MODEL_FILES[1].name));
        config.model_config.transducer.joiner = Some(paths.file(MODEL_FILES[2].name));
        config.model_config.tokens = Some(paths.file("tokens.txt"));
        config.model_config.provider = Some("cpu".to_string());
        config.model_config.num_threads = 1;
        config.model_config.debug = false;
        config.keywords_buf = Some("▁HE Y ▁S I RI".to_string());
        let spotter = KeywordSpotter::create(&config)
            .ok_or_else(|| "sherpa-onnx could not open the wake-word model".to_string())?;
        let tokenizer = SentencePieceProcessor::open(paths.root.join("bpe.model"))
            .map_err(|error| format!("Open wake-word tokenizer: {error}"))?;
        let vocabulary = fs::read_to_string(paths.root.join("tokens.txt"))
            .map_err(|error| format!("Read wake-word tokens: {error}"))?
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_string)
            .collect();
        Ok(Self {
            spotter,
            tokenizer,
            vocabulary,
        })
    }

    fn keyword_tokens(&self, phrase: &str, sensitivity: f32) -> Result<String, String> {
        validate_phrase(phrase)?;
        let pieces = self
            .tokenizer
            .encode(&phrase.trim().to_uppercase())
            .map_err(|error| format!("Compile wake phrase: {error}"))?;
        if pieces.is_empty()
            || pieces
                .iter()
                .any(|piece| piece == "<unk>" || !self.vocabulary.contains(piece.as_str()))
        {
            return Err(
                "The wake phrase contains sounds this English KWS model cannot encode".to_string(),
            );
        }
        let threshold = sensitivity_threshold(sensitivity)?;
        Ok(format!("{} :1.5 #{threshold:.3}", pieces.join(" ")))
    }
}

struct WakeWordSession {
    id: String,
    stream: OnlineStream,
    accepted_samples: u64,
}

#[derive(Default)]
struct RuntimeMetrics {
    inference_samples: u64,
    total_inference_micros: u128,
    detection_samples: u64,
    total_detection_micros: u128,
    detections: u64,
    dropped_frames: u64,
    last_error: Option<String>,
}

#[derive(Default)]
struct ManagerInner {
    engine: Option<Arc<WakeWordEngine>>,
    session: Option<WakeWordSession>,
    metrics: RuntimeMetrics,
}

#[derive(Default)]
pub struct WakeWordManager {
    inner: Mutex<ManagerInner>,
}

impl WakeWordManager {
    pub fn status(&self) -> Result<WakeWordRuntimeStatus, String> {
        let inner = lock(&self.inner)?;
        Ok(status_from(&inner))
    }

    pub fn start(&self, phrase: &str, sensitivity: f32) -> Result<WakeWordSessionStarted, String> {
        validate_phrase(phrase)?;
        sensitivity_threshold(sensitivity)?;
        let paths = bundled_model().ok_or_else(|| {
            "The verified local wake-word model is missing; run `pnpm stage:wake-word` and rebuild"
                .to_string()
        })?;
        let mut inner = lock(&self.inner)?;
        if inner.session.is_some() {
            return Err("A wake-word session is already accepting audio".to_string());
        }
        if inner.engine.is_none() {
            match WakeWordEngine::load(&paths) {
                Ok(engine) => inner.engine = Some(Arc::new(engine)),
                Err(error) => {
                    inner.metrics.last_error = Some(error.clone());
                    return Err(error);
                }
            }
        }
        let engine = Arc::clone(inner.engine.as_ref().expect("engine loaded above"));
        let keywords = engine.keyword_tokens(phrase, sensitivity)?;
        let id = format!("wake-{}", uuid::Uuid::new_v4().simple());
        inner.session = Some(WakeWordSession {
            id: id.clone(),
            stream: engine.spotter.create_stream_with_keywords(&keywords),
            accepted_samples: 0,
        });
        inner.metrics.last_error = None;
        Ok(WakeWordSessionStarted {
            session_id: id,
            status: status_from(&inner),
        })
    }

    pub fn push(
        &self,
        session_id: &str,
        sample_rate: i32,
        samples: &[f32],
    ) -> Result<Option<WakeWordDetection>, String> {
        if sample_rate != SAMPLE_RATE {
            return Err(format!("Wake-word PCM must be {SAMPLE_RATE} Hz mono"));
        }
        if samples.is_empty()
            || samples.len() > MAX_FRAME_SAMPLES
            || samples.iter().any(|sample| !sample.is_finite())
        {
            return Err("Wake-word PCM frame is empty, oversized, or non-finite".to_string());
        }
        let mut inner = lock(&self.inner)?;
        let engine = Arc::clone(
            inner
                .engine
                .as_ref()
                .ok_or_else(|| "Wake-word runtime is not loaded".to_string())?,
        );
        let inference_started = Instant::now();
        let (detection_end, detection_micros) = {
            let session = inner
                .session
                .as_mut()
                .filter(|session| session.id == session_id)
                .ok_or_else(|| "Wake-word session is stale or stopped".to_string())?;
            session.stream.accept_waveform(sample_rate, samples);
            session.accepted_samples = session
                .accepted_samples
                .saturating_add(samples.len() as u64);
            let mut detection_end = None;
            while engine.spotter.is_ready(&session.stream) {
                engine.spotter.decode(&session.stream);
                if let Some(result) = engine.spotter.get_result(&session.stream) {
                    if !result.keyword.is_empty() {
                        let timestamp_end = result
                            .timestamps
                            .iter()
                            .copied()
                            .filter(|value| value.is_finite() && *value >= 0.0)
                            .fold(0.0_f32, f32::max);
                        let estimated_end = ((timestamp_end + 0.08) * SAMPLE_RATE as f32) as u64;
                        detection_end = Some(estimated_end.min(session.accepted_samples));
                        engine.spotter.reset(&session.stream);
                        break;
                    }
                }
            }
            let detection_lag_micros = detection_end.map(|keyword_end| {
                u128::from(session.accepted_samples.saturating_sub(keyword_end)) * 1_000_000
                    / SAMPLE_RATE as u128
            });
            (detection_end, detection_lag_micros)
        };
        let inference_micros = inference_started.elapsed().as_micros();
        inner.metrics.inference_samples = inner.metrics.inference_samples.saturating_add(1);
        inner.metrics.total_inference_micros = inner
            .metrics
            .total_inference_micros
            .saturating_add(inference_micros);
        let Some(keyword_end_sample) = detection_end else {
            return Ok(None);
        };
        let detection_micros = detection_micros
            .expect("present with detection end")
            .saturating_add(inference_micros);
        inner.metrics.detections = inner.metrics.detections.saturating_add(1);
        inner.metrics.detection_samples = inner.metrics.detection_samples.saturating_add(1);
        inner.metrics.total_detection_micros = inner
            .metrics
            .total_detection_micros
            .saturating_add(detection_micros);
        Ok(Some(WakeWordDetection {
            detected: true,
            session_id: session_id.to_string(),
            keyword_end_sample,
            inference_ms: inference_micros as f64 / 1_000.0,
        }))
    }

    pub fn stop(&self, session_id: &str, dropped_frames: u64) -> Result<bool, String> {
        let mut inner = lock(&self.inner)?;
        let matches = inner
            .session
            .as_ref()
            .is_some_and(|session| session.id == session_id);
        if matches {
            inner.session = None;
            inner.metrics.dropped_frames =
                inner.metrics.dropped_frames.saturating_add(dropped_frames);
        }
        Ok(matches)
    }
}

fn lock(manager: &Mutex<ManagerInner>) -> Result<MutexGuard<'_, ManagerInner>, String> {
    manager
        .lock()
        .map_err(|_| "Wake-word runtime lock is poisoned".to_string())
}

fn status_from(inner: &ManagerInner) -> WakeWordRuntimeStatus {
    let average = |total: u128, samples: u64| {
        (samples > 0).then_some(total as f64 / samples as f64 / 1_000.0)
    };
    WakeWordRuntimeStatus {
        backend: BACKEND_ID.to_string(),
        local: true,
        runtime_version: RUNTIME_VERSION.to_string(),
        model_id: MODEL_ID.to_string(),
        model_license: MODEL_LICENSE.to_string(),
        available: bundled_model().is_some(),
        loaded: inner.engine.is_some(),
        accepting_audio: inner.session.is_some(),
        sample_rate: SAMPLE_RATE,
        model_bytes: MODEL_BYTES,
        // ONNX Runtime has no portable resident-set query. Reporting the model
        // bytes and leaving process memory unknown is more accurate than
        // presenting an estimate as a measurement.
        model_memory_bytes: None,
        idle_cpu_percent: None,
        average_inference_ms: average(
            inner.metrics.total_inference_micros,
            inner.metrics.inference_samples,
        ),
        average_detection_latency_ms: average(
            inner.metrics.total_detection_micros,
            inner.metrics.detection_samples,
        ),
        detections: inner.metrics.detections,
        dropped_frames: inner.metrics.dropped_frames,
        last_error: inner.metrics.last_error.clone(),
    }
}

pub fn validate_configuration(phrase: &str, sensitivity: u8) -> Result<(), String> {
    validate_phrase(phrase)?;
    sensitivity_threshold(sensitivity as f32 / 100.0).map(|_| ())
}

fn validate_phrase(phrase: &str) -> Result<(), String> {
    let phrase = phrase.trim();
    if phrase.is_empty()
        || phrase.len() > MAX_PHRASE_BYTES
        || phrase.chars().any(char::is_control)
        || !phrase
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, ' ' | '\'' | '-'))
    {
        return Err(
            "Wake phrase must be 1-128 bytes of English letters, numbers, spaces, apostrophes, or hyphens"
                .to_string(),
        );
    }
    Ok(())
}

fn sensitivity_threshold(sensitivity: f32) -> Result<f32, String> {
    if !sensitivity.is_finite() || !(0.0..=1.0).contains(&sensitivity) {
        return Err("Wake-word sensitivity must be between 0 and 1".to_string());
    }
    // sherpa's lower threshold is more sensitive. Keep away from zero, where
    // background speech becomes an almost guaranteed false activation.
    Ok(0.60 - sensitivity * 0.50)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sherpa_onnx::Wave;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn real_inference_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn sensitivity_maps_to_a_bounded_sherpa_threshold() {
        assert_eq!(sensitivity_threshold(0.0).unwrap(), 0.60);
        assert!((sensitivity_threshold(0.5).unwrap() - 0.35).abs() < f32::EPSILON);
        assert!((sensitivity_threshold(1.0).unwrap() - 0.10).abs() < f32::EPSILON);
        assert!(sensitivity_threshold(-0.01).is_err());
        assert!(sensitivity_threshold(f32::NAN).is_err());
    }

    #[test]
    fn phrase_validation_rejects_empty_control_and_non_english_input() {
        assert!(validate_phrase("hey little monkey").is_ok());
        assert!(validate_phrase("monkey-2's wake").is_ok());
        assert!(validate_phrase("").is_err());
        assert!(validate_phrase("hey\nmonkey").is_err());
        assert!(validate_phrase("hej lilla apan").is_ok());
        assert!(validate_phrase("こんにちは").is_err());
    }

    #[test]
    fn missing_or_partial_bundles_are_never_available() {
        assert!(resolve_bundled(None).is_none());
        let root = std::env::temp_dir().join(format!(
            "little-monkey-kws-partial-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let model = root.join(BUNDLED_DIRECTORY);
        fs::create_dir_all(&model).unwrap();
        fs::write(model.join(MODEL_FILES[0].name), b"partial").unwrap();
        assert!(resolve_bundled(Some(&root)).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_never_claims_unmeasured_memory_or_cpu() {
        let manager = WakeWordManager::default();
        let status = manager.status().unwrap();
        assert!(status.local);
        assert_eq!(status.backend, BACKEND_ID);
        assert_eq!(status.runtime_version, RUNTIME_VERSION);
        assert_eq!(status.model_memory_bytes, None);
        assert_eq!(status.idle_cpu_percent, None);
        assert!(!status.loaded);
        assert!(!status.accepting_audio);
    }

    fn samples_trigger(
        engine: &WakeWordEngine,
        samples: &[f32],
        sample_rate: i32,
        phrase: &str,
    ) -> bool {
        let keywords = engine.keyword_tokens(phrase, DEFAULT_SENSITIVITY).unwrap();
        let stream = engine.spotter.create_stream_with_keywords(&keywords);
        for frame in samples.chunks(1_600) {
            stream.accept_waveform(sample_rate, frame);
            while engine.spotter.is_ready(&stream) {
                engine.spotter.decode(&stream);
                if engine
                    .spotter
                    .get_result(&stream)
                    .is_some_and(|result| !result.keyword.is_empty())
                {
                    return true;
                }
            }
        }
        let tail = vec![0.0; (SAMPLE_RATE / 2) as usize];
        stream.accept_waveform(SAMPLE_RATE, &tail);
        stream.input_finished();
        while engine.spotter.is_ready(&stream) {
            engine.spotter.decode(&stream);
            if engine
                .spotter
                .get_result(&stream)
                .is_some_and(|result| !result.keyword.is_empty())
            {
                return true;
            }
        }
        false
    }

    fn fixture_triggers(engine: &WakeWordEngine, wave: &Wave, phrase: &str) -> bool {
        samples_trigger(engine, wave.samples(), wave.sample_rate(), phrase)
    }

    /// Dedicated CI sets LITTLE_MONKEY_KWS_E2E after staging the authenticated
    /// upstream model. This is a real native inference test, not a mocked wake
    /// event; ordinary unit tests remain usable before the large fixture is
    /// staged.
    #[test]
    fn real_model_detects_positive_audio_and_rejects_negative_audio() {
        if std::env::var_os("LITTLE_MONKEY_KWS_E2E").is_none() {
            return;
        }
        let _guard = real_inference_guard();
        let resource_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources");
        let paths = resolve_bundled(Some(&resource_root))
            .expect("run `pnpm stage:wake-word` before the real KWS test");
        let engine = WakeWordEngine::load(&paths).expect("load authenticated KWS model");
        let positive_path = paths.root.join("test_wavs/0.wav");
        let negative_path = paths.root.join("test_wavs/1.wav");
        let positive = Wave::read(positive_path.to_str().unwrap()).expect("read positive fixture");
        let negative = Wave::read(negative_path.to_str().unwrap()).expect("read negative fixture");
        assert!(fixture_triggers(&engine, &positive, "light up"));
        assert!(!fixture_triggers(&engine, &negative, "light up"));

        // This fixture says “lovely child”. A text substring implementation
        // would be vulnerable to treating a nearby form as good enough; the
        // constrained keyword decoder must reject the configured plural.
        assert!(!fixture_triggers(&engine, &negative, "lovely children"));

        // Deterministic background noise exercises the same native detector,
        // rather than a volume-based mock. The positive remains detectable and
        // the negative remains rejected.
        let mut seed = 0x5eed_u32;
        let add_noise = |samples: &[f32], seed: &mut u32| {
            samples
                .iter()
                .map(|sample| {
                    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = ((*seed >> 8) as f32 / 16_777_215.0 - 0.5) * 0.01;
                    (sample + noise).clamp(-1.0, 1.0)
                })
                .collect::<Vec<_>>()
        };
        let noisy_positive = add_noise(positive.samples(), &mut seed);
        let noisy_negative = add_noise(negative.samples(), &mut seed);
        assert!(samples_trigger(
            &engine,
            &noisy_positive,
            positive.sample_rate(),
            "light up"
        ));
        assert!(!samples_trigger(
            &engine,
            &noisy_negative,
            negative.sample_rate(),
            "light up"
        ));

        // The positive recording continues immediately into a command. Insert
        // a deterministic half-second pause after the keyword region to cover
        // the alternate “wake, pause, command” cadence.
        let split = (positive.sample_rate() as usize * 4).min(positive.samples().len());
        let mut paused = positive.samples()[..split].to_vec();
        paused.extend(vec![0.0; (positive.sample_rate() / 2) as usize]);
        paused.extend_from_slice(&positive.samples()[split..]);
        assert!(samples_trigger(
            &engine,
            &paused,
            positive.sample_rate(),
            "light up"
        ));
    }

    /// The executable privacy boundary: native KWS receives the whole fixture,
    /// but Whisper receives only samples after sherpa's keyword-end timestamp.
    #[tokio::test]
    async fn real_wake_event_feeds_only_command_pcm_to_local_whisper() {
        if std::env::var_os("LITTLE_MONKEY_WAKE_TO_WHISPER_E2E").is_none() {
            return;
        }
        let guard = real_inference_guard();
        let resource_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources");
        set_resource_dir(Some(&resource_root));
        crate::local_whisper::set_resource_dir(Some(&resource_root));
        let fixture = resource_root
            .join(BUNDLED_DIRECTORY)
            .join("test_wavs/0.wav");
        let wave = Wave::read(fixture.to_str().unwrap()).expect("read wake + command fixture");
        let manager = WakeWordManager::default();
        let started = manager.start("light up", DEFAULT_SENSITIVITY).unwrap();
        assert!(manager
            .start("light up", DEFAULT_SENSITIVITY)
            .unwrap_err()
            .contains("already accepting"));
        let mut detection = None;
        for frame in wave.samples().chunks(1_600) {
            detection = manager
                .push(&started.session_id, wave.sample_rate(), frame)
                .expect("stream real PCM");
            if detection.is_some() {
                break;
            }
        }
        let detected = detection.expect("fixture must emit a real wake event");
        let live_status = manager.status().unwrap();
        assert!(live_status.accepting_audio);
        assert_eq!(live_status.detections, 1);
        assert!(live_status.average_inference_ms.is_some_and(f64::is_finite));
        assert!(live_status
            .average_detection_latency_ms
            .is_some_and(f64::is_finite));
        assert!(manager.stop(&started.session_id, 0).unwrap());
        assert!(!manager.stop(&started.session_id, 42).unwrap());
        assert_eq!(manager.status().unwrap().dropped_frames, 0);
        assert!(manager
            .push(&started.session_id, SAMPLE_RATE, &[0.0; 160])
            .unwrap_err()
            .contains("stale or stopped"));
        drop(manager);
        drop(guard);

        let command_start = usize::try_from(detected.keyword_end_sample).unwrap();
        assert!(command_start > SAMPLE_RATE as usize);
        assert!(command_start < wave.samples().len());
        let command = &wave.samples()[command_start..];
        let root = std::env::temp_dir().join(format!(
            "little-monkey-wake-whisper-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        let command_path = root.join("command.wav");
        assert!(sherpa_onnx::write(
            command_path.to_str().unwrap(),
            command,
            SAMPLE_RATE
        ));
        let transcript = crate::local_whisper::transcribe(
            &root,
            &command_path,
            "en",
            crate::local_whisper::DEFAULT_MODEL_ID,
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("transcribe post-wake command with bundled Whisper");
        let normalized = transcript.text.to_ascii_lowercase();
        assert!(
            !normalized.contains("light up"),
            "wake leaked: {normalized}"
        );
        assert!(
            normalized.contains("here")
                || normalized.contains("quarter")
                || normalized.contains("brothel"),
            "unexpected command transcript: {normalized}"
        );
        let _ = fs::remove_dir_all(root);
    }
}
