//! Built-in local speech transcription for Talk and the desktop companion.
//!
//! There is deliberately no external `whisper.cpp` executable in this path.
//! `whisper-rs` builds whisper.cpp into Little Monkey on every desktop target,
//! the app downloads one pinned multilingual GGML model into app-owned data,
//! verifies its SHA-256 before activation, and decodes browser/desktop audio to
//! the 16 kHz mono f32 PCM whisper.cpp requires. The public M7 config keeps its
//! historical `whisperBinary`/`whisperModel` fields for wire compatibility, but
//! local transcription no longer reads them.

use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use futures_util::StreamExt;
use opus_decoder::OpusDecoder;
use sha2::{Digest, Sha256};
use symphonia::core::codecs::audio::{well_known::CODEC_ID_OPUS, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const MODEL_DIRECTORY: &str = "local-whisper-v1";

/// The upstream commit every model is fetched from.
///
/// A revision rather than a branch: the file behind a branch can change, and a
/// weights file that changes under a pinned checksum is a download that starts
/// failing for everyone at once.
const MODEL_REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// One speech model the operator can choose.
///
/// `bytes` is the exact published size and is enforced as the download's own
/// ceiling — there is no separate limit to keep in step with the largest entry,
/// and a body that runs past the size it claims is refused before it is hashed.
#[derive(Clone, Copy, Debug)]
pub struct WhisperModel {
    pub id: &'static str,
    /// What it is good for, in the terms the choice is actually made in.
    pub label: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

/// Every installable model, smallest first.
///
/// Quantized builds throughout: a q5 file is roughly a third of the float one
/// with no accuracy anybody has been able to measure in dictation, and the
/// difference between tiers dwarfs it. Sizes and checksums are upstream's own,
/// read from the release at `MODEL_REVISION`.
pub const MODELS: &[WhisperModel] = &[
    WhisperModel {
        id: "base",
        label: "Base — fastest, least accurate",
        file: "ggml-base-q5_1.bin",
        sha256: "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898",
        bytes: 59_707_625,
    },
    WhisperModel {
        id: "small",
        label: "Small — better with names and accents",
        file: "ggml-small-q5_1.bin",
        sha256: "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
        bytes: 190_085_487,
    },
    WhisperModel {
        id: "medium",
        label: "Medium — stronger on non-English speech",
        file: "ggml-medium-q5_0.bin",
        sha256: "19fea4b380c3a618ec4723c3eef2eb785ffba0d0538cf43f8f235e7b3b34220f",
        bytes: 539_212_467,
    },
    WhisperModel {
        id: "large-v3-turbo",
        label: "Large v3 Turbo — near-large accuracy, far faster",
        file: "ggml-large-v3-turbo-q5_0.bin",
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        bytes: 574_041_195,
    },
    WhisperModel {
        id: "large-v3",
        label: "Large v3 — most accurate, slowest",
        file: "ggml-large-v3-q5_0.bin",
        sha256: "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
        bytes: 1_081_140_203,
    },
];

/// What an installation that has never chosen uses, and what the installer
/// ships inside the app: small enough to bundle and to start instantly.
pub const DEFAULT_MODEL_ID: &str = "base";

/// The chosen model, or the default when the id is unknown — a configuration
/// naming a model this build does not have must not make speech unusable.
#[must_use]
pub fn model_for(id: &str) -> &'static WhisperModel {
    let wanted = id.trim();
    MODELS
        .iter()
        .find(|model| model.id == wanted)
        .unwrap_or_else(|| {
            MODELS
                .iter()
                .find(|model| model.id == DEFAULT_MODEL_ID)
                .expect("the default model is in the table")
        })
}

fn model_url(model: &WhisperModel) -> String {
    format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/{MODEL_REVISION}/{}",
        model.file
    )
}
const WHISPER_SAMPLE_RATE: u32 = 16_000;
const MAX_AUDIO_SECONDS: usize = 60 * 60;
const MAX_WHISPER_SAMPLES: usize = WHISPER_SAMPLE_RATE as usize * MAX_AUDIO_SECONDS;
const MAX_OPUS_FRAME_SAMPLES: usize = WHISPER_SAMPLE_RATE as usize * 120 / 1_000;

const BUNDLED_DIRECTORY: &str = "local-whisper";

static BUNDLED_MODEL: OnceLock<Option<PathBuf>> = OnceLock::new();
static PREPARE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static READY_MODEL: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static CONTEXT: OnceLock<Mutex<Option<(PathBuf, Arc<WhisperContext>)>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalTranscriptSegment {
    pub text: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalTranscript {
    pub text: String,
    pub segments: Vec<LocalTranscriptSegment>,
}

/// Record where the installed application keeps its bundled copy of the model.
///
/// Called once at startup. The model ships inside the application, so a first
/// run with no network — or behind a proxy that will not reach huggingface.co —
/// transcribes immediately rather than waiting on a 57MB download that may
/// never succeed. Development trees that have not run `pnpm stage:whisper` have
/// no bundled copy, and fall back to the download path below.
pub fn set_resource_dir(resource_dir: Option<&Path>) {
    let _ = BUNDLED_MODEL.set(resolve_bundled(resource_dir));
}

/// The bundled model, if this installation actually carries an intact one.
///
/// Size, not SHA-256: this file was installed as part of the signed application
/// bundle and is already covered by the startup integrity check, so re-hashing
/// 57MB on every launch would buy nothing. The *downloaded* copy is the
/// untrusted one, and it is still verified byte for byte before activation.
fn resolve_bundled(resource_dir: Option<&Path>) -> Option<PathBuf> {
    // Only the default tier is shipped inside the bundle; every other one is
    // fetched on request, which is why this is consulted for that id alone.
    let bundled = model_for(DEFAULT_MODEL_ID);
    let candidate = resource_dir?.join(BUNDLED_DIRECTORY).join(bundled.file);
    match std::fs::metadata(&candidate) {
        Ok(metadata) if metadata.is_file() && metadata.len() == bundled.bytes => Some(candidate),
        _ => None,
    }
}

fn bundled_model() -> Option<PathBuf> {
    BUNDLED_MODEL.get().cloned().flatten()
}

/// Whether a local transcription could start right now without waiting on a
/// download. Talk and the telephony surface ask before claiming to be ready.
#[must_use]
pub fn is_ready(model_id: &str) -> bool {
    let model = model_for(model_id);
    if model.id == DEFAULT_MODEL_ID && bundled_model().is_some() {
        return true;
    }
    matches!(
        ready_model_slot().lock(),
        Ok(slot) if slot.as_ref().is_some_and(|path| path.is_file() && path.ends_with(model.file))
    )
}

/// Whether this model is already on disk at the size it declares.
///
/// Size only. The listing behind it is a screen the operator opens, and hashing
/// a gigabyte to draw a checkbox is not worth it; the byte-for-byte check
/// belongs to `prepare`, which runs before anything is loaded.
#[must_use]
pub fn model_installed(app_data_dir: &Path, model: &WhisperModel) -> bool {
    if model.id == DEFAULT_MODEL_ID && bundled_model().is_some() {
        return true;
    }
    matches!(
        std::fs::metadata(model_path(app_data_dir, model)),
        Ok(metadata) if metadata.is_file() && metadata.len() == model.bytes
    )
}

fn prepare_lock() -> &'static tokio::sync::Mutex<()> {
    PREPARE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn ready_model_slot() -> &'static Mutex<Option<PathBuf>> {
    READY_MODEL.get_or_init(|| Mutex::new(None))
}

fn context_slot() -> &'static Mutex<Option<(PathBuf, Arc<WhisperContext>)>> {
    CONTEXT.get_or_init(|| Mutex::new(None))
}

fn model_path(app_data_dir: &Path, model: &WhisperModel) -> PathBuf {
    app_data_dir.join(MODEL_DIRECTORY).join(model.file)
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<std::sync::MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("{label} lock is poisoned"))
}

async fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("Open local speech model {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| format!("Read local speech model {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn verified_existing_model(path: &Path, model: &WhisperModel) -> Result<bool, String> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Inspect local speech model {}: {error}",
                path.display()
            ))
        }
    };
    if !metadata.is_file() || metadata.len() != model.bytes {
        return Ok(false);
    }
    Ok(sha256_file(path).await? == model.sha256)
}

async fn download_model(path: &Path, model: &WhisperModel) -> Result<(), String> {
    let url = reqwest::Url::parse(&model_url(model))
        .map_err(|error| format!("Built-in local speech model URL is invalid: {error}"))?;
    crate::egress::classify_public_download_url(&url, crate::egress::PublicDestinations::Only)
        .map_err(|error| format!("Local speech model download refused: {error}"))?;
    let client = crate::egress::public_download_client(
        crate::egress::PublicDestinations::Only,
        "local-whisper.model-download",
    )
    .build()
    .map_err(|error| format!("Build local speech model client: {error}"))?;
    let response = crate::egress::send(client.get(url))
        .await
        .map_err(|error| format!("Download local speech model: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Local speech model download returned {}",
            response.status()
        ));
    }

    let parent = path
        .parent()
        .ok_or_else(|| "Local speech model path has no parent directory".to_string())?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| format!("Create local speech model directory: {error}"))?;
    let temp = parent.join(format!(
        ".{}.download-{}",
        model.file,
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .await
        .map_err(|error| format!("Create local speech model staging file: {error}"))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut written = 0u64;

    let result = async {
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| format!("Read local speech model download: {error}"))?;
            written = written.saturating_add(chunk.len() as u64);
            if written > model.bytes {
                return Err("Local speech model download exceeds its size limit".to_string());
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|error| format!("Write local speech model: {error}"))?;
        }
        if written == 0 {
            return Err("Local speech model download was empty".to_string());
        }
        file.flush()
            .await
            .map_err(|error| format!("Flush local speech model: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("Sync local speech model: {error}"))?;
        let digest = format!("{:x}", hasher.finalize());
        if written != model.bytes {
            return Err(format!(
                "Local speech model download is {written} bytes, not the {} it declares",
                model.bytes
            ));
        }
        if digest != model.sha256 {
            return Err(format!(
                "Local speech model checksum mismatch; expected {}, got {digest}",
                model.sha256
            ));
        }
        drop(file);
        if tokio::fs::metadata(path).await.is_ok() {
            tokio::fs::remove_file(path)
                .await
                .map_err(|error| format!("Replace stale local speech model: {error}"))?;
        }
        tokio::fs::rename(&temp, path)
            .await
            .map_err(|error| format!("Activate local speech model: {error}"))?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

/// Ensure the built-in speech model is ready. Safe to call at startup and on
/// every transcription: concurrent callers collapse behind one install lock.
pub async fn prepare(app_data_dir: &Path, model_id: &str) -> Result<PathBuf, String> {
    let model = model_for(model_id);
    // The installed application ships the default model, so choosing that one
    // fetches nothing at all. Any other tier is a download, once.
    if model.id == DEFAULT_MODEL_ID {
        if let Some(bundled) = bundled_model() {
            return Ok(bundled);
        }
    }
    let path = model_path(app_data_dir, model);
    if let Some(cached) = lock(ready_model_slot(), "local speech model readiness")?.clone() {
        if cached == path && cached.is_file() {
            return Ok(cached);
        }
    }

    let _guard = prepare_lock().lock().await;
    if let Some(cached) = lock(ready_model_slot(), "local speech model readiness")?.clone() {
        if cached == path && cached.is_file() {
            return Ok(cached);
        }
    }

    if !verified_existing_model(&path, model).await? {
        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|error| format!("Remove invalid local speech model: {error}"))?;
        }
        download_model(&path, model).await?;
    }
    *lock(ready_model_slot(), "local speech model readiness")? = Some(path.clone());
    Ok(path)
}

fn whisper_context(model: &Path) -> Result<Arc<WhisperContext>, String> {
    let mut slot = lock(context_slot(), "local Whisper context")?;
    // Keyed by path, not merely "is something loaded". Without this, choosing a
    // different tier changes a setting and nothing else: the first model loaded
    // would stay loaded for the life of the process.
    if let Some((loaded, context)) = slot.as_ref() {
        if loaded == model {
            return Ok(Arc::clone(context));
        }
    }
    let mut params = WhisperContextParameters::default();
    // CPU is the portable baseline and guarantees that the same installed app
    // works on macOS, Windows and Linux without a CUDA/Metal/Vulkan runtime.
    params.use_gpu(false);
    let context = Arc::new(
        WhisperContext::new_with_params(model, params)
            .map_err(|error| format!("Load built-in local Whisper model: {error}"))?,
    );
    *slot = Some((model.to_path_buf(), Arc::clone(&context)));
    Ok(context)
}

fn downmix_interleaved(
    samples: &[f32],
    channels: usize,
    output: &mut Vec<f32>,
) -> Result<(), String> {
    if channels == 0 {
        return Err("Decoded audio reported zero channels".to_string());
    }
    if samples.len() % channels != 0 {
        return Err("Decoded audio contains an incomplete frame".to_string());
    }
    for frame in samples.chunks_exact(channels) {
        output.push(frame.iter().copied().sum::<f32>() / channels as f32);
    }
    Ok(())
}

fn resample_linear(input: &[f32], source_rate: u32) -> Result<Vec<f32>, String> {
    if input.is_empty() {
        return Err("Recorded audio decoded to no samples".to_string());
    }
    if source_rate == 0 {
        return Err("Recorded audio has no sample rate".to_string());
    }
    if source_rate == WHISPER_SAMPLE_RATE {
        if input.len() > MAX_WHISPER_SAMPLES {
            return Err(
                "Recorded audio exceeds the one-hour local transcription limit".to_string(),
            );
        }
        return Ok(input.to_vec());
    }
    let output_len = ((input.len() as u128 * WHISPER_SAMPLE_RATE as u128)
        .div_ceil(source_rate as u128)) as usize;
    if output_len == 0 || output_len > MAX_WHISPER_SAMPLES {
        return Err("Recorded audio exceeds the one-hour local transcription limit".to_string());
    }
    let ratio = source_rate as f64 / WHISPER_SAMPLE_RATE as f64;
    let mut output = Vec::with_capacity(output_len);
    for index in 0..output_len {
        let source = index as f64 * ratio;
        let left = source.floor() as usize;
        let right = (left + 1).min(input.len() - 1);
        let mix = (source - left as f64) as f32;
        output.push(input[left] * (1.0 - mix) + input[right] * mix);
    }
    Ok(output)
}

fn open_media(path: &Path) -> Result<Box<dyn FormatReader + '_>, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("Open recorded audio {}: {error}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }
    symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("Decode recorded audio container: {error}"))
}

/// Every language the bundled model can be asked for, as `(code, english_name)`.
///
/// Read from whisper.cpp's own table rather than restated here: a list written
/// out by hand is a list that drifts from the model that has to honour it.
pub fn supported_languages() -> Vec<(String, String)> {
    (0..=whisper_rs::get_lang_max_id())
        .filter_map(|id| {
            let code = whisper_rs::get_lang_str(id)?;
            let name = whisper_rs::get_lang_str_full(id).unwrap_or(code);
            Some((code.to_string(), name.to_string()))
        })
        .collect()
}

/// Whether a demux error is simply the end of the recording.
///
/// A browser's `MediaRecorder` writes a live stream: clusters as they happen,
/// a segment of unknown size, and no terminator once the microphone closes.
/// Symphonia reaches the last short element and reports `UnexpectedEof`, which
/// is not a corrupt file — it is the end of one. Treating it as a failure threw
/// away every utterance *after* decoding it, so nothing was ever transcribed.
fn is_end_of_stream(error: &SymphoniaError) -> bool {
    matches!(error, SymphoniaError::IoError(source) if source.kind() == std::io::ErrorKind::UnexpectedEof)
}

fn decode_opus(
    format: &mut dyn FormatReader,
    track_id: u32,
    channels: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<f32>, String> {
    if !(1..=2).contains(&channels) {
        return Err(format!(
            "Browser Opus audio has {channels} channels; local transcription supports mono or stereo"
        ));
    }
    let mut decoder = OpusDecoder::new(WHISPER_SAMPLE_RATE, channels)
        .map_err(|error| format!("Initialize built-in Opus decoder: {error}"))?;
    let mut frame = vec![0.0f32; MAX_OPUS_FRAME_SAMPLES * channels];
    let mut mono = Vec::new();
    loop {
        if cancellation.is_cancelled() {
            return Err("Transcription cancelled".to_string());
        }
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                return Err("Recorded audio changed tracks mid-stream".to_string())
            }
            Err(ref error) if is_end_of_stream(error) => break,
            Err(error) => return Err(format!("Demux Opus audio: {error}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let samples_per_channel = decoder
            .decode_float(packet.data.as_ref(), &mut frame, false)
            .map_err(|error| format!("Decode browser Opus audio: {error}"))?;
        let used = samples_per_channel
            .checked_mul(channels)
            .ok_or_else(|| "Decoded Opus sample count overflow".to_string())?;
        downmix_interleaved(&frame[..used], channels, &mut mono)?;
        if mono.len() > MAX_WHISPER_SAMPLES {
            return Err(
                "Recorded audio exceeds the one-hour local transcription limit".to_string(),
            );
        }
    }
    if mono.is_empty() {
        return Err("Recorded audio decoded to no samples".to_string());
    }
    Ok(mono)
}

fn decode_audio(path: &Path, cancellation: &CancellationToken) -> Result<Vec<f32>, String> {
    let mut format = open_media(path)?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "Recorded file contains no audio track".to_string())?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .ok_or_else(|| "Recorded audio track has no codec parameters".to_string())?
        .audio()
        .ok_or_else(|| "Recorded audio track has non-audio codec parameters".to_string())?
        .clone();
    let channels = params.channels.as_ref().map_or(1, |value| value.count());

    if params.codec == CODEC_ID_OPUS {
        return decode_opus(&mut *format, track_id, channels, cancellation);
    }

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|error| format!("Unsupported recorded audio codec: {error}"))?;
    let mut mono = Vec::new();
    let mut source_rate = None;
    loop {
        if cancellation.is_cancelled() {
            return Err("Transcription cancelled".to_string());
        }
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                return Err("Recorded audio changed tracks mid-stream".to_string())
            }
            Err(ref error) if is_end_of_stream(error) => break,
            Err(error) => return Err(format!("Read recorded audio: {error}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(audio) => audio,
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                return Err("Recorded audio changed format mid-stream".to_string())
            }
            Err(error) => return Err(format!("Decode recorded audio: {error}")),
        };
        let rate = audio.spec().rate();
        match source_rate {
            Some(previous) if previous != rate => {
                return Err("Recorded audio changes sample rate mid-stream".to_string())
            }
            None => source_rate = Some(rate),
            _ => {}
        }
        let channels = audio.spec().channels().count();
        let mut interleaved = vec![0.0f32; audio.samples_interleaved()];
        audio.copy_to_slice_interleaved(&mut interleaved);
        downmix_interleaved(&interleaved, channels, &mut mono)?;
        let max_source_samples = rate as usize * MAX_AUDIO_SECONDS;
        if mono.len() > max_source_samples {
            return Err(
                "Recorded audio exceeds the one-hour local transcription limit".to_string(),
            );
        }
    }
    resample_linear(
        &mono,
        source_rate
            .or(params.sample_rate)
            .ok_or_else(|| "Recorded audio did not report a sample rate".to_string())?,
    )
}

fn timestamp_ms(value: i64) -> Option<u64> {
    u64::try_from(value).ok()?.checked_mul(10)
}

unsafe extern "C" fn cancellation_abort_callback(user_data: *mut c_void) -> bool {
    if user_data.is_null() {
        return false;
    }
    // SAFETY: run_whisper passes a pointer to a boxed CancellationToken whose
    // allocation stays alive and unmoved until state.full() has returned.
    let cancellation = unsafe { &*user_data.cast::<CancellationToken>() };
    cancellation.is_cancelled()
}

fn run_whisper(
    model: &Path,
    pcm: Vec<f32>,
    language: String,
    initial_prompt: Option<String>,
    cancellation: CancellationToken,
) -> Result<LocalTranscript, String> {
    if cancellation.is_cancelled() {
        return Err("Transcription cancelled".to_string());
    }
    let context = whisper_context(model)?;
    let mut state = context
        .create_state()
        .map_err(|error| format!("Create built-in Whisper state: {error}"))?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_context(true);
    // Words the decoder should expect. whisper.cpp conditions on this exactly as
    // it conditions on the previous window's text, which is what makes a name
    // already on screen come back spelled instead of guessed at phonetically —
    // "Sundbyberg" rather than "soon the B-Berry". It is a hint, not a
    // constraint: nothing here forces a token that was not said.
    if let Some(prompt) = initial_prompt.as_deref().map(str::trim).filter(|value| !value.is_empty())
    {
        params.set_initial_prompt(prompt);
    }
    let language = language.trim().to_string();
    if language.is_empty() || language.eq_ignore_ascii_case("auto") {
        // "auto" is a language whisper.cpp understands: it detects, then
        // transcribes. `set_detect_language` is a different request — it means
        // detect and *stop*, and `whisper_full` returns right after detection
        // with no segments at all. Setting it here made every transcription on
        // the default configuration come back empty, which is a spoken turn
        // that records, decodes, runs the model and then says nothing.
        params.set_language(Some("auto"));
    } else {
        params.set_language(Some(&language));
    }
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .clamp(1, 8) as i32;
    params.set_n_threads(threads);

    // Do not use whisper-rs' set_abort_callback_safe at the pinned revision.
    // It type-erases the stored closure but instantiates its trampoline with the
    // original concrete closure type, so the callback casts user_data to the
    // wrong type. Once whisper.cpp invokes that callback the result is undefined
    // behavior (the CI E2E test manifested it as an inference hang). Keep the
    // cancellation token in our own stable allocation and wire the raw callback
    // for exactly the synchronous lifetime of state.full() instead.
    let cancel_for_callback = Box::new(cancellation.clone());
    let cancel_user_data = (&*cancel_for_callback as *const CancellationToken)
        .cast_mut()
        .cast::<c_void>();
    unsafe {
        params.set_abort_callback(Some(cancellation_abort_callback));
        params.set_abort_callback_user_data(cancel_user_data);
    }

    let full_result = state.full(params, &pcm);
    drop(cancel_for_callback);
    if let Err(error) = full_result {
        if cancellation.is_cancelled() {
            return Err("Transcription cancelled".to_string());
        }
        return Err(format!("Run built-in local Whisper: {error}"));
    }

    let mut segments = Vec::new();
    let mut parts = Vec::new();
    for segment in state.as_iter() {
        let text = segment
            .to_str_lossy()
            .map_err(|error| format!("Read local Whisper segment: {error}"))?
            .trim()
            .to_string();
        if text.is_empty() || is_non_speech_annotation(&text) {
            continue;
        }
        parts.push(text.clone());
        segments.push(LocalTranscriptSegment {
            text,
            start_ms: timestamp_ms(segment.start_timestamp()),
            end_ms: timestamp_ms(segment.end_timestamp()),
        });
    }
    let text = parts.join(" ").trim().to_string();
    if text.is_empty() {
        return Err(NO_SPEECH.to_string());
    }
    Ok(LocalTranscript { text, segments })
}

/// What the model returns when it decoded the window but heard no speech in it.
///
/// A distinct constant rather than a sentence written twice, because Talk has to
/// recognise this case: a fragment of room noise is silence, not a failed turn.
pub const NO_SPEECH: &str = "Built-in local Whisper returned an empty transcript";

/// Whether a segment is Whisper's non-speech annotation rather than something
/// somebody said.
///
/// Fed a fan, a keyboard or a room, the model narrates instead of transcribing:
/// "[ Inaudible ]", "[BLANK_AUDIO]", "(wind blowing)", "*music*". Left in, each
/// burst of noise becomes a spoken turn the assistant then answers. Matching the
/// wrapper instead of a word list is what makes this hold for every language the
/// model decodes — the brackets are Whisper's convention, the words inside them
/// are not — and speech itself never comes back wrapped end to end.
fn is_non_speech_annotation(text: &str) -> bool {
    let mut characters = text.chars();
    let (Some(first), Some(last)) = (characters.next(), characters.next_back()) else {
        return false;
    };
    matches!((first, last), ('[', ']') | ('(', ')') | ('*', '*'))
}

/// Decode any supported recording container and transcribe it through the
/// app-owned model. Model preparation happens automatically on first use.
pub async fn transcribe(
    app_data_dir: &Path,
    path: &Path,
    language: &str,
    model_id: &str,
    initial_prompt: Option<&str>,
    cancellation: CancellationToken,
) -> Result<LocalTranscript, String> {
    let model = prepare(app_data_dir, model_id).await?;
    let path = path.to_path_buf();
    let language = language.to_string();
    let initial_prompt = initial_prompt.map(str::to_string);
    let cancellation_for_worker = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        let pcm = decode_audio(&path, &cancellation_for_worker)?;
        run_whisper(&model, pcm, language, initial_prompt, cancellation_for_worker)
    })
    .await
    .map_err(|error| format!("Local transcription worker failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Room noise came back as "[ Inaudible ] [ Inaudible ] ..." and was
    /// submitted as a spoken turn, which the assistant then answered. Whisper's
    /// annotations are not speech, in any language, and must not survive into a
    /// transcript.
    #[test]
    fn whisper_narrating_the_room_is_not_a_transcript() {
        for annotation in [
            "[ Inaudible ]",
            "[BLANK_AUDIO]",
            "(wind blowing)",
            "*music*",
            "[ Ljud av tangentbord ]",
        ] {
            assert!(is_non_speech_annotation(annotation), "{annotation}");
        }
        for speech in [
            "Tell me a joke.",
            "The array [0] is empty",
            "I said (quietly) that it works",
            "*",
        ] {
            assert!(!is_non_speech_annotation(speech), "{speech}");
        }
    }

    #[test]
    fn the_language_list_comes_from_the_model_not_from_a_list_here() {
        let languages = supported_languages();
        // Whisper's own table, so it is neither empty nor a handful.
        assert!(languages.len() > 90, "got {} languages", languages.len());
        let code = |wanted: &str| languages.iter().any(|(id, _)| id == wanted);
        assert!(code("en") && code("sv") && code("fa"));
        // Every entry carries a name to show, not just a code.
        assert!(languages.iter().all(|(id, label)| !id.is_empty() && !label.is_empty()));
    }

    #[test]
    fn a_live_recordings_short_last_element_is_its_end_not_a_failure() {
        // What Symphonia reports at the end of a `MediaRecorder` stream: the
        // segment has no terminator, so the final element runs out early.
        assert!(is_end_of_stream(&SymphoniaError::IoError(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "end of stream",
        ))));
        // A real read failure still is one.
        assert!(!is_end_of_stream(&SymphoniaError::IoError(
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        )));
        assert!(!is_end_of_stream(&SymphoniaError::DecodeError("corrupt")));
    }

    #[test]
    fn a_bundled_model_is_taken_only_when_it_is_whole() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-bundled-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let dir = root.join(BUNDLED_DIRECTORY);
        fs::create_dir_all(&dir).unwrap();
        let bundled = model_for(DEFAULT_MODEL_ID);
        let model = dir.join(bundled.file);

        assert_eq!(resolve_bundled(None), None, "no resource dir, no model");
        assert_eq!(
            resolve_bundled(Some(&root)),
            None,
            "an absent model is not a bundled model"
        );

        // A truncated resource is the failure this guards: it would otherwise
        // be handed to whisper.cpp as though it were the real thing.
        fs::write(&model, b"not the whole model").unwrap();
        assert_eq!(
            resolve_bundled(Some(&root)),
            None,
            "a short model is not a bundled model"
        );

        fs::write(&model, vec![0u8; bundled.bytes as usize]).unwrap();
        assert_eq!(resolve_bundled(Some(&root)), Some(model));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn downmixes_stereo_frames() {
        let mut mono = Vec::new();
        downmix_interleaved(&[1.0, -1.0, 0.5, 0.5], 2, &mut mono).unwrap();
        assert_eq!(mono, vec![0.0, 0.5]);
    }

    #[test]
    fn resamples_to_whispers_rate() {
        let input: Vec<f32> = (0..48_000).map(|index| index as f32 / 48_000.0).collect();
        let output = resample_linear(&input, 48_000).unwrap();
        assert_eq!(output.len(), 16_000);
        assert!((output[8_000] - 0.5).abs() < 0.001);
    }

    #[test]
    fn automatic_model_path_is_app_owned() {
        let root = Path::new("/tmp/little-monkey-test-data");
        for model in MODELS {
            assert_eq!(
                model_path(root, model),
                root.join(MODEL_DIRECTORY).join(model.file)
            );
        }
    }

    #[test]
    fn every_offered_model_is_pinned_and_distinct() {
        assert!(MODELS.len() >= 5, "the whole ladder is offered");
        for model in MODELS {
            assert!(model.file.starts_with("ggml-") && model.file.ends_with(".bin"));
            // A checksum is what makes the download safe to run; a size is what
            // stops a body that never ends.
            assert_eq!(model.sha256.len(), 64, "{} has no usable checksum", model.id);
            assert!(model.sha256.chars().all(|value| value.is_ascii_hexdigit()));
            assert!(model.bytes > 0);
            assert!(model_url(model).contains(MODEL_REVISION));
            assert!(model_url(model).ends_with(model.file));
        }
        // Ids and files are both identities; a duplicate of either would make
        // two tiers share one download.
        for (index, model) in MODELS.iter().enumerate() {
            assert!(
                !MODELS[index + 1..].iter().any(|other| other.id == model.id
                    || other.file == model.file
                    || other.sha256 == model.sha256),
                "{} is not distinct",
                model.id
            );
        }
        // Smallest first, so the list reads as a ladder in the settings.
        assert!(MODELS.windows(2).all(|pair| pair[0].bytes < pair[1].bytes));
    }

    #[test]
    fn an_unknown_model_falls_back_rather_than_failing() {
        assert_eq!(model_for("large-v3").id, "large-v3");
        // A config from a build that offered something this one does not must
        // not make speech unusable.
        assert_eq!(model_for("gigantic-v9").id, DEFAULT_MODEL_ID);
        assert_eq!(model_for("").id, DEFAULT_MODEL_ID);
        assert_eq!(model_for(" large-v3 ").id, "large-v3");
    }

    #[test]
    fn centiseconds_become_milliseconds_without_negative_wrap() {
        assert_eq!(timestamp_ms(123), Some(1_230));
        assert_eq!(timestamp_ms(-1), None);
    }

    #[test]
    fn cancellation_abort_callback_reads_stable_token() {
        let cancellation = Box::new(CancellationToken::new());
        let user_data = (&*cancellation as *const CancellationToken)
            .cast_mut()
            .cast::<c_void>();
        assert!(!unsafe { cancellation_abort_callback(user_data) });
        cancellation.cancel();
        assert!(unsafe { cancellation_abort_callback(user_data) });
    }

    #[tokio::test]
    async fn e2e_auto_provisions_and_transcribes_real_audio() {
        let wav = match std::env::var_os("LITTLE_MONKEY_LOCAL_WHISPER_E2E_WAV") {
            Some(path) => PathBuf::from(path),
            None => return,
        };
        let webm = PathBuf::from(
            std::env::var_os("LITTLE_MONKEY_LOCAL_WHISPER_E2E_WEBM")
                .expect("WebM fixture must accompany the WAV fixture"),
        );
        let root = std::env::temp_dir().join(format!(
            "little-monkey-whisper-e2e-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();

        let default_model = model_for(DEFAULT_MODEL_ID);
        let prepared = prepare(&root, DEFAULT_MODEL_ID)
            .await
            .expect("model auto-provisions");
        assert!(prepared.is_file());
        assert_eq!(sha256_file(&prepared).await.unwrap(), default_model.sha256);

        // Both containers, and both language settings. "auto" is what the
        // shipped configuration uses and what every spoken turn asks for, and
        // it was the one combination nothing covered — so it was the one that
        // returned an empty transcript for every recording ever made.
        for fixture in [&wav, &webm] {
            for language in ["en", "auto"] {
                let transcript = transcribe(
                    &root,
                    fixture,
                    language,
                    DEFAULT_MODEL_ID,
                    None,
                    CancellationToken::new(),
                )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("{} as {language} failed: {error}", fixture.display())
                    });
                let normalized = transcript.text.to_ascii_lowercase();
                assert!(
                    normalized.contains("country") || normalized.contains("ask not"),
                    "unexpected transcript for {} as {language}: {}",
                    fixture.display(),
                    transcript.text
                );
            }
        }
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
