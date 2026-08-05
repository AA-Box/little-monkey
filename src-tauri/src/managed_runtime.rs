//! Verified app-owned native runtime discovery and materialization.
//!
//! Release builds bundle pinned runtime trees under Tauri resources (staged by
//! `scripts/stage-managed-runtime.mjs`). This module verifies such a tree
//! against its per-file manifest and atomically copies it into the shared
//! Little Monkey app-data directory, where both the desktop process and the
//! separately installed `monkey` CLI can find it.
//!
//! Two runtimes ride these rails: [`LLAMA`] (`llama-server`, chat/embeddings)
//! and [`STABLE_DIFFUSION`] (`sd-server`, image and video generation). Each is
//! described by a [`ManagedRuntimeSpec`] and gets its own versioned directory,
//! its own compile-time trusted manifest digest, and its own install lock, so
//! publishing one can never disturb the other. System `llama-server` discovery
//! remains a development fallback in `llama.rs`; it is not the shipped path.

use crate::process_lock::acquire_cross_process_lock;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub const MANAGED_LLAMA_VERSION: &str = "b9637";
/// Pinned stable-diffusion.cpp release. Upstream tags releases as
/// `master-<build>-<commit>`; the whole tag is the version so the staged
/// directory name is unambiguous across rebuilds of the same commit.
pub const MANAGED_SD_VERSION: &str = "master-812-ea7f0c8";
const MANIFEST_FILE: &str = "runtime-manifest.json";
const MAX_RUNTIME_FILES: usize = 256;
const MAX_RUNTIME_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const TRUSTED_RUNTIME_MANIFEST_SHA256: Option<&str> =
    option_env!("LITTLE_MONKEY_TRUSTED_RUNTIME_MANIFEST_SHA256");
const TRUSTED_SD_MANIFEST_SHA256: Option<&str> =
    option_env!("LITTLE_MONKEY_TRUSTED_SD_MANIFEST_SHA256");

/// One managed native runtime: what to verify, where to publish it, and which
/// binary inside the tree is the one callers actually launch.
#[derive(Clone, Copy, Debug)]
pub struct ManagedRuntimeSpec {
    /// Directory segment under `<app-data>/runtimes/` and in the staged
    /// resource name (`managed-runtime/<id>-<version>`).
    pub id: &'static str,
    /// Value the manifest's `runtime` field must carry.
    pub manifest_runtime: &'static str,
    pub version: &'static str,
    /// Release URL prefix the manifest's `sourceUrl` must start with.
    pub source_url_prefix: &'static str,
    /// Development/CI override pointing at a directory or the binary itself.
    pub override_env: &'static str,
    /// Host targets this runtime publishes binaries for. A host outside the
    /// list has no managed runtime rather than a mismatched one.
    pub supported_targets: &'static [&'static str],
    executable_unix: &'static str,
    executable_windows: &'static str,
    trusted_manifest_sha256: Option<&'static str>,
}

impl ManagedRuntimeSpec {
    /// The launchable binary's flat file name inside the runtime tree.
    pub fn executable(&self) -> &'static str {
        if cfg!(target_os = "windows") {
            self.executable_windows
        } else {
            self.executable_unix
        }
    }

    fn trusted_manifest_digest(&self) -> Result<&'static str, String> {
        self.trusted_manifest_sha256
            .filter(|digest| valid_sha256(digest))
            .ok_or_else(|| {
                format!(
                    "No trusted managed {} manifest is embedded in this build; source-build developers must run `pnpm stage:runtime` and rebuild",
                    self.id
                )
            })
    }
}

/// llama.cpp — chat and embedding inference. Ships for every desktop target.
pub const LLAMA: ManagedRuntimeSpec = ManagedRuntimeSpec {
    id: "llama",
    manifest_runtime: "llama.cpp",
    version: MANAGED_LLAMA_VERSION,
    source_url_prefix: "https://github.com/ggml-org/llama.cpp/releases/",
    override_env: "LITTLE_MONKEY_LLAMA_RUNTIME",
    supported_targets: &[
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        "aarch64-pc-windows-msvc",
        "x86_64-pc-windows-msvc",
    ],
    executable_unix: "llama-server",
    executable_windows: "llama-server.exe",
    trusted_manifest_sha256: TRUSTED_RUNTIME_MANIFEST_SHA256,
};

/// stable-diffusion.cpp — image and video generation. Upstream publishes
/// prebuilt binaries for three hosts only (Metal on Apple silicon, Vulkan on
/// x86_64 Windows and Linux), so the other targets get no managed runtime and
/// the Studio surface stays unavailable there rather than failing at launch.
pub const STABLE_DIFFUSION: ManagedRuntimeSpec = ManagedRuntimeSpec {
    id: "sd",
    manifest_runtime: "stable-diffusion.cpp",
    version: MANAGED_SD_VERSION,
    source_url_prefix: "https://github.com/leejet/stable-diffusion.cpp/releases/",
    override_env: "LITTLE_MONKEY_SD_RUNTIME",
    supported_targets: &[
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ],
    executable_unix: "sd-server",
    executable_windows: "sd-server.exe",
    trusted_manifest_sha256: TRUSTED_SD_MANIFEST_SHA256,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeManifestFile {
    name: String,
    sha256: String,
    size_bytes: u64,
    executable: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeManifest {
    schema_version: u32,
    runtime: String,
    version: String,
    target: String,
    source_url: String,
    archive_sha256: String,
    files: Vec<RuntimeManifestFile>,
}

pub fn llama_server_filename() -> &'static str {
    LLAMA.executable()
}

pub fn sd_server_filename() -> &'static str {
    STABLE_DIFFUSION.executable()
}

/// `llama-tts` — speech generation and voice cloning. It lives in the same
/// verified tree as `llama-server`, so it inherits that tree's pinned version
/// and per-file checksums rather than needing a runtime of its own.
pub fn llama_tts_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "llama-tts.exe"
    } else {
        "llama-tts"
    }
}

/// The verified `llama-tts` beside an already-materialized `llama-server`.
pub fn find_managed_llama_tts(app_data_dir: Option<&Path>) -> Option<PathBuf> {
    let server = find_managed_llama_server(app_data_dir)?;
    let tts = server.with_file_name(llama_tts_filename());
    tts.is_file().then_some(tts)
}

pub fn managed_runtime_dir_for(spec: &ManagedRuntimeSpec, app_data_dir: &Path) -> PathBuf {
    app_data_dir
        .join("runtimes")
        .join(spec.id)
        .join(spec.version)
}

pub fn managed_runtime_dir(app_data_dir: &Path) -> PathBuf {
    managed_runtime_dir_for(&LLAMA, app_data_dir)
}

/// The host triple, independent of which runtime is being verified.
fn host_target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        Some("aarch64-pc-windows-msvc")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// The host target, but only when `spec` actually publishes a binary for it.
fn expected_runtime_target(spec: &ManagedRuntimeSpec) -> Option<&'static str> {
    host_target().filter(|target| spec.supported_targets.contains(target))
}

/// Whether this host can run `spec` at all. Callers use it to hide a surface
/// rather than to offer it and fail at launch.
pub fn runtime_supported_here(spec: &ManagedRuntimeSpec) -> bool {
    expected_runtime_target(spec).is_some()
}

fn is_safe_flat_name(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_file(path: &Path, expected_size: u64) -> Result<String, String> {
    if expected_size > MAX_RUNTIME_FILE_BYTES {
        return Err(format!(
            "Managed runtime file {} exceeds the {} byte safety limit",
            path.display(),
            MAX_RUNTIME_FILE_BYTES
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Failed to inspect runtime file {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "Managed runtime entry {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() != expected_size {
        return Err(format!(
            "Managed runtime file {} has size {}, expected {}",
            path.display(),
            metadata.len(),
            expected_size
        ));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Failed to open runtime file {}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Failed to read runtime file {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn read_manifest_bytes(directory: &Path) -> Result<Vec<u8>, String> {
    let path = directory.join(MANIFEST_FILE);
    let bytes = fs::read(&path).map_err(|error| {
        format!(
            "Failed to read managed runtime manifest {}: {error}",
            path.display()
        )
    })?;
    if bytes.len() > 1024 * 1024 {
        return Err("Managed runtime manifest exceeds 1 MiB".to_string());
    }
    Ok(bytes)
}

fn read_trusted_manifest(
    directory: &Path,
    trusted_manifest_sha256: &str,
) -> Result<(RuntimeManifest, Vec<u8>), String> {
    if !valid_sha256(trusted_manifest_sha256) {
        return Err(
            "No trusted managed runtime manifest is embedded in this build; source-build developers must run `pnpm stage:runtime` and rebuild"
                .to_string(),
        );
    }
    let path = directory.join(MANIFEST_FILE);
    let bytes = read_manifest_bytes(directory)?;
    let actual_manifest_sha256 = format!("{:x}", Sha256::digest(&bytes));
    if actual_manifest_sha256 != trusted_manifest_sha256 {
        return Err(format!(
            "Managed runtime manifest checksum mismatch for {}: expected {}, got {}",
            path.display(),
            trusted_manifest_sha256,
            actual_manifest_sha256
        ));
    }
    let manifest: RuntimeManifest = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "Invalid managed runtime manifest {}: {error}",
            path.display()
        )
    })?;
    Ok((manifest, bytes))
}

fn verify_runtime_directory_with_digest(
    spec: &ManagedRuntimeSpec,
    directory: &Path,
    trusted_manifest_sha256: &str,
) -> Result<PathBuf, String> {
    // Authenticate the manifest against compile-time trust data before
    // parsing any filenames or checksums from it.
    let (manifest, _) = read_trusted_manifest(directory, trusted_manifest_sha256)?;
    if manifest.schema_version != 1
        || manifest.runtime != spec.manifest_runtime
        || manifest.version != spec.version
    {
        return Err(format!(
            "Unsupported managed runtime manifest: schema {}, runtime {}, version {}",
            manifest.schema_version, manifest.runtime, manifest.version
        ));
    }
    let expected_target = expected_runtime_target(spec).ok_or_else(|| {
        format!(
            "Managed {} runtime is unsupported on this platform",
            spec.manifest_runtime
        )
    })?;
    if manifest.target != expected_target
        || !valid_sha256(&manifest.archive_sha256)
        || !manifest.source_url.starts_with(spec.source_url_prefix)
    {
        return Err("Managed runtime provenance is invalid".to_string());
    }
    if manifest.files.is_empty() || manifest.files.len() > MAX_RUNTIME_FILES {
        return Err(format!(
            "Managed runtime manifest contains an invalid file count ({})",
            manifest.files.len()
        ));
    }

    let server_name = spec.executable();
    let mut found_server = false;
    let mut expected_files = HashSet::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        if !is_safe_flat_name(&entry.name)
            || !valid_sha256(&entry.sha256)
            || !expected_files.insert(entry.name.clone())
        {
            return Err(format!(
                "Managed runtime manifest contains an invalid file entry: {}",
                entry.name
            ));
        }
        let path = directory.join(&entry.name);
        let actual = sha256_file(&path, entry.size_bytes)?;
        if !actual.eq_ignore_ascii_case(&entry.sha256) {
            return Err(format!(
                "Managed runtime checksum mismatch for {}: expected {}, got {}",
                path.display(),
                entry.sha256,
                actual
            ));
        }
        if entry.name == server_name {
            if !entry.executable {
                return Err(format!("Managed {server_name} is not marked executable"));
            }
            found_server = true;
        }
    }
    for entry in fs::read_dir(directory).map_err(|error| {
        format!(
            "Failed to list managed runtime directory {}: {error}",
            directory.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to inspect managed runtime directory {}: {error}",
                directory.display()
            )
        })?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "Managed runtime contains a non-UTF-8 filename".to_string())?;
        if name == MANIFEST_FILE {
            continue;
        }
        if !expected_files.contains(&name) {
            return Err(format!(
                "Managed runtime contains an unexpected entry: {}",
                entry.path().display()
            ));
        }
    }
    if !found_server {
        return Err(format!(
            "Managed runtime manifest does not contain {server_name}"
        ));
    }
    Ok(directory.join(server_name))
}

fn verify_runtime_directory(
    spec: &ManagedRuntimeSpec,
    directory: &Path,
) -> Result<PathBuf, String> {
    verify_runtime_directory_with_digest(spec, directory, spec.trusted_manifest_digest()?)
}

fn runtime_candidates(spec: &ManagedRuntimeSpec, base: &Path) -> [PathBuf; 3] {
    let staged = format!("{}-{}", spec.id, spec.version);
    [
        base.join("managed-runtime").join(&staged),
        base.join("resources").join("managed-runtime").join(&staged),
        base.join("Resources")
            .join("resources")
            .join("managed-runtime")
            .join(&staged),
    ]
}

fn bundled_runtime_near_current_exe(spec: &ManagedRuntimeSpec) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    for ancestor in executable.ancestors().take(7) {
        for candidate in runtime_candidates(spec, ancestor) {
            if candidate.join(MANIFEST_FILE).is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn bundled_runtime_in_for(
    spec: &ManagedRuntimeSpec,
    resource_dir: &Path,
) -> Option<PathBuf> {
    runtime_candidates(spec, resource_dir)
        .into_iter()
        .find(|candidate| candidate.join(MANIFEST_FILE).is_file())
        .or_else(|| bundled_runtime_near_current_exe(spec))
}

pub fn bundled_runtime_in(resource_dir: &Path) -> Option<PathBuf> {
    bundled_runtime_in_for(&LLAMA, resource_dir)
}

fn explicit_runtime_override(spec: &ManagedRuntimeSpec) -> Option<PathBuf> {
    let value = std::env::var_os(spec.override_env)?;
    let path = PathBuf::from(value);
    if path.is_dir() {
        Some(path.join(spec.executable()))
    } else {
        Some(path)
    }
}

/// Finds an already materialized/bundled app-owned runtime. The explicit
/// environment override exists for development and CI fixtures only.
pub fn find_managed_server(
    spec: &ManagedRuntimeSpec,
    app_data_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(override_path) = explicit_runtime_override(spec) {
        if override_path.is_file() {
            return Some(override_path);
        }
    }
    if let Some(root) = app_data_dir {
        let installed = managed_runtime_dir_for(spec, root);
        if let Ok(server) = verify_runtime_directory(spec, &installed) {
            return Some(server);
        }
    }
    let bundled = bundled_runtime_near_current_exe(spec)?;
    verify_runtime_directory(spec, &bundled).ok()
}

pub fn find_managed_llama_server(app_data_dir: Option<&Path>) -> Option<PathBuf> {
    find_managed_server(&LLAMA, app_data_dir)
}

pub fn find_managed_sd_server(app_data_dir: Option<&Path>) -> Option<PathBuf> {
    find_managed_server(&STABLE_DIFFUSION, app_data_dir)
}

/// Verifies the bundled runtime and publishes a private copy under app data.
/// Returns `Ok(None)` for developer/source builds without staged resources;
/// release builds always have a bundle and therefore return `Some`.
pub fn materialize_bundled_runtime_for(
    spec: &ManagedRuntimeSpec,
    resource_dir: Option<&Path>,
    app_data_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    let source = resource_dir
        .and_then(|directory| bundled_runtime_in_for(spec, directory))
        .or_else(|| bundled_runtime_near_current_exe(spec));
    let Some(source) = source else {
        return Ok(None);
    };
    let trusted_manifest_sha256 = spec.trusted_manifest_digest()?;
    materialize_runtime_from_source(spec, &source, app_data_dir, trusted_manifest_sha256).map(Some)
}

pub fn materialize_bundled_runtime(
    resource_dir: Option<&Path>,
    app_data_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    materialize_bundled_runtime_for(&LLAMA, resource_dir, app_data_dir)
}

pub fn materialize_bundled_sd_runtime(
    resource_dir: Option<&Path>,
    app_data_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    materialize_bundled_runtime_for(&STABLE_DIFFUSION, resource_dir, app_data_dir)
}

fn remove_invalid_runtime_destination(destination: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Failed to inspect invalid managed runtime {}: {error}",
                destination.display()
            ))
        }
    };
    let result = if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(destination)
    } else {
        fs::remove_file(destination)
    };
    result.map_err(|error| {
        format!(
            "Failed to remove invalid managed runtime {}: {error}",
            destination.display()
        )
    })
}

fn materialize_runtime_from_source(
    spec: &ManagedRuntimeSpec,
    source: &Path,
    app_data_dir: &Path,
    trusted_manifest_sha256: &str,
) -> Result<PathBuf, String> {
    verify_runtime_directory_with_digest(spec, source, trusted_manifest_sha256)?;
    let destination = managed_runtime_dir_for(spec, app_data_dir);
    if let Ok(server) =
        verify_runtime_directory_with_digest(spec, &destination, trusted_manifest_sha256)
    {
        return Ok(server);
    }

    let parent = destination
        .parent()
        .ok_or_else(|| "Managed runtime destination has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create managed runtime directory {}: {error}",
            parent.display()
        )
    })?;
    let install_lock_path = parent.join(format!(".{}.install.lock", spec.version));
    let _install_lock = acquire_cross_process_lock(&install_lock_path)?;

    // Another process may have published the valid runtime while this one
    // waited. Recheck under the lock before allocating another staging tree.
    if let Ok(server) =
        verify_runtime_directory_with_digest(spec, &destination, trusted_manifest_sha256)
    {
        return Ok(server);
    }

    let staging = parent.join(format!(".{}-{}.tmp", spec.version, Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|error| {
        format!(
            "Failed to create managed runtime staging directory {}: {error}",
            staging.display()
        )
    })?;

    let publish = (|| -> Result<PathBuf, String> {
        let (manifest, manifest_bytes) = read_trusted_manifest(&source, trusted_manifest_sha256)?;
        for entry in &manifest.files {
            if !is_safe_flat_name(&entry.name) {
                return Err(format!("Unsafe managed runtime file name {}", entry.name));
            }
            let from = source.join(&entry.name);
            let to = staging.join(&entry.name);
            fs::copy(&from, &to).map_err(|error| {
                format!(
                    "Failed to copy managed runtime file {} to {}: {error}",
                    from.display(),
                    to.display()
                )
            })?;
            #[cfg(unix)]
            if entry.executable {
                let mut permissions = fs::metadata(&to)
                    .map_err(|error| format!("Failed to inspect {}: {error}", to.display()))?
                    .permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&to, permissions).map_err(|error| {
                    format!("Failed to make {} executable: {error}", to.display())
                })?;
            }
        }
        fs::write(staging.join(MANIFEST_FILE), manifest_bytes).map_err(|error| {
            format!(
                "Failed to write managed runtime manifest in {}: {error}",
                staging.display()
            )
        })?;
        verify_runtime_directory_with_digest(spec, &staging, trusted_manifest_sha256)?;

        // Staging is already authenticated. The version lock makes removal
        // of an invalid tree plus atomic directory publication single-writer,
        // without retaining a full quarantine copy.
        remove_invalid_runtime_destination(&destination)?;
        fs::rename(&staging, &destination).map_err(|error| {
            format!(
                "Failed to activate managed runtime {}: {error}",
                destination.display()
            )
        })?;
        verify_runtime_directory_with_digest(spec, &destination, trusted_manifest_sha256)
    })();

    if publish.is_err() && staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    publish
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_fixture(spec: &ManagedRuntimeSpec, label: &str) -> (PathBuf, String) {
        let directory = std::env::temp_dir().join(format!(
            "little-monkey-managed-runtime-{label}-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&directory).unwrap();
        let server_name = spec.executable();
        let server_bytes = b"verified-test-runtime";
        fs::write(directory.join(server_name), server_bytes).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "runtime": spec.manifest_runtime,
            "version": spec.version,
            "target": expected_runtime_target(spec).unwrap(),
            "sourceUrl": format!("{}download/{}/fixture", spec.source_url_prefix, spec.version),
            "archiveSha256": "a".repeat(64),
            "files": [{
                "name": server_name,
                "sha256": format!("{:x}", Sha256::digest(server_bytes)),
                "sizeBytes": server_bytes.len(),
                "executable": true
            }]
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        let trusted_manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
        fs::write(directory.join(MANIFEST_FILE), manifest_bytes).unwrap();
        (directory, trusted_manifest_sha256)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn concurrent_materialization_serializes_invalid_runtime_repair() {
        let (source, trusted_manifest_sha256) =
            runtime_fixture(&LLAMA, "concurrent-repair-source");
        let app_data = std::env::temp_dir().join(format!(
            "little-monkey-managed-runtime-concurrent-app-{}",
            Uuid::new_v4().simple()
        ));
        let destination = managed_runtime_dir(&app_data);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("corrupt-runtime"), b"invalid").unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let source = source.clone();
            let app_data = app_data.clone();
            let trusted_manifest_sha256 = trusted_manifest_sha256.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                materialize_runtime_from_source(
                    &LLAMA,
                    &source,
                    &app_data,
                    &trusted_manifest_sha256,
                )
            }));
        }
        barrier.wait();
        let installed = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(installed[0], installed[1]);
        assert!(verify_runtime_directory_with_digest(
            &LLAMA,
            &destination,
            &trusted_manifest_sha256
        )
        .is_ok());
        let runtime_parent = destination.parent().unwrap();
        for entry in fs::read_dir(runtime_parent).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"));
            assert!(!name.ends_with(".invalid"));
        }
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(app_data);
    }

    #[test]
    fn flat_runtime_names_reject_traversal_and_subdirectories() {
        assert!(is_safe_flat_name("llama-server"));
        assert!(is_safe_flat_name("ggml.dll"));
        assert!(!is_safe_flat_name("../llama-server"));
        assert!(!is_safe_flat_name("lib/ggml.so"));
        assert!(!is_safe_flat_name(""));
    }

    /// Speech rides the llama tree rather than a runtime of its own, so the
    /// staged manifest must actually carry `llama-tts` beside the server.
    #[test]
    fn the_llama_tree_carries_the_speech_binary() {
        let (directory, digest) = runtime_fixture(&LLAMA, "tts-sibling");
        let server =
            verify_runtime_directory_with_digest(&LLAMA, &directory, &digest).unwrap();
        assert_eq!(
            server.with_file_name(llama_tts_filename()).file_name(),
            Some(std::ffi::OsStr::new(llama_tts_filename()))
        );
        assert_ne!(llama_tts_filename(), llama_server_filename());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn current_release_platform_has_an_exact_runtime_target() {
        assert!(expected_runtime_target(&LLAMA).is_some());
    }

    /// stable-diffusion.cpp publishes fewer prebuilt targets than llama.cpp.
    /// A host outside that set must report "unsupported" rather than silently
    /// accepting a tree built for a different triple.
    #[test]
    fn stable_diffusion_targets_are_a_subset_of_llama_targets() {
        for target in STABLE_DIFFUSION.supported_targets {
            assert!(LLAMA.supported_targets.contains(target), "{target}");
        }
        assert!(runtime_supported_here(&LLAMA));
        assert_eq!(
            runtime_supported_here(&STABLE_DIFFUSION),
            STABLE_DIFFUSION
                .supported_targets
                .contains(&host_target().unwrap())
        );
    }

    /// The two runtimes must never share a directory, a staged resource name,
    /// or an install lock — publishing one cannot disturb the other.
    #[test]
    fn each_runtime_publishes_to_its_own_versioned_directory() {
        let root = Path::new("/tmp/little-monkey-test-data");
        assert_eq!(
            managed_runtime_dir_for(&LLAMA, root),
            root.join("runtimes").join("llama").join(MANAGED_LLAMA_VERSION)
        );
        assert_eq!(
            managed_runtime_dir_for(&STABLE_DIFFUSION, root),
            root.join("runtimes").join("sd").join(MANAGED_SD_VERSION)
        );
        assert_ne!(
            managed_runtime_dir_for(&LLAMA, root),
            managed_runtime_dir_for(&STABLE_DIFFUSION, root)
        );
        assert_ne!(LLAMA.override_env, STABLE_DIFFUSION.override_env);
        assert_ne!(LLAMA.executable(), STABLE_DIFFUSION.executable());
    }

    #[test]
    fn runtime_candidates_cover_tauri_resource_layouts() {
        let base = Path::new("/app/Contents");
        let candidates = runtime_candidates(&LLAMA, base);
        assert!(candidates[0].ends_with("managed-runtime/llama-b9637"));
        assert!(candidates[1].ends_with("resources/managed-runtime/llama-b9637"));
        assert!(candidates[2].ends_with("Resources/resources/managed-runtime/llama-b9637"));
        let sd = runtime_candidates(&STABLE_DIFFUSION, base);
        assert!(sd[0].ends_with(format!("managed-runtime/sd-{MANAGED_SD_VERSION}")));
    }

    #[test]
    fn verification_rejects_unmanifested_runtime_files() {
        let (directory, trusted_manifest_sha256) = runtime_fixture(&LLAMA, "unexpected-file");
        assert!(
            verify_runtime_directory_with_digest(&LLAMA, &directory, &trusted_manifest_sha256)
                .is_ok()
        );

        fs::write(directory.join("unexpected-backend.dll"), b"untrusted").unwrap();
        assert!(
            verify_runtime_directory_with_digest(&LLAMA, &directory, &trusted_manifest_sha256)
                .unwrap_err()
                .contains("unexpected entry")
        );
        let _ = fs::remove_dir_all(directory);
    }

    /// A tree staged for one runtime must not verify as the other, even when
    /// its own manifest digest is presented — otherwise a swapped resource
    /// directory could launch `sd-server` where `llama-server` was expected.
    #[test]
    fn a_runtime_tree_does_not_verify_under_another_runtimes_spec() {
        if !runtime_supported_here(&STABLE_DIFFUSION) {
            return;
        }
        let (directory, digest) = runtime_fixture(&STABLE_DIFFUSION, "cross-runtime");
        assert!(verify_runtime_directory_with_digest(&STABLE_DIFFUSION, &directory, &digest).is_ok());
        assert!(
            verify_runtime_directory_with_digest(&LLAMA, &directory, &digest)
                .unwrap_err()
                .contains("Unsupported managed runtime manifest")
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn verification_rejects_replacing_runtime_and_manifest_together() {
        let (directory, trusted_manifest_sha256) = runtime_fixture(&LLAMA, "replaced-tree");
        let server_name = llama_server_filename();
        let replacement_bytes = b"attacker-replaced-runtime";
        fs::write(directory.join(server_name), replacement_bytes).unwrap();
        let replacement_manifest = serde_json::json!({
            "schemaVersion": 1,
            "runtime": "llama.cpp",
            "version": MANAGED_LLAMA_VERSION,
            "target": expected_runtime_target(&LLAMA).unwrap(),
            "sourceUrl": format!(
                "https://github.com/ggml-org/llama.cpp/releases/download/{MANAGED_LLAMA_VERSION}/replacement"
            ),
            "archiveSha256": "b".repeat(64),
            "files": [{
                "name": server_name,
                "sha256": format!("{:x}", Sha256::digest(replacement_bytes)),
                "sizeBytes": replacement_bytes.len(),
                "executable": true
            }]
        });
        fs::write(
            directory.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(&replacement_manifest).unwrap(),
        )
        .unwrap();

        assert!(
            verify_runtime_directory_with_digest(&LLAMA, &directory, &trusted_manifest_sha256)
                .unwrap_err()
                .contains("manifest checksum mismatch")
        );
        let _ = fs::remove_dir_all(directory);
    }
}
