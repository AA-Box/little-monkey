use base64::Engine as _;
use little_monkey_lib::executable_extensions::{
    Approval, ExtensionManager, ExtensionManifest, InvocationRequest, PermissionGrant,
    PermissionKind,
};
use little_monkey_lib::package_ecosystem::{
    InstallSource, PackageSignature, RegistryPackageVersion, RegistrySnapshot,
};
use ring::signature::Ed25519KeyPair;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const LMX_SCHEMA_VERSION: u32 = 1;
const MAX_FILES: usize = 128;
const MAX_PATH_CHARS: usize = 512;
const MAX_FILE_BYTES: usize = 3 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 3 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const DEFAULT_TEST_INPUT: &str = "{}";

const EXTENSION_WIT: &str = r#"package little-monkey:extension@1.0.0;

interface host {
  record header { name: string, value: string, }
  record http-request {
    method: string,
    url: string,
    headers: list<header>,
    body: list<u8>,
    auth-slot: option<string>,
  }
  record http-response { status: u16, headers: list<header>, body: list<u8>, }
  log: func(level: string, message: string) -> result<_, string>;
  now-ms: func() -> u64;
  random-bytes: func(length: u32) -> result<list<u8>, string>;
  config-get: func(key: string) -> result<option<string>, string>;
  state-get: func(key: string) -> result<option<list<u8>>, string>;
  state-put: func(key: string, value: list<u8>) -> result<_, string>;
  send-http: func(request: http-request) -> result<http-response, string>;
  artifact-read: func(artifact-id: string) -> result<list<u8>, string>;
  artifact-write: func(bytes: list<u8>) -> result<string, string>;
  workspace-read: func(handle: string, relative-path: string) -> result<list<u8>, string>;
  workspace-write: func(handle: string, relative-path: string, bytes: list<u8>) -> result<_, string>;
  model-invoke: func(model-id: string, request-json: string) -> result<string, string>;
  device-request: func(device-id: string, capability: string, request-json: string) -> result<string, string>;
  emit-event: func(kind: string, payload-json: string) -> result<_, string>;
  set-tool-result: func(payload-json: string) -> result<_, string>;
  is-cancelled: func() -> bool;
  telemetry: func(name: string, value: u64) -> result<_, string>;
}

interface guest {
  run: func(capability-id: string, input-json: string) -> result<string, string>;
}

world extension {
  import host;
  export guest;
}
"#;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevTemplate {
    Tool,
    Channel,
    Connector,
    ModelProvider,
    EmbeddingProvider,
    Stt,
    Tts,
    RealtimeVoice,
    WebSearch,
    DeviceProvider,
}

impl DevTemplate {
    fn capability_kind(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Channel => "channel",
            Self::Connector => "connector",
            Self::ModelProvider => "model_provider",
            Self::EmbeddingProvider => "embedding_provider",
            Self::Stt => "stt",
            Self::Tts => "tts",
            Self::RealtimeVoice => "realtime_voice",
            Self::WebSearch => "web_search",
            Self::DeviceProvider => "device_provider",
        }
    }

    fn capability_id(self) -> &'static str {
        match self {
            Self::Tool => "echo",
            Self::Channel => "channel",
            Self::Connector => "connector",
            Self::ModelProvider => "model",
            Self::EmbeddingProvider => "embed",
            Self::Stt => "transcribe",
            Self::Tts => "synthesize",
            Self::RealtimeVoice => "voice",
            Self::WebSearch => "search",
            Self::DeviceProvider => "device",
        }
    }
}

#[derive(Debug, Serialize)]
struct BuildResult {
    source: String,
    bundle_dir: String,
    extension_id: String,
    version: String,
    component_sha256: String,
}

#[derive(Debug, Serialize)]
struct PackageResult {
    output: String,
    extension_id: String,
    version: String,
    package_sha256: String,
    manifest_sha256: String,
}

#[derive(Debug, Serialize)]
struct SignResult {
    output: String,
    extension_id: String,
    version: String,
    package_sha256: String,
    manifest_sha256: String,
    trust_root_id: String,
    key_id: String,
}

#[derive(Debug, Serialize)]
struct ConformanceCaseResult {
    name: String,
    capability_id: String,
    passed: bool,
    output: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConformanceReport {
    extension_id: String,
    version: String,
    compatible: bool,
    permissions: Vec<String>,
    cases: Vec<ConformanceCaseResult>,
    logs: Vec<little_monkey_lib::executable_extensions::ExtensionLogRow>,
    passed: usize,
    failed: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConformanceCase {
    name: String,
    capability_id: String,
    #[serde(default = "default_test_input")]
    input: Value,
    #[serde(default)]
    expected: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LmxEnvelope {
    schema_version: u32,
    manifest: Value,
    files_base64: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct PublishResult {
    registry_id: String,
    sequence: u64,
    snapshot: String,
    package: PackageResult,
    conformance_passed: usize,
}

fn default_test_input() -> Value {
    serde_json::json!({})
}

pub fn init(
    path: &Path,
    extension_id: Option<&str>,
    display_name: Option<&str>,
    template: DevTemplate,
    publisher: &str,
    json: bool,
) -> Result<(), String> {
    if path.exists() {
        let mut entries = fs::read_dir(path)
            .map_err(|error| format!("Cannot inspect '{}': {error}", path.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "Refusing to initialize non-empty directory '{}'",
                path.display()
            ));
        }
    } else {
        fs::create_dir_all(path)
            .map_err(|error| format!("Cannot create '{}': {error}", path.display()))?;
    }
    let root = path
        .canonicalize()
        .map_err(|error| format!("Cannot resolve '{}': {error}", path.display()))?;
    let slug = slugify(
        root.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("extension"),
    );
    let extension_id = extension_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("dev.local.{slug}"));
    let display_name = display_name
        .map(str::to_string)
        .unwrap_or_else(|| title_case(&slug));
    let crate_name = slug.replace('.', "-");
    let capability_id = template.capability_id();

    fs::create_dir_all(root.join("src"))
        .and_then(|_| fs::create_dir_all(root.join("wit")))
        .map_err(|error| format!("Cannot create extension project directories: {error}"))?;

    let cargo = format!(
        "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\nserde = {{ version = \"1\", features = [\"derive\"] }}\nserde_json = \"1\"\nwit-bindgen = \"=0.60.0\"\n"
    );
    fs::write(root.join("Cargo.toml"), cargo)
        .map_err(|error| format!("Cannot write Cargo.toml: {error}"))?;
    fs::write(root.join("wit/little-monkey-extension.wit"), EXTENSION_WIT)
        .map_err(|error| format!("Cannot write WIT contract: {error}"))?;

    let source = format!(
        r#"mod bindings {{
    wit_bindgen::generate!({{
        path: "wit",
        world: "extension",
    }});
}}

use bindings::exports::little_monkey::extension::guest::Guest;
use bindings::little_monkey::extension::host;
use serde::{{Deserialize, Serialize}};

const CAPABILITY_ID: &str = {capability_id:?};

#[derive(Deserialize)]
struct Input {{
    #[serde(default)]
    text: String,
}}

#[derive(Serialize)]
struct Output {{
    echoed: String,
}}

struct Extension;

impl Guest for Extension {{
    fn run(capability_id: String, input_json: String) -> Result<String, String> {{
        if capability_id != CAPABILITY_ID {{
            return Err(format!("unsupported capability: {{capability_id}}"));
        }}
        if host::is_cancelled() {{
            return Err("cancelled".to_string());
        }}
        let input: Input = serde_json::from_str(&input_json)
            .map_err(|error| format!("invalid input: {{error}}"))?;
        host::log("info", "development extension invocation")?;
        let output = serde_json::to_string(&Output {{ echoed: input.text }})
            .map_err(|error| error.to_string())?;
        host::set_tool_result(&output)?;
        Ok(output)
    }}
}}

bindings::export!(Extension with_types_in bindings);
"#
    );
    fs::write(root.join("src/lib.rs"), source)
        .map_err(|error| format!("Cannot write src/lib.rs: {error}"))?;

    let zero_digest = "0".repeat(64);
    let manifest = serde_json::json!({
        "schema_version": 1,
        "extension_id": extension_id,
        "version": "0.1.0",
        "display_name": display_name,
        "description": format!("Little Monkey {} extension", template.capability_kind()),
        "host_api": { "minimum": "1.0.0", "maximum_exclusive": "2.0.0" },
        "component": { "path": "component.wasm", "sha256": zero_digest },
        "capabilities": [{
            "capability_id": capability_id,
            "kind": template.capability_kind(),
            "display_name": display_name,
            "description": "Development capability generated by `monkey extensions init`.",
            "input_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "text": { "type": "string" } }
            }
        }],
        "permissions": [],
        "config_schema": [],
        "secret_slots": [],
        "dependencies": [],
        "compatibility": {
            "minimum_app_version": "1.5.0",
            "maximum_app_version_exclusive": null,
            "platforms": ["linux", "macos", "windows"],
            "architectures": ["aarch64", "x86_64"]
        },
        "publisher": publisher,
        "provenance": {
            "publisher": publisher,
            "source": { "local_folder": { "canonical_path": root.to_string_lossy() } },
            "source_revision": "local",
            "build_reproducible": true
        },
        "signature": null,
        "checksums": { "component.wasm": "0".repeat(64) }
    });
    write_json(root.join("extension.json"), &manifest)?;
    write_json(
        root.join("extension.tests.json"),
        &serde_json::json!([{
            "name": "generated smoke test",
            "capability_id": capability_id,
            "input": { "text": "hello" },
            "expected": { "echoed": "hello" }
        }]),
    )?;
    fs::write(root.join(".gitignore"), "target/\n.little-monkey/\n*.lmx\n")
        .map_err(|error| format!("Cannot write .gitignore: {error}"))?;
    fs::write(
        root.join("README.md"),
        format!(
            "# {display_name}\n\nGenerated by `monkey extensions init`.\n\n```sh\nmonkey extensions dev .\nmonkey extensions test .\nmonkey extensions pack .\n```\n"
        ),
    )
    .map_err(|error| format!("Cannot write README.md: {error}"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": root,
                "extension_id": manifest["extension_id"],
                "template": template.capability_kind(),
                "next": ["monkey extensions dev .", "monkey extensions test .", "monkey extensions pack ."]
            }))
            .map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "Initialized {} at {}",
            manifest["extension_id"],
            root.display()
        );
        println!("Next: monkey extensions dev .");
    }
    Ok(())
}

pub async fn validate(manager: &ExtensionManager, target: &str, json: bool) -> Result<(), String> {
    let path = Path::new(target);
    if path.exists() {
        let source = if path.is_dir() && path.join("Cargo.toml").is_file() {
            build_bundle(path)?.1
        } else {
            path.canonicalize()
                .map_err(|error| format!("Cannot resolve '{}': {error}", path.display()))?
        };
        let preview = manager.discover(&source)?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&preview).map_err(|error| error.to_string())?
            );
        } else {
            println!(
                "{} {}",
                preview.manifest.display_name, preview.manifest.version
            );
            println!("compatible: {}", preview.compatible);
            println!("trust: {:?}", preview.trust.state);
            for permission in &preview.permissions {
                println!(
                    "permission: {:?} {} — {}",
                    permission.kind, permission.scope, permission.reason
                );
            }
            for blocker in &preview.blockers {
                println!("blocker: {blocker}");
            }
        }
        if !preview.compatible || !preview.blockers.is_empty() {
            return Err("Extension validation failed".to_string());
        }
        return Ok(());
    }
    let detail = manager.validate_installed(target).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&detail).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "{} {} — {:?}",
            detail.manifest.display_name, detail.active_version, detail.health.state
        );
    }
    Ok(())
}

pub fn pack(source: &Path, output: Option<&Path>, json: bool) -> Result<(), String> {
    let (_, bundle) = build_bundle(source)?;
    let manifest = read_manifest(&bundle)?;
    let output = output.map(Path::to_path_buf).unwrap_or_else(|| {
        source.join("dist").join(format!(
            "{}-{}.lmx",
            manifest.extension_id, manifest.version
        ))
    });
    let result = pack_bundle(&bundle, &output)?;
    print_value(&result, json)
}

pub fn sign(
    package: &Path,
    private_key: &Path,
    trust_root_id: &str,
    key_id: &str,
    output: Option<&Path>,
    json: bool,
) -> Result<(), String> {
    let output = output.unwrap_or(package);
    let result = sign_package(package, output, private_key, trust_root_id, key_id)?;
    print_value(&result, json)
}

pub async fn test_extension(source: &Path, json: bool) -> Result<(), String> {
    let report = conformance_report(source).await?;
    print_value(&report, json)?;
    if report.failed > 0 {
        return Err(format!("{} conformance case(s) failed", report.failed));
    }
    Ok(())
}

pub async fn dev(
    source: &Path,
    capability: Option<&str>,
    input: &str,
    once: bool,
) -> Result<(), String> {
    let root = source
        .canonicalize()
        .map_err(|error| format!("Cannot resolve '{}': {error}", source.display()))?;
    let profile = root.join(".little-monkey/dev-profile");
    fs::create_dir_all(&profile)
        .map_err(|error| format!("Cannot create development profile: {error}"))?;
    let input_json = read_json_arg(input)?;
    serde_json::from_str::<Value>(&input_json)
        .map_err(|error| format!("Development invocation input must be JSON: {error}"))?;
    let mut observed = project_fingerprint(&root)?;

    loop {
        let (_, bundle) = build_bundle(&root)?;
        let manager = ExtensionManager::new(&profile)?;
        let preview = manager.discover(&bundle)?;
        let extension_id = preview.manifest.extension_id.clone();
        if manager.inspect(&extension_id).is_ok() {
            let _ = manager.set_running(&extension_id, false).await;
            let _ = manager.uninstall(&extension_id);
        }
        let grants = development_grants(&preview.permissions, &root)?;
        let detail = manager
            .install(
                &bundle,
                Approval {
                    approval_digest: preview.approval_digest.clone(),
                    grants,
                    allow_unsigned: true,
                    allow_untrusted: true,
                    allow_high_risk: true,
                },
            )
            .await?;
        manager.set_enabled(&extension_id, true).await?;
        manager.set_running(&extension_id, true).await?;

        println!(
            "[DEVELOPMENT MODE] {} {}",
            detail.manifest.display_name, detail.active_version
        );
        println!("Isolated profile: {}", profile.display());
        if preview.permissions.is_empty() {
            println!("Requested permissions: none");
        } else {
            println!("Requested permissions:");
            for permission in &preview.permissions {
                println!(
                    "  {:?} {} — {}",
                    permission.kind, permission.scope, permission.reason
                );
            }
        }
        if let Some(capability_id) = capability {
            let response = manager
                .invoke(InvocationRequest {
                    extension_id: extension_id.clone(),
                    capability_id: capability_id.to_string(),
                    input_json: input_json.clone(),
                    invocation_id: None,
                    input_artifact_ids: Vec::new(),
                    expected_kind: None,
                    expected_version: None,
                })
                .await?;
            println!("Invocation output: {}", response.output_json);
        }
        for row in manager.logs(&extension_id, 100)? {
            println!("[guest:{}] {}", row.level, row.message);
        }
        if once {
            let _ = manager.set_running(&extension_id, false).await;
            return Ok(());
        }

        println!("Watching for changes. Ctrl-C stops development mode.");
        loop {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| format!("Cannot listen for Ctrl-C: {error}"))?;
                    let _ = manager.set_running(&extension_id, false).await;
                    let _ = manager.uninstall(&extension_id);
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_millis(750)) => {
                    let next = project_fingerprint(&root)?;
                    if next != observed {
                        observed = next;
                        let _ = manager.set_running(&extension_id, false).await;
                        let _ = manager.uninstall(&extension_id);
                        println!("[DEVELOPMENT MODE] change detected; rebuilding…");
                        break;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn publish(
    source: &Path,
    snapshot_path: &Path,
    registry_root: &Path,
    publisher_private_key: &Path,
    trust_root_id: &str,
    key_id: &str,
    registry_private_key: &Path,
    output: Option<&Path>,
    refresh_hours: u64,
    expiry_days: u64,
    json: bool,
) -> Result<(), String> {
    if refresh_hours == 0 || expiry_days == 0 || refresh_hours >= expiry_days.saturating_mul(24) {
        return Err("Registry refresh window must be positive and shorter than expiry".to_string());
    }
    let report = conformance_report(source).await?;
    if report.failed > 0 {
        return Err(format!(
            "Publishing refused: {} conformance case(s) failed",
            report.failed
        ));
    }

    let (_, bundle) = build_bundle(source)?;
    let mut snapshot: RegistrySnapshot =
        serde_json::from_str(&fs::read_to_string(snapshot_path).map_err(|error| {
            format!(
                "Cannot read registry snapshot '{}': {error}",
                snapshot_path.display()
            )
        })?)
        .map_err(|error| format!("Invalid registry snapshot: {error}"))?;
    if snapshot.signature.algorithm != "ed25519" {
        return Err("Registry snapshot must use ed25519".to_string());
    }

    let mut manifest = read_manifest(&bundle)?;
    manifest.provenance.source = InstallSource::CuratedRegistry {
        registry_id: snapshot.registry_id.clone(),
    };
    manifest.provenance.source_revision =
        git_revision(source).unwrap_or_else(|| manifest.version.to_string());
    manifest.provenance.build_reproducible = true;
    manifest.signature = None;
    manifest.validate()?;
    write_json(bundle.join("extension.json"), &manifest)?;

    let unsigned_output = output.map(Path::to_path_buf).unwrap_or_else(|| {
        source.join("dist").join(format!(
            "{}-{}.lmx",
            manifest.extension_id, manifest.version
        ))
    });
    pack_bundle(&bundle, &unsigned_output)?;
    let signed = sign_package(
        &unsigned_output,
        &unsigned_output,
        publisher_private_key,
        trust_root_id,
        key_id,
    )?;

    let package_id = format!("extension.{}", manifest.extension_id);
    let versions = snapshot.packages.entry(package_id).or_default();
    versions.retain(|entry| entry.version != manifest.version);
    versions.push(RegistryPackageVersion {
        version: manifest.version,
        bundle_sha256: signed.package_sha256.clone(),
        manifest_sha256: signed.manifest_sha256.clone(),
    });
    versions.sort_by_key(|entry| entry.version);

    let now = now_ms()?;
    snapshot.sequence = snapshot
        .sequence
        .checked_add(1)
        .ok_or_else(|| "Registry sequence exhausted".to_string())?;
    snapshot.generated_unix_ms = now;
    snapshot.refresh_after_unix_ms = now
        .checked_add(refresh_hours.saturating_mul(60 * 60 * 1000))
        .ok_or_else(|| "Registry refresh timestamp overflow".to_string())?;
    snapshot.expires_unix_ms = now
        .checked_add(expiry_days.saturating_mul(24 * 60 * 60 * 1000))
        .ok_or_else(|| "Registry expiry timestamp overflow".to_string())?;
    snapshot.signature.signature_hex.clear();
    let registry_signature = sign_bytes(
        registry_private_key,
        &snapshot
            .signing_payload()
            .map_err(|error| error.to_string())?,
    )?;
    snapshot.signature.signature_hex = registry_signature;

    let artifact_dir = registry_root
        .join("extensions")
        .join(&manifest.extension_id);
    fs::create_dir_all(&artifact_dir)
        .map_err(|error| format!("Cannot create registry artifact directory: {error}"))?;
    let destination = artifact_dir.join(format!("{}.lmx", manifest.version));
    fs::copy(&unsigned_output, &destination)
        .map_err(|error| format!("Cannot publish .lmx artifact: {error}"))?;
    write_json(snapshot_path.to_path_buf(), &snapshot)?;

    print_value(
        &PublishResult {
            registry_id: snapshot.registry_id,
            sequence: snapshot.sequence,
            snapshot: snapshot_path.to_string_lossy().to_string(),
            package: PackageResult {
                output: destination.to_string_lossy().to_string(),
                extension_id: signed.extension_id,
                version: signed.version,
                package_sha256: signed.package_sha256,
                manifest_sha256: signed.manifest_sha256,
            },
            conformance_passed: report.passed,
        },
        json,
    )
}

fn build_bundle(source: &Path) -> Result<(BuildResult, PathBuf), String> {
    let root = source
        .canonicalize()
        .map_err(|error| format!("Cannot resolve '{}': {error}", source.display()))?;
    if !root.join("Cargo.toml").is_file() || !root.join("extension.json").is_file() {
        return Err("Extension project needs Cargo.toml and extension.json".to_string());
    }
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&root)
        .status()
        .map_err(|error| format!("Could not run cargo: {error}"))?;
    if !status.success() {
        return Err("Extension build failed. Ensure Rust has the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`).".to_string());
    }
    let crate_name = cargo_package_name(&root.join("Cargo.toml"))?;
    let artifact = root
        .join("target/wasm32-wasip2/release")
        .join(format!("{}.wasm", crate_name.replace('-', "_")));
    let component = fs::read(&artifact).map_err(|error| {
        format!(
            "Built component '{}' is missing: {error}",
            artifact.display()
        )
    })?;
    let component_digest = sha256_hex(&component);
    let mut manifest: ExtensionManifest = serde_json::from_str(
        &fs::read_to_string(root.join("extension.json"))
            .map_err(|error| format!("Cannot read extension.json: {error}"))?,
    )
    .map_err(|error| format!("Invalid extension.json: {error}"))?;
    manifest.component.path = "component.wasm".to_string();
    manifest.component.sha256 = component_digest.clone();
    manifest.checksums.clear();
    manifest
        .checksums
        .insert("component.wasm".to_string(), component_digest.clone());
    manifest.signature = None;
    manifest.provenance.source_revision =
        git_revision(&root).unwrap_or_else(|| "local".to_string());
    manifest.provenance.build_reproducible = true;

    let bundle = root
        .join(".little-monkey/dist")
        .join(&manifest.extension_id)
        .join(manifest.version.to_string());
    if bundle.exists() {
        fs::remove_dir_all(&bundle)
            .map_err(|error| format!("Cannot clear previous build: {error}"))?;
    }
    fs::create_dir_all(&bundle)
        .map_err(|error| format!("Cannot create build directory: {error}"))?;
    manifest.provenance.source = InstallSource::LocalFolder {
        canonical_path: bundle
            .canonicalize()
            .map_err(|error| format!("Cannot resolve build directory: {error}"))?
            .to_string_lossy()
            .to_string(),
    };
    manifest.validate()?;
    fs::write(bundle.join("component.wasm"), &component)
        .map_err(|error| format!("Cannot write component.wasm: {error}"))?;
    write_json(bundle.join("extension.json"), &manifest)?;
    Ok((
        BuildResult {
            source: root.to_string_lossy().to_string(),
            bundle_dir: bundle.to_string_lossy().to_string(),
            extension_id: manifest.extension_id,
            version: manifest.version.to_string(),
            component_sha256: component_digest,
        },
        bundle,
    ))
}

async fn conformance_report(source: &Path) -> Result<ConformanceReport, String> {
    let root = source
        .canonicalize()
        .map_err(|error| format!("Cannot resolve '{}': {error}", source.display()))?;
    let (_, bundle) = build_bundle(&root)?;
    let profile = root.join(".little-monkey/test-profile");
    if profile.exists() {
        fs::remove_dir_all(&profile)
            .map_err(|error| format!("Cannot reset test profile: {error}"))?;
    }
    fs::create_dir_all(&profile).map_err(|error| format!("Cannot create test profile: {error}"))?;
    let manager = ExtensionManager::new(&profile)?;
    let preview = manager.discover(&bundle)?;
    if !preview.compatible || !preview.blockers.is_empty() {
        return Err(format!(
            "Extension is not installable: {}",
            preview.blockers.join("; ")
        ));
    }
    let grants = development_grants(&preview.permissions, &root)?;
    let extension_id = preview.manifest.extension_id.clone();
    manager
        .install(
            &bundle,
            Approval {
                approval_digest: preview.approval_digest,
                grants,
                allow_unsigned: true,
                allow_untrusted: true,
                allow_high_risk: true,
            },
        )
        .await?;
    manager.set_enabled(&extension_id, true).await?;
    manager.set_running(&extension_id, true).await?;

    let cases = load_conformance_cases(&root, &preview.manifest)?;
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let input_json = serde_json::to_string(&case.input).map_err(|error| error.to_string())?;
        match manager
            .invoke(InvocationRequest {
                extension_id: extension_id.clone(),
                capability_id: case.capability_id.clone(),
                input_json,
                invocation_id: None,
                input_artifact_ids: Vec::new(),
                expected_kind: None,
                expected_version: None,
            })
            .await
        {
            Ok(response) => {
                let output = serde_json::from_str::<Value>(&response.output_json)
                    .unwrap_or_else(|_| Value::String(response.output_json));
                let passed = case
                    .expected
                    .as_ref()
                    .is_none_or(|expected| expected == &output);
                results.push(ConformanceCaseResult {
                    name: case.name,
                    capability_id: case.capability_id,
                    passed,
                    output: Some(output),
                    error: if passed {
                        None
                    } else {
                        Some("output did not match expected JSON".to_string())
                    },
                });
            }
            Err(error) => results.push(ConformanceCaseResult {
                name: case.name,
                capability_id: case.capability_id,
                passed: false,
                output: None,
                error: Some(error),
            }),
        }
    }
    let logs = manager.logs(&extension_id, 200)?;
    let _ = manager.set_running(&extension_id, false).await;
    let _ = manager.uninstall(&extension_id);
    let _ = fs::remove_dir_all(&profile);
    let passed = results.iter().filter(|case| case.passed).count();
    let failed = results.len().saturating_sub(passed);
    Ok(ConformanceReport {
        extension_id,
        version: preview.manifest.version.to_string(),
        compatible: preview.compatible,
        permissions: preview
            .permissions
            .iter()
            .map(|permission| format!("{:?} {}", permission.kind, permission.scope))
            .collect(),
        cases: results,
        logs,
        passed,
        failed,
    })
}

fn load_conformance_cases(
    root: &Path,
    manifest: &ExtensionManifest,
) -> Result<Vec<ConformanceCase>, String> {
    let path = root.join("extension.tests.json");
    if path.is_file() {
        let cases: Vec<ConformanceCase> = serde_json::from_str(
            &fs::read_to_string(&path)
                .map_err(|error| format!("Cannot read '{}': {error}", path.display()))?,
        )
        .map_err(|error| format!("Invalid extension.tests.json: {error}"))?;
        if cases.is_empty() {
            return Err("extension.tests.json must contain at least one case".to_string());
        }
        return Ok(cases);
    }
    Ok(manifest
        .capabilities
        .iter()
        .map(|capability| ConformanceCase {
            name: format!("{} smoke", capability.capability_id),
            capability_id: capability.capability_id.clone(),
            input: serde_json::from_str(DEFAULT_TEST_INPUT).expect("static JSON"),
            expected: None,
        })
        .collect())
}

fn development_grants(
    permissions: &[little_monkey_lib::executable_extensions::PermissionView],
    workspace: &Path,
) -> Result<Vec<PermissionGrant>, String> {
    let canonical = workspace
        .canonicalize()
        .map_err(|error| format!("Cannot resolve development workspace: {error}"))?
        .to_string_lossy()
        .to_string();
    Ok(permissions
        .iter()
        .map(|permission| PermissionGrant {
            permission_id: permission.permission_id.clone(),
            binding: matches!(
                permission.kind,
                PermissionKind::WorkspaceRead | PermissionKind::WorkspaceWrite
            )
            .then(|| canonical.clone()),
        })
        .collect())
}

fn pack_bundle(bundle: &Path, output: &Path) -> Result<PackageResult, String> {
    let manifest = read_manifest(bundle)?;
    let manifest_value = serde_json::to_value(&manifest).map_err(|error| error.to_string())?;
    let manifest_text = canonical_json(&manifest_value)?;
    if manifest_text.len() > MAX_MANIFEST_BYTES {
        return Err("extension.json exceeds the .lmx manifest limit".to_string());
    }
    let mut files_base64 = BTreeMap::new();
    let mut collisions = BTreeSet::new();
    let mut total = 0usize;
    for entry in WalkDir::new(bundle).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|error| format!("Cannot inspect bundle: {error}"))?;
        if entry.path() == bundle {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("Cannot inspect '{}': {error}", entry.path().display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Symlinks are not permitted in .lmx packages: {}",
                entry.path().display()
            ));
        }
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            return Err(format!(
                "Unsupported package entry: {}",
                entry.path().display()
            ));
        }
        let relative = entry
            .path()
            .strip_prefix(bundle)
            .map_err(|_| "Bundle traversal escaped source root".to_string())?;
        let relative = safe_relative(relative)?;
        if relative == "extension.json" {
            continue;
        }
        let collision = relative.to_lowercase();
        if !collisions.insert(collision) {
            return Err(format!("Duplicate/colliding .lmx path: {relative}"));
        }
        if files_base64.len() >= MAX_FILES {
            return Err(format!(".lmx package exceeds {MAX_FILES} files"));
        }
        let bytes = fs::read(entry.path())
            .map_err(|error| format!("Cannot read '{}': {error}", entry.path().display()))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!("{relative} exceeds the per-file .lmx limit"));
        }
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| ".lmx size overflow".to_string())?;
        if total > MAX_TOTAL_BYTES {
            return Err(".lmx decoded payload exceeds its total limit".to_string());
        }
        files_base64.insert(
            relative,
            base64::engine::general_purpose::STANDARD.encode(bytes),
        );
    }
    if files_base64.is_empty() || !files_base64.contains_key(&manifest.component.path) {
        return Err(".lmx package is missing the declared component".to_string());
    }
    let envelope = LmxEnvelope {
        schema_version: LMX_SCHEMA_VERSION,
        manifest: manifest_value,
        files_base64,
    };
    let value = serde_json::to_value(&envelope).map_err(|error| error.to_string())?;
    let text = canonical_json(&value)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create package output directory: {error}"))?;
    }
    fs::write(output, &text)
        .map_err(|error| format!("Cannot write '{}': {error}", output.display()))?;
    Ok(PackageResult {
        output: output.to_string_lossy().to_string(),
        extension_id: manifest.extension_id,
        version: manifest.version.to_string(),
        package_sha256: sha256_hex(text.as_bytes()),
        manifest_sha256: sha256_hex(manifest_text.as_bytes()),
    })
}

fn sign_package(
    input: &Path,
    output: &Path,
    private_key: &Path,
    trust_root_id: &str,
    key_id: &str,
) -> Result<SignResult, String> {
    let text = fs::read_to_string(input)
        .map_err(|error| format!("Cannot read package '{}': {error}", input.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("Invalid .lmx JSON: {error}"))?;
    if canonical_json(&value)? != text {
        return Err(
            ".lmx is not deterministically encoded; run `monkey extensions pack` first".to_string(),
        );
    }
    let mut envelope: LmxEnvelope =
        serde_json::from_value(value).map_err(|error| format!("Invalid .lmx envelope: {error}"))?;
    if envelope.schema_version != LMX_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported .lmx schema {}",
            envelope.schema_version
        ));
    }
    let mut manifest: ExtensionManifest = serde_json::from_value(envelope.manifest.clone())
        .map_err(|error| format!("Invalid extension manifest: {error}"))?;
    manifest.signature = None;
    manifest.validate()?;
    let signature_hex = sign_bytes(private_key, &manifest.signing_payload()?)?;
    manifest.signature = Some(PackageSignature {
        trust_root_id: trust_root_id.to_string(),
        key_id: key_id.to_string(),
        algorithm: "ed25519".to_string(),
        signature_hex,
    });
    manifest.validate()?;
    envelope.manifest = serde_json::to_value(&manifest).map_err(|error| error.to_string())?;
    let value = serde_json::to_value(&envelope).map_err(|error| error.to_string())?;
    let signed_text = canonical_json(&value)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create signing output directory: {error}"))?;
    }
    fs::write(output, &signed_text).map_err(|error| {
        format!(
            "Cannot write signed package '{}': {error}",
            output.display()
        )
    })?;
    let manifest_text =
        canonical_json(&serde_json::to_value(&manifest).map_err(|error| error.to_string())?)?;
    Ok(SignResult {
        output: output.to_string_lossy().to_string(),
        extension_id: manifest.extension_id,
        version: manifest.version.to_string(),
        package_sha256: sha256_hex(signed_text.as_bytes()),
        manifest_sha256: sha256_hex(manifest_text.as_bytes()),
        trust_root_id: trust_root_id.to_string(),
        key_id: key_id.to_string(),
    })
}

fn sign_bytes(private_key: &Path, payload: &[u8]) -> Result<String, String> {
    let text = fs::read_to_string(private_key).map_err(|error| {
        format!(
            "Cannot read signing key '{}': {error}",
            private_key.display()
        )
    })?;
    let encoded = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .map(str::trim)
        .collect::<String>();
    let der = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("Signing key is not valid PEM/base64 PKCS#8: {error}"))?;
    let key_pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der)
        .map_err(|_| "Signing key must be an Ed25519 PKCS#8 private key".to_string())?;
    Ok(hex(key_pair.sign(payload).as_ref()))
}

fn read_manifest(bundle: &Path) -> Result<ExtensionManifest, String> {
    let manifest: ExtensionManifest = serde_json::from_str(
        &fs::read_to_string(bundle.join("extension.json"))
            .map_err(|error| format!("Cannot read extension.json: {error}"))?,
    )
    .map_err(|error| format!("Invalid extension.json: {error}"))?;
    manifest.validate()?;
    Ok(manifest)
}

fn cargo_package_name(path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Cannot read '{}': {error}", path.display()))?;
    let mut in_package = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line == "[package]" {
            in_package = true;
            continue;
        }
        if in_package && line.starts_with('[') {
            break;
        }
        if in_package && line.starts_with("name") {
            let (_, value) = line
                .split_once('=')
                .ok_or_else(|| "Invalid Cargo package name".to_string())?;
            let name = value.trim().trim_matches('"');
            if !name.is_empty() {
                return Ok(name.to_string());
            }
        }
    }
    Err("Cargo.toml [package] needs a name".to_string())
}

fn read_json_arg(value: &str) -> Result<String, String> {
    if let Some(path) = value.strip_prefix('@') {
        fs::read_to_string(path).map_err(|error| format!("Cannot read '{path}': {error}"))
    } else {
        Ok(value.to_string())
    }
}

fn safe_relative(path: &Path) -> Result<String, String> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::CurDir
                    | Component::RootDir
                    | Component::Prefix(_)
            )
        })
    {
        return Err(format!("Unsafe .lmx path: {}", path.display()));
    }
    let value = path.to_string_lossy().replace('\\', "/");
    if value.is_empty()
        || value.len() > MAX_PATH_CHARS
        || !value.is_ascii()
        || value.starts_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("Unsafe .lmx path: {value}"));
    }
    Ok(value)
}

fn canonical_json(value: &Value) -> Result<String, String> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_string(value).map_err(|error| error.to_string())
        }
        Value::Array(items) => Ok(format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        )),
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            let mut output = Vec::with_capacity(keys.len());
            for key in keys {
                output.push(format!(
                    "{}:{}",
                    serde_json::to_string(key).map_err(|error| error.to_string())?,
                    canonical_json(&object[key])?
                ));
            }
            Ok(format!("{{{}}}", output.join(",")))
        }
    }
}

fn write_json(path: PathBuf, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create '{}': {error}", parent.display()))?;
    }
    fs::write(
        &path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
        ),
    )
    .map_err(|error| format!("Cannot write '{}': {error}", path.display()))
}

fn print_value(value: &impl Serialize, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
        );
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock is before Unix epoch: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "System clock exceeds timestamp range".to_string())
}

fn git_revision(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if matches!(ch, '-' | '_' | ' ' | '.') && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-')
        .to_string()
        .chars()
        .take(48)
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn title_case(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn project_fingerprint(root: &Path) -> Result<String, String> {
    let mut rows = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|error| format!("Cannot watch project: {error}"))?;
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if relative.components().next().is_some_and(|component| {
            let value = component.as_os_str().to_string_lossy();
            value == "target" || value == ".little-monkey" || value == ".git"
        }) {
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("Cannot stat watched file: {error}"))?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        rows.push(format!(
            "{}:{}:{}",
            relative.display(),
            metadata.len(),
            modified
        ));
    }
    Ok(sha256_hex(rows.join("\n").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_orders_nested_objects() {
        let value = serde_json::json!({"z": 1, "a": {"y": 2, "b": 3}});
        assert_eq!(
            canonical_json(&value).unwrap(),
            r#"{"a":{"b":3,"y":2},"z":1}"#
        );
    }

    #[test]
    fn unsafe_package_paths_are_rejected() {
        assert!(safe_relative(Path::new("../escape")).is_err());
        assert!(safe_relative(Path::new("/absolute")).is_err());
        assert!(safe_relative(Path::new("component.wasm")).is_ok());
    }

    #[test]
    fn every_template_has_a_stable_runtime_kind() {
        let templates = [
            DevTemplate::Tool,
            DevTemplate::Channel,
            DevTemplate::Connector,
            DevTemplate::ModelProvider,
            DevTemplate::EmbeddingProvider,
            DevTemplate::Stt,
            DevTemplate::Tts,
            DevTemplate::RealtimeVoice,
            DevTemplate::WebSearch,
            DevTemplate::DeviceProvider,
        ];
        let kinds = templates
            .iter()
            .map(|template| template.capability_kind())
            .collect::<BTreeSet<_>>();
        assert_eq!(kinds.len(), templates.len());
    }
}
