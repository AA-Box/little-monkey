//! App-owned model installation and one-session llama-server lifecycle.
//!
//! `monkey pull <reference>` and `monkey run <reference>` deliberately do
//! not depend on an Ollama daemon or binary. Resolution and installation are
//! delegated to `little_monkey_lib::model_sources`, so the CLI and desktop
//! app share the same public-model restrictions, immutable checksum receipt,
//! resumable `.part` file, final SHA-256 verification, and provenance
//! sidecar. A managed run then launches only Little Monkey's verified bundled
//! llama-server on an ephemeral loopback port and owns that child until the
//! chat session ends.

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use little_monkey_lib::egress;
use little_monkey_lib::model_sources::{
    self, InstalledModelReference, ModelDownloadProgress, ModelReferenceSource,
};

const CHAT_CONTEXT_TOKENS: u32 = 4096;
const MIN_CHAT_CONTEXT_TOKENS: i64 = 256;
const MAX_CHAT_CONTEXT_TOKENS: i64 = 262_144;
const CHAT_GPU_LAYERS: i32 = 999;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(300);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(400);
const MAX_MODELS_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_START_ATTEMPTS: usize = 3;

fn app_data_dir() -> Result<PathBuf, String> {
    little_monkey_lib::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app data directory".to_string())
}

fn managed_models_dir(app_data: &Path) -> Result<PathBuf, String> {
    let directory = app_data.join("models");
    std::fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Failed to create managed models directory {}: {error}",
            directory.display()
        )
    })?;
    Ok(directory)
}

fn source_label(source: ModelReferenceSource) -> &'static str {
    match source {
        ModelReferenceSource::OllamaRegistry => "Ollama registry",
        ModelReferenceSource::HuggingFace => "Hugging Face",
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

struct InstallProgress {
    interactive: bool,
    last_bucket: Option<u64>,
}

impl InstallProgress {
    fn new() -> Self {
        Self {
            interactive: std::io::stderr().is_terminal(),
            last_bucket: None,
        }
    }

    fn update(&mut self, event: ModelDownloadProgress) {
        let percent = event
            .downloaded
            .saturating_mul(100)
            .checked_div(event.total.max(1))
            .unwrap_or(0)
            .min(100);
        if self.interactive {
            eprint!(
                "\rDownloading {:<36} {:>6.1}%  {} / {}",
                truncate_label(&event.file, 36),
                percent as f64,
                human_bytes(event.downloaded),
                human_bytes(event.total)
            );
            let _ = std::io::stderr().flush();
            self.last_bucket = Some(percent / 10);
            return;
        }

        let bucket = percent / 25;
        if self.last_bucket != Some(bucket) || percent == 100 {
            eprintln!(
                "Downloading {}: {percent}% ({} / {})",
                event.file,
                human_bytes(event.downloaded),
                human_bytes(event.total)
            );
            self.last_bucket = Some(bucket);
        }
    }

    fn finish(&self) {
        if self.interactive {
            eprintln!();
        }
    }
}

fn truncate_label(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    format!("{}…", value.chars().take(keep).collect::<String>())
}

/// Resolves and installs one supported public reference into
/// `<app_data>/models`. Install performs its own second resolution and rejects
/// any digest drift before writing bytes.
async fn install_from_source(reference: &str) -> Result<InstalledModelReference, String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err("Model reference cannot be empty".to_string());
    }

    eprintln!("Resolving {reference}...");
    let resolved = model_sources::resolve_reference(reference).await?;
    eprintln!(
        "Resolved {} from {}: {} ({})",
        resolved.display_name,
        source_label(resolved.source),
        resolved.file_name,
        human_bytes(resolved.size_bytes)
    );
    eprintln!("SHA-256: {}", resolved.sha256);
    if let Some(license) = resolved.license_name.as_deref() {
        eprintln!("License: {license}");
    }

    let data = app_data_dir()?;
    let models = managed_models_dir(&data)?;
    let expected_sha256 = resolved.sha256.clone();
    let mut progress = InstallProgress::new();
    let installed =
        model_sources::install_reference(&models, reference, &expected_sha256, |event| {
            progress.update(event)
        })
        .await;
    progress.finish();
    let installed = installed?;
    eprintln!("Installed: {}", installed.local_path.display());
    if !installed.provenance.tool_calling {
        eprintln!(
            "Warning: this GGUF does not contain a compatible embedded Jinja tool template; ordinary chat can still work."
        );
    }
    Ok(installed)
}

/// `run` first reuses a matching, validated provenance sidecar so installed
/// models work without network access. Provenance retains both the requested
/// reference and its immutable canonical resolution, allowing either form to
/// select the same verified local file later.
pub async fn install_for_run(reference: &str) -> Result<InstalledModelReference, String> {
    let data = app_data_dir()?;
    let models = managed_models_dir(&data)?;
    if let Some(installed) = model_sources::find_installed_reference(&models, reference)? {
        eprintln!("Using installed model: {}", installed.local_path.display());
        return Ok(installed);
    }
    install_from_source(reference).await
}

/// Managed `pull` rejects `--insecure`: the shared installer intentionally
/// accepts only authenticated public HTTPS sources and never downgrades TLS.
pub async fn pull(reference: &str, insecure: bool) -> Result<(), String> {
    if insecure {
        return Err(
            "`--insecure` is not supported for app-owned model installs. Little Monkey only downloads verified public models over HTTPS. Use `monkey --provider ollama pull ... --insecure` only when you explicitly want the legacy Ollama daemon path."
                .to_string(),
        );
    }
    let _ = install_from_source(reference).await?;
    Ok(())
}

fn managed_llama_server(app_data: &Path) -> Result<PathBuf, String> {
    match little_monkey_lib::managed_runtime::materialize_bundled_runtime(None, app_data)? {
        Some(path) => Ok(path),
        None => little_monkey_lib::managed_runtime::find_managed_llama_server(Some(app_data))
            .ok_or_else(|| {
                "Little Monkey's verified bundled llama-server runtime is unavailable. Reinstall the app to restore it; source-build developers can run `pnpm stage:runtime` or set LITTLE_MONKEY_LLAMA_RUNTIME to a deliberate test runtime."
                    .to_string()
            }),
    }
}

fn candidate_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("Failed to find a candidate loopback port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("Failed to inspect the candidate loopback port: {error}"))
}

pub fn context_tokens(requested: Option<i64>) -> Result<u32, String> {
    match requested {
        None => Ok(CHAT_CONTEXT_TOKENS),
        Some(value)
            if (MIN_CHAT_CONTEXT_TOKENS..=MAX_CHAT_CONTEXT_TOKENS).contains(&value) =>
        {
            Ok(value as u32)
        }
        Some(value) => Err(format!(
            "--num-ctx must be between {MIN_CHAT_CONTEXT_TOKENS} and {MAX_CHAT_CONTEXT_TOKENS} tokens for managed models; got {value}"
        )),
    }
}

fn chat_server_args(
    model_path: &Path,
    port: u16,
    context_tokens: u32,
    alias: &str,
) -> Vec<OsString> {
    let mut args = vec![
        "-m".into(),
        model_path.as_os_str().to_owned(),
        "--host".into(),
        Ipv4Addr::LOCALHOST.to_string().into(),
        "--port".into(),
        port.to_string().into(),
        "-c".into(),
        context_tokens.to_string().into(),
        "-ngl".into(),
        CHAT_GPU_LAYERS.to_string().into(),
        "--jinja".into(),
    ];
    args.push("--alias".into());
    args.push(alias.into());
    args
}

/// Owns a managed llama-server child for exactly one CLI chat session.
/// Normal return, startup failure, and unwinding all terminate and reap it.
pub struct ManagedServerSession {
    child: Option<Child>,
    port: u16,
    model_alias: String,
}

impl ManagedServerSession {
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", Ipv4Addr::LOCALHOST, self.port)
    }

    pub fn model_alias(&self) -> &str {
        &self.model_alias
    }
}

impl Drop for ManagedServerSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn wait_until_healthy(
    client: &reqwest::Client,
    child: &mut Child,
    port: u16,
    expected_alias: &str,
) -> Result<(), String> {
    let health_url = format!("http://{}:{port}/health", Ipv4Addr::LOCALHOST);
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "Managed llama-server exited before becoming healthy ({status})"
                ))
            }
            Ok(None) => {}
            Err(error) => {
                return Err(format!(
                    "Failed to inspect managed llama-server process: {error}"
                ))
            }
        }

        if let Ok(response) =
            egress::send(client.get(&health_url).timeout(Duration::from_secs(2))).await
        {
            if response.status().is_success()
                && server_reports_alias(client, port, expected_alias).await
            {
                // The port candidate was released before spawn, so another
                // process can still win the bind race. Prove our child
                // remains alive after the nonce identity response.
                return match child.try_wait() {
                    Ok(None) => Ok(()),
                    Ok(Some(status)) => Err(format!(
                        "Managed llama-server exited before becoming healthy ({status})"
                    )),
                    Err(error) => Err(format!(
                        "Failed to inspect managed llama-server process: {error}"
                    )),
                };
            }
        }
        tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
    }
    Err(format!(
        "Timed out after {}s waiting for managed llama-server health",
        HEALTH_TIMEOUT.as_secs()
    ))
}

async fn server_reports_alias(client: &reqwest::Client, port: u16, expected_alias: &str) -> bool {
    let models_url = format!("http://{}:{port}/v1/models", Ipv4Addr::LOCALHOST);
    // Chunked below rather than buffered, and metering does not change that.
    let mut response =
        match egress::send(client.get(models_url).timeout(Duration::from_secs(2))).await {
            Ok(response) if response.status().is_success() => response,
            _ => return false,
        };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODELS_RESPONSE_BYTES as u64)
    {
        return false;
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => return false,
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_MODELS_RESPONSE_BYTES {
            return false;
        }
        bytes.extend_from_slice(&chunk);
    }
    models_payload_reports_alias(&bytes, expected_alias)
}

fn models_payload_reports_alias(bytes: &[u8], expected_alias: &str) -> bool {
    let payload: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(payload) => payload,
        Err(_) => return false,
    };
    payload["data"].as_array().is_some_and(|models| {
        models
            .iter()
            .any(|model| model["id"].as_str() == Some(expected_alias))
    })
}

/// Materializes/finds the verified app-owned runtime, starts it against the
/// installed GGUF on an available loopback port, and waits for `/health`.
pub async fn start_server(
    client: &reqwest::Client,
    installed: &InstalledModelReference,
    context_tokens: u32,
) -> Result<ManagedServerSession, String> {
    model_sources::verify_managed_model_for_runtime(&installed.local_path)?;
    let data = app_data_dir()?;
    let binary = managed_llama_server(&data)?;
    for attempt in 1..=MAX_START_ATTEMPTS {
        let port = candidate_loopback_port()?;
        let startup_alias = little_monkey_lib::llama::fresh_server_alias();
        let args = chat_server_args(&installed.local_path, port, context_tokens, &startup_alias);
        eprintln!("Starting Little Monkey's managed llama-server on 127.0.0.1:{port}...");

        let mut command = Command::new(&binary);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(directory) = binary.parent() {
            command.current_dir(directory);
        }
        let mut child = command.spawn().map_err(|error| {
            format!(
                "Failed to start managed llama-server {}: {error}",
                binary.display()
            )
        })?;

        match wait_until_healthy(client, &mut child, port, &startup_alias).await {
            Ok(()) => {
                eprintln!("Managed model ready.");
                return Ok(ManagedServerSession {
                    child: Some(child),
                    port,
                    model_alias: startup_alias,
                });
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let can_retry = error.contains("exited before becoming healthy")
                    && attempt < MAX_START_ATTEMPTS;
                if can_retry {
                    eprintln!(
                        "Managed llama-server exited during startup; retrying with a fresh loopback port ({}/{MAX_START_ATTEMPTS})...",
                        attempt + 1
                    );
                    continue;
                }
                return Err(error);
            }
        }
    }
    Err("Managed llama-server could not claim a loopback port".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_server_args_bind_only_loopback_and_enable_jinja_tools() {
        let args = chat_server_args(
            Path::new("/models/model.gguf"),
            32123,
            8192,
            "hf:org/repo@commit#model.gguf",
        )
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "-m",
                "/models/model.gguf",
                "--host",
                "127.0.0.1",
                "--port",
                "32123",
                "-c",
                "8192",
                "-ngl",
                "999",
                "--jinja",
                "--alias",
                "hf:org/repo@commit#model.gguf",
            ]
        );
    }

    #[test]
    fn managed_server_identity_requires_the_exact_model_alias() {
        let payload = br#"{
            "object": "list",
            "data": [
                {"id": "hf:org/repo@commit#model.gguf", "object": "model"}
            ]
        }"#;
        assert!(models_payload_reports_alias(
            payload,
            "hf:org/repo@commit#model.gguf"
        ));
        assert!(!models_payload_reports_alias(payload, "other-model"));
        assert!(!models_payload_reports_alias(b"not-json", "other-model"));
    }

    #[test]
    fn managed_startup_aliases_are_unique_nonces() {
        let first = little_monkey_lib::llama::fresh_server_alias();
        let second = little_monkey_lib::llama::fresh_server_alias();
        assert_ne!(first, second);
        assert!(!first.contains("hf:org/repo"));
    }

    #[test]
    fn managed_context_defaults_and_validates_range() {
        assert_eq!(context_tokens(None).unwrap(), 4096);
        assert_eq!(context_tokens(Some(256)).unwrap(), 256);
        assert_eq!(context_tokens(Some(262_144)).unwrap(), 262_144);
        assert!(context_tokens(Some(255)).unwrap_err().contains("--num-ctx"));
        assert!(context_tokens(Some(262_145))
            .unwrap_err()
            .contains("262144"));
    }

    #[test]
    fn available_port_is_ephemeral_and_loopback_rebindable() {
        let port = match candidate_loopback_port() {
            Ok(port) => port,
            // Some restricted CI/sandbox profiles prohibit even a loopback
            // bind. Production still reports that failure to the caller.
            Err(error)
                if error.contains("Operation not permitted")
                    || error.contains("Permission denied") =>
            {
                return
            }
            Err(error) => panic!("{error}"),
        };
        assert_ne!(port, 0);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        assert_eq!(listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn managed_insecure_pull_is_rejected_without_network_access() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime.block_on(pull("qwen3", true)).unwrap_err();
        assert!(error.contains("--insecure"));
        assert!(error.contains("--provider ollama"));
    }

    #[test]
    fn progress_labels_are_unicode_safe_and_bounded() {
        let value = truncate_label("模型-very-long-model-name.gguf", 12);
        assert!(value.chars().count() <= 12);
        assert!(value.ends_with('…'));
    }

    #[test]
    fn byte_display_uses_decimal_model_download_units() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_500_000), "1.5 MB");
        assert_eq!(human_bytes(4_700_000_000), "4.7 GB");
    }
}
