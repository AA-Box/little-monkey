//! Durable, user-selected multi-clip video timeline export.
//!
//! This combines *published* Studio video jobs into a single MP4. FFmpeg is a
//! host utility invoked with fixed argv and bounded app-owned paths, never a
//! command taken from model output. Journal reconciliation makes crash between
//! gallery publication and job completion idempotent. Source-model latent,
//! motion, and audio context cannot be transferred between native samplers.

use crate::generation_commands::{self, GenerationEntry};
use crate::profiles::ProfileScopedPaths;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_TIMELINES: usize = 48;
const MAX_SEGMENTS: usize = 16;
const MAX_TIMELINE_FILE: u64 = 1024 * 1024;
const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;
const EXPORT_LIMIT: Duration = Duration::from_secs(30 * 60);
const MAX_MUSIC_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoTimelineSegment {
    pub job_id: String,
    #[serde(default)]
    pub trim_start_ms: u64,
    #[serde(default)]
    pub trim_end_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineStatus {
    Queued,
    Rendering,
    Interrupted,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoTimeline {
    pub id: String,
    pub title: String,
    pub segments: Vec<VideoTimelineSegment>,
    pub soundtrack_path: Option<String>,
    pub status: TimelineStatus,
    pub output_entry_id: Option<String>,
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_millis() as u64).unwrap_or(0)
}
fn mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}
fn active() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}
fn cancellations() -> &'static Mutex<HashMap<String, CancellationToken>> {
    static TOKENS: OnceLock<Mutex<HashMap<String, CancellationToken>>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}
struct ScratchDir(PathBuf);
impl Drop for ScratchDir {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}
fn export_gate() -> &'static tokio::sync::Mutex<()> {
    static M: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn journal_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.profile_data_dir().map_err(|e| e.to_string())?.join("studio-v1");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if fs::symlink_metadata(&dir).map_err(|e| e.to_string())?.file_type().is_symlink() {
        return Err("Unsafe Studio timeline directory".into());
    }
    Ok(dir.join("video-timelines.json"))
}

fn load(path: &Path) -> Result<Vec<VideoTimeline>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_TIMELINE_FILE {
        return Err("Unsafe or oversized timeline journal".into());
    }
    let jobs: Vec<VideoTimeline> = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("Corrupted timeline journal: {e}"))?;
    if jobs.len() > MAX_TIMELINES {
        return Err("Timeline journal limit exceeded".into());
    }
    Ok(jobs)
}

fn save(path: &Path, jobs: &[VideoTimeline]) -> Result<(), String> {
    let encoded = serde_json::to_vec(jobs).map_err(|e| e.to_string())?;
    if encoded.len() as u64 > MAX_TIMELINE_FILE { return Err("Timeline journal exceeds size limit".into()); }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.is_file() || meta.file_type().is_symlink() { return Err("Unsafe timeline journal destination".into()); }
    }
    let tmp = path.with_file_name(format!(".video-timelines-{}.tmp", Uuid::new_v4()));
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
    let saved = (|| -> Result<(), String> {
        let mut file = opts.open(&tmp).map_err(|e| e.to_string())?;
        file.write_all(&encoded).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            fs::File::open(path.parent().ok_or("Missing timeline directory")?)
                .and_then(|folder| folder.sync_all()).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    if saved.is_err() { let _ = fs::remove_file(&tmp); }
    saved
}

fn valid_source_id(id: &str) -> bool {
    if let Some(entry) = id.strip_prefix("gallery:studio-") {
        let Ok(uuid) = Uuid::parse_str(entry) else { return false };
        return uuid.to_string() == entry;
    }
    Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
}

fn validate_segments(segments: &[VideoTimelineSegment]) -> Result<(), String> {
    if !(2..=MAX_SEGMENTS).contains(&segments.len()) {
        return Err(format!("Select 2–{MAX_SEGMENTS} video clips to export"));
    }
    let mut seen = BTreeSet::new();
    for seg in segments {
        if !valid_source_id(&seg.job_id) {
            return Err("A timeline segment must name a video job or a Studio gallery entry".into());
        }
        if !seen.insert(&seg.job_id) {
            return Err("A video job cannot appear twice in one timeline".into());
        }
        if seg.trim_start_ms > 600_000 || seg.trim_end_ms.is_some_and(|end| {
            end <= seg.trim_start_ms || end > 600_000
        }) {
            return Err("Invalid trim range; trims must be ordered and within ten minutes".into());
        }
    }
    Ok(())
}

fn validated_soundtrack(path: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(raw) = path.filter(|s| !s.trim().is_empty()) else { return Ok(None); };
    let music = PathBuf::from(raw);
    if !music.is_absolute() { return Err("Soundtrack must be an absolute local file path".into()); }
    let metadata = fs::symlink_metadata(&music).map_err(|e| format!("Soundtrack: {e}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_MUSIC_BYTES {
        return Err("Soundtrack must be a regular audio file of at most 128 MiB".into());
    }
    let ext = music.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
    if !["wav", "mp3", "m4a", "aac", "flac", "ogg"].contains(&ext.as_str()) {
        return Err("Soundtrack must be WAV, MP3, M4A, AAC, FLAC or OGG".into());
    }
    Ok(Some(music))
}

fn clip_entries(
    timeline: &VideoTimeline,
    completed_jobs: &[crate::studio_video_jobs::VideoJobView],
    gallery: &[GenerationEntry],
) -> Result<Option<Vec<GenerationEntry>>, String> {
    let mut found = Vec::new();
    for segment in &timeline.segments {
        if let Some(entry_id) = segment.job_id.strip_prefix("gallery:") {
            let item = gallery.iter().find(|entry| entry.entry_id == entry_id)
                .ok_or_else(|| format!("Gallery source {entry_id} is unavailable"))?;
            if item.media_type != "video/mp4" && item.media_type != "video/webm" {
                return Err("Gallery timeline source is not an MP4 or WebM video".into());
            }
            found.push(item.clone());
            continue;
        }
        let job = completed_jobs.iter().find(|j| j.id == segment.job_id)
            .ok_or_else(|| "A timeline references an unknown video job".to_string())?;
        match job.status {
            crate::studio_video_jobs::VideoJobStatus::Completed => {},
            crate::studio_video_jobs::VideoJobStatus::Queued |
            crate::studio_video_jobs::VideoJobStatus::Running |
            crate::studio_video_jobs::VideoJobStatus::Interrupted => return Ok(None),
            _ => return Err(format!("Source video {} failed or was cancelled", segment.job_id)),
        }
        let item = gallery.iter().find(|entry| {
            entry.video_job_id.as_deref() == Some(segment.job_id.as_str())
                && job.result_entry_ids.contains(&entry.entry_id)
                && entry.task.is_video()
        }).ok_or_else(|| format!("Completed source clip {} is absent from the gallery", segment.job_id))?;
        if item.media_type != "video/mp4" && item.media_type != "video/webm" {
            return Err(format!("Timeline cannot decode {}", item.media_type));
        }
        if let Some(end) = segment.trim_end_ms {
            if item.duration_ms > 0 && end > item.duration_ms {
                return Err("Clip trim end exceeds published duration".into());
            }
        }
        if item.duration_ms > 0 && segment.trim_start_ms >= item.duration_ms {
            return Err("Clip trim start exceeds published duration".into());
        }
        found.push(item.clone());
    }
    Ok(Some(found))
}

/// Imported ComfyUI clips may not record pixel dimensions in gallery metadata.
/// Inspect the actual decoded video stream so the timeline canvas is never a
/// guessed 64x64 thumbnail.
async fn probe_canvas(path: &Path) -> Result<(u32, u32), String> {
    let ffprobe = ffmpeg_executable().with_file_name("ffprobe");
    let output = tokio::time::timeout(Duration::from_secs(20),
        Command::new(ffprobe)
            .args(["-v","error","-select_streams","v:0",
                   "-show_entries","stream=width,height","-of","csv=s=x:p=0"])
            .arg(path)
            .stdin(Stdio::null()).stderr(Stdio::null()).output()
    ).await.map_err(|_| "Video geometry probe timed out".to_string())?
     .map_err(|e| format!("FFprobe is required to read video dimensions: {e}"))?;
    if !output.status.success() { return Err("Cannot read video stream geometry".into()); }
    let line = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
    let mut parts = line.trim().split('x');
    let width: u32 = parts.next().unwrap_or("").parse().map_err(|_| "Invalid video width")?;
    let height: u32 = parts.next().unwrap_or("").parse().map_err(|_| "Invalid video height")?;
    if parts.next().is_some() || width < 32 || height < 32 || width > 8192 || height > 8192 {
        return Err("Source video dimensions are unsupported".into());
    }
    let scale = f64::min(1920.0 / f64::from(width), 1080.0 / f64::from(height)).min(1.0);
    let target_width = ((f64::from(width) * scale).round() as u32 / 2 * 2).max(64);
    let target_height = ((f64::from(height) * scale).round() as u32 / 2 * 2).max(64);
    Ok((target_width, target_height))
}

async fn probe_duration_ms(path: &Path) -> Result<u64, String> {
    let binary = ffmpeg_executable().with_file_name("ffprobe");
    let output = tokio::time::timeout(Duration::from_secs(20),
        Command::new(binary)
            .args(["-v", "error", "-show_entries", "format=duration",
                   "-of", "default=noprint_wrappers=1:nokey=1"])
            .arg(path).stdin(Stdio::null()).stderr(Stdio::null()).output()
    ).await.map_err(|_| "MP4 duration probe timed out".to_string())?
     .map_err(|e| format!("FFprobe is required to inspect MP4 duration: {e}"))?;
    if !output.status.success() { return Err("Cannot inspect assembled MP4 duration".into()); }
    let text = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
    let seconds: f64 = text.trim().parse().map_err(|_| "Invalid MP4 duration")?;
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 60.0 * 60.0 {
        return Err("Assembled MP4 duration is invalid or exceeds one hour".into());
    }
    Ok((seconds * 1000.0).round() as u64)
}

fn ffmpeg_executable() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        for p in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
            if Path::new(p).is_file() { return PathBuf::from(p); }
        }
    }
    PathBuf::from("ffmpeg")
}

fn filter_graph(segments: &[VideoTimelineSegment], width: u32, height: u32) -> String {
    let mut filters = Vec::new();
    for (idx, segment) in segments.iter().enumerate() {
        let trim = match segment.trim_end_ms {
            Some(end) => format!("trim=start={:.3}:end={:.3},", segment.trim_start_ms as f64 / 1000., end as f64 / 1000.),
            None if segment.trim_start_ms > 0 => format!("trim=start={:.3},", segment.trim_start_ms as f64 / 1000.),
            None => String::new(),
        };
        filters.push(format!(
            "[{idx}:v:0]{trim}setpts=PTS-STARTPTS,fps=24,scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p[v{idx}]"
        ));
    }
    let links = (0..segments.len()).map(|idx| format!("[v{idx}]")).collect::<String>();
    filters.push(format!("{links}concat=n={}:v=1:a=0[outv]", segments.len()));
    filters.join(";")
}

#[derive(Clone, Debug)]
struct SourceAudio {
    has_audio: bool,
    duration_ms: u64,
}

async fn source_audio(path: &Path) -> Result<SourceAudio, String> {
    let ffprobe = ffmpeg_executable().with_file_name("ffprobe");
    let output = tokio::time::timeout(Duration::from_secs(20),
        Command::new(ffprobe)
            .args(["-v", "error", "-show_entries", "stream=codec_type",
                   "-show_entries", "format=duration", "-of", "json"])
            .arg(path).stdin(Stdio::null()).stderr(Stdio::null()).output()
    ).await.map_err(|_| "Source audio probe timed out".to_string())?
     .map_err(|e| format!("FFprobe is required for audio inspection: {e}"))?;
    if !output.status.success() {
        return Err("FFprobe could not read the source video".into());
    }
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let duration: f64 = result.get("format").and_then(|v| v.get("duration"))
        .and_then(|v| v.as_str()).ok_or("Video container has no duration")?
        .parse().map_err(|_| "Invalid source-video duration")?;
    if !duration.is_finite() || duration <= 0.0 || duration > 3600.0 {
        return Err("A source clip has an invalid or excessive duration".into());
    }
    Ok(SourceAudio {
        has_audio: result.get("streams").and_then(|v| v.as_array())
            .is_some_and(|streams| streams.iter().any(|v| v.get("codec_type").and_then(|s| s.as_str()) == Some("audio"))),
        duration_ms: (duration * 1000.0).round() as u64,
    })
}

/// Preserve the audio timeline as well as video. When one source has no sound,
/// insert exactly its clipped duration of silence so later audio remains in
/// sync. User-picked music overrides source audio completely.
fn graph_with_audio(
    segments: &[VideoTimelineSegment],
    sources: &[SourceAudio],
    width: u32,
    height: u32,
) -> Result<String, String> {
    if segments.len() != sources.len() { return Err("Audio source count mismatch".into()); }
    let mut video_parts = filter_graph(segments, width, height)
        .split(';').map(str::to_string).collect::<Vec<_>>();
    video_parts.pop(); // replace the video-only concat with audio+video concat
    let mut links = String::new();
    let mut total_duration_ms = 0_u64;
    for (index, (segment, source)) in segments.iter().zip(sources.iter()).enumerate() {
        let end = segment.trim_end_ms.unwrap_or(source.duration_ms);
        if end <= segment.trim_start_ms || end > source.duration_ms.saturating_add(100) {
            return Err("A timeline audio trim is beyond the source clip duration".into());
        }
        let duration_ms = end.saturating_sub(segment.trim_start_ms);
        total_duration_ms = total_duration_ms.saturating_add(duration_ms);
        let seconds = duration_ms as f64 / 1000.0;
        let part = if source.has_audio {
            format!(
                "[{index}:a:0]atrim=start={:.3}:end={:.3},asetpts=PTS-STARTPTS,aresample=48000,\
                 aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo,\
                 apad,atrim=duration={seconds:.3}[a{index}]",
                segment.trim_start_ms as f64 / 1000.0, end as f64 / 1000.0
            )
        } else {
            format!("anullsrc=r=48000:cl=stereo,atrim=duration={seconds:.3},asetpts=PTS-STARTPTS[a{index}]")
        };
        video_parts.push(part);
        links.push_str(&format!("[v{index}][a{index}]"));
    }
    if total_duration_ms > 3_600_000 {
        return Err("A Studio timeline cannot exceed one hour".into());
    }
    video_parts.push(format!("{links}concat=n={}:v=1:a=1[outv][outa]", segments.len()));
    Ok(video_parts.join(";"))
}

/// Stateless FFmpeg runner that is usable directly from the native decoder
/// acceptance test. It never accepts a caller-authored FFmpeg argument.
async fn export_ffmpeg(
    files: &[PathBuf],
    segments: &[VideoTimelineSegment],
    soundtrack: Option<&Path>,
    output: &Path,
    width: u32,
    height: u32,
) -> Result<(), String> {
    if files.len() != segments.len() { return Err("Timeline source count mismatch".into()); }
    let source_audio = if soundtrack.is_none() {
        let mut info = Vec::with_capacity(files.len());
        for path in files {
            info.push(source_audio(path).await?);
        }
        info
    } else {
        Vec::new()
    };
    let mix_audio = soundtrack.is_none() && source_audio.iter().any(|source| source.has_audio);
    let args = if mix_audio {
        graph_with_audio(segments, &source_audio, width, height)?
    } else {
        filter_graph(segments, width, height)
    };
    let mut cmd = Command::new(ffmpeg_executable());
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    for path in files {
        cmd.arg("-i").arg(path);
    }
    if let Some(music) = soundtrack {
        cmd.args(["-stream_loop", "-1", "-i"]).arg(music);
    }
    cmd.arg("-filter_complex").arg(args)
        .args(["-map", "[outv]"]);
    if soundtrack.is_some() {
        cmd.args(["-map", &format!("{}:a:0", files.len()), "-c:a", "aac", "-b:a", "160k"]);
    } else if mix_audio {
        cmd.args(["-map", "[outa]", "-c:a", "aac", "-b:a", "160k"]);
    } else {
        cmd.arg("-an");
    }
    cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20",
              "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-shortest"])
        .arg(output)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(EXPORT_LIMIT, cmd.status()).await
        .map_err(|_| "Timeline export exceeded the 30-minute limit".to_string())?
        .map_err(|e| format!("Could not launch FFmpeg: {e}"))?;
    if !status.success() { return Err("FFmpeg could not assemble the timeline (check video codecs and audio input)".into()); }
    let m = fs::symlink_metadata(output).map_err(|e| e.to_string())?;
    if !m.is_file() || m.file_type().is_symlink() || m.len() == 0 || m.len() > MAX_INPUT_BYTES {
        return Err("Composed video is empty, too large, or unsafe".into());
    }
    Ok(())
}

fn existing_output(app: &AppHandle, timeline: &VideoTimeline) -> Result<Option<String>, String> {
    Ok(generation_commands::generation_gallery(app.clone())?.into_iter()
        .find(|e| e.model_id == format!("studio:timeline:{}", timeline.id)).map(|e| e.entry_id))
}

fn reconcile(app: &AppHandle, timelines: &mut [VideoTimeline]) -> Result<bool, String> {
    let active = active().lock().map_err(|e| e.to_string())?.clone();
    let mut changed = false;
    for record in timelines.iter_mut() {
        if matches!(record.status, TimelineStatus::Rendering | TimelineStatus::Interrupted | TimelineStatus::Failed) {
            if let Some(existing) = existing_output(app, record)? {
                record.output_entry_id = Some(existing);
                record.status = TimelineStatus::Completed;
                record.error = None;
                changed = true;
            } else if record.status == TimelineStatus::Rendering && !active.contains(&record.id) {
                record.status = TimelineStatus::Interrupted;
                record.error = Some("Timeline export was interrupted; retry recomposes the same saved clips.".into());
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn update(app: &AppHandle, id: &str, mut f: impl FnMut(&mut VideoTimeline) -> Result<(), String>) -> Result<VideoTimeline, String> {
    let _lock = mutex().lock().map_err(|e| e.to_string())?;
    let path = journal_path(app)?;
    let mut timelines = load(&path)?;
    let job = timelines.iter_mut().find(|j| j.id == id).ok_or("Unknown Studio video timeline")?;
    f(job)?;
    job.updated_at_ms = timestamp();
    let snapshot = job.clone();
    save(&path, &timelines)?;
    Ok(snapshot)
}

fn launch_ready(app: &AppHandle) -> Result<(), String> {
    let selected = {
        let _lock = mutex().lock().map_err(|e| e.to_string())?;
        let path = journal_path(app)?;
        let mut timelines = load(&path)?;
        if reconcile(app, &mut timelines)? { save(&path, &timelines)?; }
        timelines.into_iter().filter(|r| r.status == TimelineStatus::Queued).collect::<Vec<_>>()
    };
    // Hold queued exports until every ordered source has published media.
    // A temporary absence is not a failed timeline, even across app restart.
    let video_jobs = crate::studio_video_jobs::generation_video_jobs(app.clone())?;
    let gallery = generation_commands::generation_gallery(app.clone())?;
    for job in selected {
        match clip_entries(&job, &video_jobs, &gallery) {
            Ok(Some(_)) => {},
            Ok(None) => continue,
            Err(error) => {
                let _ = update(app, &job.id, |record| {
                    if record.status == TimelineStatus::Queued {
                        record.status = TimelineStatus::Failed;
                        record.error = Some(error.clone());
                    }
                    Ok(())
                });
                continue;
            }
        }
        let fresh = active().lock().map_err(|e| e.to_string())?.insert(job.id.clone());
        if !fresh { continue; }
        let cancellation = CancellationToken::new();
        cancellations().lock().map_err(|e| e.to_string())?
            .insert(job.id.clone(), cancellation.clone());
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let _gate = export_gate().lock().await;
            let _ = run(&app, &job.id, cancellation).await;
            if let Ok(mut tokens) = cancellations().lock() { tokens.remove(&job.id); }
            if let Ok(mut active) = active().lock() { active.remove(&job.id); }
        });
    }
    Ok(())
}

async fn run(app: &AppHandle, id: &str, cancellation: CancellationToken) -> Result<(), String> {
    let task = update(app, id, |job| {
        if job.status != TimelineStatus::Queued { return Err("Timeline export is no longer queued".into()); }
        job.status = TimelineStatus::Rendering;
        job.error = None;
        Ok(())
    })?;
    // Dropping an in-progress FFmpeg status future kills the process through
    // kill_on_drop, and ScratchDir removes temporary media on cancellation.
    let result = tokio::select! {
        result = assemble(app, &task) => result,
        _ = cancellation.cancelled() => Err("Timeline export cancelled".to_string()),
    };
    let _ = update(app, id, |job| {
        match &result {
            Ok(entry) => {
                job.status = TimelineStatus::Completed;
                job.output_entry_id = Some(entry.entry_id.clone());
                job.error = None;
            }
            Err(e) if job.status != TimelineStatus::Cancelled => {
                job.status = TimelineStatus::Failed;
                job.error = Some(e.chars().take(400).collect());
            }
            _ => {}
        }
        Ok(())
    });
    result.map(|_| ())
}

async fn assemble(app: &AppHandle, timeline: &VideoTimeline) -> Result<GenerationEntry, String> {
    if let Some(entry_id) = existing_output(app, timeline)? {
        return generation_commands::generation_gallery(app.clone())?
            .into_iter().find(|e| e.entry_id == entry_id).ok_or("Timeline gallery checkpoint missing".into());
    }
    let video_jobs = crate::studio_video_jobs::generation_video_jobs(app.clone())?;
    let gallery = generation_commands::generation_gallery(app.clone())?;
    let entries = clip_entries(timeline, &video_jobs, &gallery)?
        .ok_or("Timeline source clips are not complete yet")?;
    let music = validated_soundtrack(timeline.soundtrack_path.as_deref())?;
    let base = app.profile_data_dir().map_err(|e| e.to_string())?;
    let store = crate::artifact_store::ArtifactStore::with_max_blob_size(
        base.join("content-v1"), MAX_INPUT_BYTES,
    ).map_err(|e| e.to_string())?;
    let root = base.join("studio-v1").join("timeline-exports");
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    if fs::symlink_metadata(&root).map_err(|e| e.to_string())?.file_type().is_symlink() {
        return Err("Unsafe timeline export directory".into());
    }
    let scratch = root.join(Uuid::new_v4().to_string());
    fs::create_dir(&scratch).map_err(|e| e.to_string())?;
    let _cleanup = ScratchDir(scratch.clone());
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    let result = async {
        let mut paths = Vec::new();
        for (idx, entry) in entries.iter().enumerate() {
            let bytes = store.read(&entry.artifact_id).map_err(|e| e.to_string())?;
            if bytes.is_empty() || bytes.len() as u64 > MAX_INPUT_BYTES {
                return Err("Timeline clip is empty or exceeds the input limit".into());
            }
            let ext = if entry.media_type == "video/webm" { "webm" } else { "mp4" };
            let path = scratch.join(format!("{idx}.{ext}"));
            fs::write(&path, bytes).map_err(|e| e.to_string())?;
            paths.push(path);
        }
        let (width, height) = probe_canvas(&paths[0]).await?;
        let out = scratch.join("timeline.mp4");
        export_ffmpeg(&paths, &timeline.segments, music.as_deref(), &out, width, height).await?;
        let duration_ms = probe_duration_ms(&out).await?;
        let bytes = fs::read(&out).map_err(|e| e.to_string())?;
        generation_commands::publish_timeline_export(app, timeline, &entries, &bytes, (width, height), duration_ms)
    }.await;
    result
}

pub(crate) fn start_background_recovery(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = launch_ready(&app) { eprintln!("Video timeline startup recovery failed: {e}"); }
    });
}
pub(crate) fn notify_video_published(app: &AppHandle) {
    if let Err(e) = launch_ready(app) { eprintln!("Video timeline dispatch failed: {e}"); }
}

#[tauri::command(rename_all = "camelCase")]
pub fn generation_video_timeline_submit(
    app: AppHandle,
    title: String,
    segments: Vec<VideoTimelineSegment>,
    soundtrack_path: Option<String>,
) -> Result<VideoTimeline, String> {
    validate_segments(&segments)?;
    if title.trim().is_empty() || title.len() > 200 {
        return Err("Timeline needs a title of at most 200 characters".into());
    }
    validated_soundtrack(soundtrack_path.as_deref())?;
    let new = {
        let _lock = mutex().lock().map_err(|e| e.to_string())?;
        let path = journal_path(&app)?;
        let mut existing = load(&path)?;
        if existing.len() >= MAX_TIMELINES { return Err("Studio timeline history is full".into()); }
        let now = timestamp();
        let record = VideoTimeline {
            id: Uuid::new_v4().to_string(),
            title, segments, soundtrack_path,
            status: TimelineStatus::Queued,
            output_entry_id: None, error: None, created_at_ms: now, updated_at_ms: now,
        };
        existing.push(record.clone());
        save(&path, &existing)?;
        record
    };
    launch_ready(&app)?;
    Ok(new)
}

#[tauri::command]
pub fn generation_video_timelines(app: AppHandle) -> Result<Vec<VideoTimeline>, String> {
    let records = {
        let _lock = mutex().lock().map_err(|e| e.to_string())?;
        let path = journal_path(&app)?;
        let mut records = load(&path)?;
        if reconcile(&app, &mut records)? { save(&path, &records)?; }
        records.into_iter().rev().collect::<Vec<_>>()
    };
    launch_ready(&app)?;
    Ok(records)
}

#[tauri::command(rename_all = "camelCase")]
pub fn generation_video_timeline_retry(app: AppHandle, timeline_id: String) -> Result<VideoTimeline, String> {
    let record = update(&app, &timeline_id, |job| {
        if !matches!(job.status, TimelineStatus::Failed | TimelineStatus::Interrupted) {
            return Err("Only failed or interrupted timeline exports can be retried".into());
        }
        job.status = TimelineStatus::Queued;
        job.error = None;
        Ok(())
    })?;
    launch_ready(&app)?;
    Ok(record)
}

#[tauri::command(rename_all = "camelCase")]
pub fn generation_video_timeline_cancel(app: AppHandle, timeline_id: String) -> Result<VideoTimeline, String> {
    let record = update(&app, &timeline_id, |job| {
        if !matches!(job.status, TimelineStatus::Queued | TimelineStatus::Rendering) {
            return Err("Only queued or rendering timeline exports can be cancelled".into());
        }
        job.status = TimelineStatus::Cancelled;
        job.error = None;
        Ok(())
    })?;
    if let Ok(tokens) = cancellations().lock() {
        if let Some(token) = tokens.get(&timeline_id) { token.cancel(); }
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn segment(id: &str) -> VideoTimelineSegment {
        VideoTimelineSegment { job_id: id.into(), trim_start_ms: 0, trim_end_ms: None }
    }
    #[test]
    fn requires_distinct_valid_source_jobs_and_trim_ranges() {
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();
        assert!(validate_segments(&[segment(&a),segment(&b)]).is_ok());
        assert!(validate_segments(&[segment(&a),segment(&a)]).is_err());
        assert!(validate_segments(&[segment("../escape"),segment(&b)]).is_err());
        let mut bad = segment(&a);
        bad.trim_end_ms=Some(0);
        assert!(validate_segments(&[bad,segment(&b)]).is_err());
    }
    #[test]
    fn ordered_filter_graph_is_bounded_and_tracks_editor_trims() {
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();
        let graph = filter_graph(&[
            VideoTimelineSegment { job_id:a, trim_start_ms:500, trim_end_ms:Some(1200) },
            segment(&b),
        ], 640, 384);
        assert!(graph.contains("trim=start=0.500:end=1.200"));
        assert!(graph.contains("[v0][v1]concat=n=2:v=1:a=0[outv]"));
        assert!(graph.contains("pad=640:384"));
    }
    #[test]
    fn gallery_video_refs_are_explicit_and_path_safe() {
        let uuid = Uuid::new_v4().to_string();
        assert!(valid_source_id(&format!("gallery:studio-{uuid}")));
        assert!(!valid_source_id("gallery:../../etc/passwd"));
        assert!(!valid_source_id("gallery:studio-not-a-uuid"));
        assert!(!valid_source_id(&format!("gallery:{uuid}")));
    }
    #[test]
    fn audio_concat_preserves_audio_and_fills_missing_segment_with_silence() {
        let ids = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
        let parts = ids.iter().map(|id| segment(id)).collect::<Vec<_>>();
        let graph = graph_with_audio(&parts,
            &[SourceAudio { has_audio: true, duration_ms: 1000 },
              SourceAudio { has_audio: false, duration_ms: 1000 }], 640, 384).unwrap();
        assert!(graph.contains("[0:a:0]atrim"));
        assert!(graph.contains("anullsrc=r=48000:cl=stereo"));
        assert!(graph.contains("[v0][a0][v1][a1]concat=n=2:v=1:a=1[outv][outa]"));
    }

    #[test]
    fn soundtrack_rejects_unbounded_or_relative_files() {
        assert!(validated_soundtrack(Some("../../../something.mp3")).is_err());
        assert!(validated_soundtrack(Some("")).unwrap().is_none());
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn actual_ffmpeg_assembles_two_distinct_video_segments() {
        let root = std::env::temp_dir().join(format!("lm-timeline-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let a = root.join("a.mp4");
        let b = root.join("b.mp4");
        for (color, dest) in [("red", &a), ("blue", &b)] {
            let status = std::process::Command::new("ffmpeg")
                .args(["-nostdin","-loglevel","error","-f","lavfi","-i",
                       &format!("color={color}:size=64x64:rate=24:duration=1"),
                       "-c:v","mpeg4","-y"]).arg(dest).status().unwrap();
            assert!(status.success(), "FFmpeg fixture generation failed");
        }
        // First source contains a real audio stream; the second has none.
        // The compositor must carry the first and pad the second with silence.
        let with_audio = root.join("a_with_audio.mp4");
        let attached = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-i"])
            .arg(&a)
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                   "-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy",
                   "-c:a", "aac", "-y"])
            .arg(&with_audio).status().unwrap();
        assert!(attached.success(), "Audio fixture must be muxed into first clip");
        fs::rename(&with_audio, &a).unwrap();

        let clip_ids = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
        let segments = clip_ids.iter().map(|id| segment(id)).collect::<Vec<_>>();
        let out = root.join("out.mp4");
        export_ffmpeg(&[a,b], &segments, None, &out, 64, 64).await.unwrap();
        let metadata = fs::metadata(&out).unwrap();
        assert!(metadata.len() > 500);
        let duration = probe_duration_ms(&out).await.unwrap();
        assert!(duration >= 1950 && duration <= 2050, "two full one-second clips must produce two seconds");
        let verify = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-i"])
            .arg(&out).args(["-f","null","-"]).status().unwrap();
        assert!(verify.success(), "Exported MP4 must decode all frames");
        let audio_check = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type",
                   "-of", "csv=p=0"])
            .arg(&out).output().unwrap();
        assert!(audio_check.status.success());
        assert!(String::from_utf8_lossy(&audio_check.stdout).contains("audio"),
            "Source audio must survive mixed audio/silent clip concatenation");
        fs::remove_dir_all(root).unwrap();
    }
}
