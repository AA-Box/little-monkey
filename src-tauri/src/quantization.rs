//! Model Conversion and Quantization Workbench (ROADMAP.md Phase 8).
//!
//! This module is deliberately Tauri-free and depends only on the local
//! filesystem, `sha2`, and (for the real backend) an external `llama-quantize`
//! process — no network access, no app state. Thin `#[tauri::command]` glue
//! lives in `m3_commands.rs`.
//!
//! ## Honesty about what is real here
//!
//! Full Hugging Face/safetensors -> GGUF *conversion* (what upstream
//! `llama.cpp`'s `convert_hf_to_gguf.py` does) needs a Python environment
//! with `transformers`/`torch` that is not a dependency of this app and is
//! very unlikely to exist wherever this ships. This module does **not**
//! attempt that conversion and never fabricates a result for it — safetensors
//! inputs are genuinely detected and validated (real header sniffing, see
//! [`sniff_safetensors_header`]), but [`QuantizationWorkbench::convert`]
//! always returns [`QuantizationError::Unsupported`] for them, with a message
//! pointing the user at converting to GGUF externally first.
//!
//! What *is* real and fully exercised end to end:
//!
//! - GGUF header sniffing ([`sniff_gguf_header`]): a real, bounded parser for
//!   the GGUF binary format (magic, version, tensor/metadata counts, and
//!   selected string metadata such as `general.architecture` and
//!   `general.license`), used for both input validation and output
//!   verification (the report's [`EvalResult`]).
//! - Safetensors header sniffing ([`sniff_safetensors_header`]): parses the
//!   `u64` header length + JSON tensor/metadata dictionary safetensors files
//!   always start with.
//! - License surfacing and heuristic risk classification
//!   ([`assess_license`]) — reuses `M3ModelLicense` (see
//!   `m3_runtime_hub.rs`) when the source is an already-installed, verified
//!   Runtime Hub model, and falls back to whatever license string can be
//!   sniffed directly out of the file/directory otherwise.
//! - [`GgufQuantType`]: a static, descriptive reference table of well-known
//!   `llama.cpp` GGUF quantization types and their approximate size/quality
//!   tradeoffs (bits-per-weight class and a plain-language note). These are
//!   widely cited, descriptive figures — not benchmark numbers measured by
//!   this app — and are labelled as such in the report.
//! - [`LlamaCppQuantizeBackend`]: shells out to the real `llama-quantize`
//!   binary (part of `llama.cpp`) when it is genuinely found on `PATH` or in
//!   common install locations, for real GGUF -> GGUF (re)quantization. This
//!   is the "real backend" the ROADMAP acceptance criteria ask for, when the
//!   host machine has `llama.cpp` installed (e.g. via `brew install
//!   llama.cpp`).
//! - [`PassthroughGgufRequantize`]: an honest no-op fallback used only for the
//!   `Copy` pseudo quant level (mirroring `llama-quantize`'s own `COPY` type)
//!   when no real quantizer is available. It only ever copies bytes
//!   losslessly and verifies the digest — it never claims to have quantized
//!   anything, and [`QuantizationWorkbench::convert`] refuses to pair it with
//!   any quant level other than [`GgufQuantType::Copy`].
//! - Digests reuse `m3_runtime_hub::sha256_file` rather than a second
//!   SHA-256 implementation.
//!
//! Every conversion produces a reproducible [`ConversionReport`] (inputs:
//! source digest/format/declared license; the tool actually used and its
//! version if captured; outputs: digest/size; a static tradeoff note; a
//! license risk warning; and a real, structural GGUF-parses smoke-test
//! result), persisted as `report.json` next to the produced artifact.

use crate::m3_runtime_hub::{sha256_file, M3ModelLicense};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// ===========================================================================
// Errors
// ===========================================================================

#[derive(Debug)]
pub enum QuantizationError {
    Invalid(String),
    NotFound(String),
    Unsupported(String),
    Backend(String),
    Digest(String),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json(serde_json::Error),
}

impl fmt::Display for QuantizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid: {message}"),
            Self::NotFound(message) => write!(formatter, "not found: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported: {message}"),
            Self::Backend(message) => write!(formatter, "backend: {message}"),
            Self::Digest(message) => write!(formatter, "digest: {message}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} at {}: {source}", path.display()),
            Self::Json(error) => write!(formatter, "JSON: {error}"),
        }
    }
}

impl std::error::Error for QuantizationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for QuantizationError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub type QuantizationResult<T> = Result<T, QuantizationError>;

fn io_at(operation: &'static str, path: &Path, source: io::Error) -> QuantizationError {
    QuantizationError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ===========================================================================
// GGUF header sniffing
// ===========================================================================

const GGUF_MAGIC: [u8; 4] = *b"GGUF";
/// GGUF v1 used 32-bit lengths/counts throughout; every file produced by a
/// current `llama.cpp`/conversion tool is v2 or v3 (64-bit lengths/counts).
/// Rejecting v1 keeps the parser below simple and correct rather than
/// silently misreading an effectively obsolete format.
const MIN_SUPPORTED_GGUF_VERSION: u32 = 2;
const MAX_SUPPORTED_GGUF_VERSION: u32 = 3;
/// Bounds the total bytes read while parsing the metadata + tensor-info
/// sections of a GGUF file (never the tensor payload itself, which this
/// module never reads). Protects against a corrupt or hostile file claiming
/// an enormous key/tensor count from causing unbounded reads.
const MAX_GGUF_HEADER_SECTION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GGUF_STRING_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GGUF_ARRAY_ELEMENTS: u64 = 4_000_000;
const MAX_GGUF_METADATA_KV_COUNT: u64 = 1_000_000;
const MAX_GGUF_TENSOR_COUNT: u64 = 1_000_000;
const MAX_GGUF_VALUE_NESTING: u32 = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GgufHeaderInfo {
    pub version: u32,
    pub tensor_count: u64,
    pub metadata_kv_count: u64,
    pub architecture: Option<String>,
    pub name: Option<String>,
    pub quantization_version: Option<String>,
    /// The model's own trained context window, read straight from its
    /// `<architecture>.context_length` metadata key (e.g.
    /// `llama.context_length`, `qwen2.context_length`) — the authoritative
    /// source for how large a context this model was actually trained for,
    /// used to auto-size `llama-server`'s `-c` instead of one fixed guess
    /// for every model (see `llama.rs::resolve_ctx_size`).
    pub context_length: Option<u64>,
    pub declared_license: Option<String>,
    /// Kept internal so callers can validate runtime compatibility without
    /// exposing a potentially large template in reports or over IPC.
    #[serde(skip)]
    pub(crate) chat_template: Option<String>,
}

/// A `Read` adapter that errors once more than `remaining` bytes have been
/// read through it, so a single bounds check protects every read call made
/// while parsing a GGUF header instead of scattering arithmetic through the
/// parser.
struct BoundedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            if buf.is_empty() {
                return Ok(0);
            }
            return Err(io::Error::other(
                "GGUF header/metadata section exceeded its safety bound",
            ));
        }
        let cap = buf.len().min(self.remaining as usize);
        let read = self.inner.read(&mut buf[..cap])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

fn read_u32<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_gguf_string<R: Read>(reader: &mut R) -> QuantizationResult<String> {
    let len = read_u64(reader)
        .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF string length: {error}")))?;
    if len > MAX_GGUF_STRING_BYTES {
        return Err(QuantizationError::Invalid(format!(
            "GGUF string of {len} bytes exceeds the {MAX_GGUF_STRING_BYTES} byte safety bound"
        )));
    }
    let mut bytes = vec![0_u8; len as usize];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF string body: {error}")))?;
    String::from_utf8(bytes)
        .map_err(|error| QuantizationError::Invalid(format!("GGUF string is not valid UTF-8: {error}")))
}

/// GGUF metadata value type tags, from the `gguf` binary format.
const GGUF_TYPE_UINT8: u32 = 0;
const GGUF_TYPE_INT8: u32 = 1;
const GGUF_TYPE_UINT16: u32 = 2;
const GGUF_TYPE_INT16: u32 = 3;
const GGUF_TYPE_UINT32: u32 = 4;
const GGUF_TYPE_INT32: u32 = 5;
const GGUF_TYPE_FLOAT32: u32 = 6;
const GGUF_TYPE_BOOL: u32 = 7;
const GGUF_TYPE_STRING: u32 = 8;
const GGUF_TYPE_ARRAY: u32 = 9;
const GGUF_TYPE_UINT64: u32 = 10;
const GGUF_TYPE_INT64: u32 = 11;
const GGUF_TYPE_FLOAT64: u32 = 12;

/// A parsed scalar metadata value worth keeping around after the read —
/// everything else (`read_gguf_value`'s other branches) is discarded once
/// its bytes are consumed, since nothing today needs it.
enum GgufScalar {
    Text(String),
    UInt(u64),
}

/// Reads (and, for strings/integers, returns) one metadata value of
/// `value_type`, recursing for arrays up to [`MAX_GGUF_VALUE_NESTING`] deep.
fn read_gguf_value<R: Read>(
    reader: &mut R,
    value_type: u32,
    depth: u32,
) -> QuantizationResult<Option<GgufScalar>> {
    if depth > MAX_GGUF_VALUE_NESTING {
        return Err(QuantizationError::Invalid(
            "GGUF metadata array nesting exceeded the safety bound".to_string(),
        ));
    }
    match value_type {
        GGUF_TYPE_UINT8 | GGUF_TYPE_INT8 | GGUF_TYPE_BOOL => {
            let mut byte = [0_u8; 1];
            reader
                .read_exact(&mut byte)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF scalar: {error}")))?;
            Ok(Some(GgufScalar::UInt(byte[0] as u64)))
        }
        GGUF_TYPE_UINT16 | GGUF_TYPE_INT16 => {
            let mut bytes = [0_u8; 2];
            reader
                .read_exact(&mut bytes)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF scalar: {error}")))?;
            Ok(Some(GgufScalar::UInt(u16::from_le_bytes(bytes) as u64)))
        }
        GGUF_TYPE_UINT32 | GGUF_TYPE_INT32 => {
            let value = read_u32(reader)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF scalar: {error}")))?;
            Ok(Some(GgufScalar::UInt(value as u64)))
        }
        GGUF_TYPE_FLOAT32 => {
            read_u32(reader)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF scalar: {error}")))?;
            Ok(None)
        }
        GGUF_TYPE_UINT64 | GGUF_TYPE_INT64 => {
            let value = read_u64(reader)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF scalar: {error}")))?;
            Ok(Some(GgufScalar::UInt(value)))
        }
        GGUF_TYPE_FLOAT64 => {
            read_u64(reader)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF scalar: {error}")))?;
            Ok(None)
        }
        GGUF_TYPE_STRING => Ok(Some(GgufScalar::Text(read_gguf_string(reader)?))),
        GGUF_TYPE_ARRAY => {
            let element_type = read_u32(reader)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF array header: {error}")))?;
            let count = read_u64(reader)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF array header: {error}")))?;
            if count > MAX_GGUF_ARRAY_ELEMENTS {
                return Err(QuantizationError::Invalid(format!(
                    "GGUF array of {count} elements exceeds the {MAX_GGUF_ARRAY_ELEMENTS} element safety bound"
                )));
            }
            for _ in 0..count {
                read_gguf_value(reader, element_type, depth + 1)?;
            }
            Ok(None)
        }
        other => Err(QuantizationError::Invalid(format!(
            "unknown GGUF metadata value type {other}"
        ))),
    }
}

/// Parses a GGUF file's magic, version, tensor/metadata counts, and a
/// handful of well-known `general.*` and tokenizer string metadata keys,
/// from any `Read`
/// (a real file when sniffing on disk, an in-memory cursor in tests). Only
/// ever reads through the metadata + tensor-info sections — the tensor
/// payload itself is never touched, so this is fast and bounded even on a
/// multi-gigabyte model.
pub fn sniff_gguf_header<R: Read>(source: R) -> QuantizationResult<GgufHeaderInfo> {
    let mut reader = BoundedReader {
        inner: source,
        remaining: MAX_GGUF_HEADER_SECTION_BYTES,
    };

    let mut magic = [0_u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF magic: {error}")))?;
    if magic != GGUF_MAGIC {
        return Err(QuantizationError::Invalid(
            "file does not start with the GGUF magic bytes".to_string(),
        ));
    }
    let version = read_u32(&mut reader)
        .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF version: {error}")))?;
    if !(MIN_SUPPORTED_GGUF_VERSION..=MAX_SUPPORTED_GGUF_VERSION).contains(&version) {
        return Err(QuantizationError::Unsupported(format!(
            "unsupported GGUF version {version} (supported: {MIN_SUPPORTED_GGUF_VERSION}..={MAX_SUPPORTED_GGUF_VERSION})"
        )));
    }
    let tensor_count = read_u64(&mut reader)
        .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF tensor count: {error}")))?;
    if tensor_count > MAX_GGUF_TENSOR_COUNT {
        return Err(QuantizationError::Invalid(format!(
            "GGUF tensor count {tensor_count} exceeds the {MAX_GGUF_TENSOR_COUNT} safety bound"
        )));
    }
    let metadata_kv_count = read_u64(&mut reader)
        .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF metadata count: {error}")))?;
    if metadata_kv_count > MAX_GGUF_METADATA_KV_COUNT {
        return Err(QuantizationError::Invalid(format!(
            "GGUF metadata count {metadata_kv_count} exceeds the {MAX_GGUF_METADATA_KV_COUNT} safety bound"
        )));
    }

    let mut architecture = None;
    let mut name = None;
    let mut quantization_version = None;
    let mut context_length = None;
    let mut declared_license = None;
    let mut chat_template = None;
    for _ in 0..metadata_kv_count {
        let key = read_gguf_string(&mut reader)?;
        let value_type = read_u32(&mut reader)
            .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF value type: {error}")))?;
        let value = read_gguf_value(&mut reader, value_type, 0)?;
        match (key.as_str(), value) {
            ("general.architecture", Some(GgufScalar::Text(text))) => architecture = Some(text),
            ("general.name", Some(GgufScalar::Text(text))) => name = Some(text),
            ("general.quantization_version", Some(GgufScalar::Text(text))) => {
                quantization_version = Some(text)
            }
            ("tokenizer.chat_template", Some(GgufScalar::Text(text))) => chat_template = Some(text),
            (
                "general.license" | "general.license.name" | "general.license.spdx",
                Some(GgufScalar::Text(text)),
            ) if declared_license.is_none() => {
                declared_license = Some(text);
            }
            // Keyed per-architecture (`llama.context_length`,
            // `qwen2.context_length`, ...) rather than a fixed name, so match
            // the suffix instead of one exact key. Only one such key exists
            // per real GGUF file.
            (key, Some(GgufScalar::UInt(value))) if key.ends_with(".context_length") => {
                context_length = Some(value);
            }
            _ => {}
        }
    }

    // Structurally walk the tensor-info table too (name, dimensions, ggml
    // type, data offset) so a truncated/corrupt tensor table — not just a
    // truncated metadata section — is caught by the same parse. This is the
    // real "does it parse as valid GGUF" smoke test the eval result reuses.
    for _ in 0..tensor_count {
        let _name = read_gguf_string(&mut reader)?;
        let dimension_count = read_u32(&mut reader)
            .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF tensor dims: {error}")))?;
        if dimension_count > 64 {
            return Err(QuantizationError::Invalid(
                "GGUF tensor has an implausible dimension count".to_string(),
            ));
        }
        for _ in 0..dimension_count {
            read_u64(&mut reader)
                .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF tensor dims: {error}")))?;
        }
        let _ggml_type = read_u32(&mut reader)
            .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF tensor type: {error}")))?;
        let _offset = read_u64(&mut reader)
            .map_err(|error| QuantizationError::Invalid(format!("truncated GGUF tensor offset: {error}")))?;
    }

    Ok(GgufHeaderInfo {
        version,
        tensor_count,
        metadata_kv_count,
        architecture,
        name,
        quantization_version,
        context_length,
        declared_license,
        chat_template,
    })
}

pub(crate) fn sniff_gguf_file(path: &Path) -> QuantizationResult<GgufHeaderInfo> {
    let file = File::open(path).map_err(|error| io_at("open GGUF source", path, error))?;
    sniff_gguf_header(file)
}

// ===========================================================================
// Safetensors header sniffing
// ===========================================================================

const MAX_SAFETENSORS_HEADER_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetensorsHeaderInfo {
    pub header_size_bytes: u64,
    pub tensor_count: usize,
    pub metadata: BTreeMap<String, String>,
    pub declared_license: Option<String>,
}

/// Parses a safetensors file's `u64le` header length followed by that many
/// bytes of JSON (a map of tensor name -> `{dtype, shape, data_offsets}`,
/// plus an optional `__metadata__` string map) — the format every
/// `.safetensors` file starts with, regardless of what it stores.
pub fn sniff_safetensors_header<R: Read>(mut source: R) -> QuantizationResult<SafetensorsHeaderInfo> {
    let header_size = read_u64(&mut source)
        .map_err(|error| QuantizationError::Invalid(format!("truncated safetensors header length: {error}")))?;
    if header_size == 0 || header_size > MAX_SAFETENSORS_HEADER_BYTES {
        return Err(QuantizationError::Invalid(format!(
            "safetensors header length {header_size} is not between 1 and {MAX_SAFETENSORS_HEADER_BYTES} bytes"
        )));
    }
    let mut header_bytes = vec![0_u8; header_size as usize];
    source
        .read_exact(&mut header_bytes)
        .map_err(|error| QuantizationError::Invalid(format!("truncated safetensors header body: {error}")))?;
    let parsed: Value = serde_json::from_slice(&header_bytes)
        .map_err(|error| QuantizationError::Invalid(format!("safetensors header is not valid JSON: {error}")))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| QuantizationError::Invalid("safetensors header JSON is not an object".to_string()))?;

    let mut metadata = BTreeMap::new();
    let mut declared_license = None;
    let mut tensor_count = 0_usize;
    for (key, value) in object {
        if key == "__metadata__" {
            if let Some(entries) = value.as_object() {
                for (meta_key, meta_value) in entries {
                    if let Some(text) = meta_value.as_str() {
                        if meta_key.eq_ignore_ascii_case("license") && declared_license.is_none() {
                            declared_license = Some(text.to_string());
                        }
                        metadata.insert(meta_key.clone(), text.to_string());
                    }
                }
            }
            continue;
        }
        // A real tensor descriptor always carries dtype/shape/data_offsets;
        // this is a light structural check, not a full validation.
        let is_tensor_descriptor = value
            .as_object()
            .is_some_and(|entry| entry.contains_key("dtype") && entry.contains_key("shape"));
        if is_tensor_descriptor {
            tensor_count += 1;
        }
    }

    Ok(SafetensorsHeaderInfo {
        header_size_bytes: header_size,
        tensor_count,
        metadata,
        declared_license,
    })
}

fn sniff_safetensors_file(path: &Path) -> QuantizationResult<SafetensorsHeaderInfo> {
    let file = File::open(path).map_err(|error| io_at("open safetensors source", path, error))?;
    sniff_safetensors_header(file)
}

/// Best-effort `"license"`/`"license_name"` lookup at the top level of a
/// Hugging Face-style `config.json`. Rare, but some repos do carry it there;
/// this is deliberately narrow (only the two exact top-level keys) rather
/// than a general config parser.
fn config_json_declared_license(directory: &Path) -> Option<String> {
    let config_path = directory.join("config.json");
    let bytes = fs::read(config_path).ok()?;
    let parsed: Value = serde_json::from_slice(&bytes).ok()?;
    let object = parsed.as_object()?;
    for key in ["license", "license_name"] {
        if let Some(text) = object.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

// ===========================================================================
// Source format detection
// ===========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Gguf,
    Safetensors,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum SourceHeader {
    Gguf(GgufHeaderInfo),
    Safetensors(SafetensorsHeaderInfo),
}

impl SourceHeader {
    pub fn declared_license(&self) -> Option<&str> {
        match self {
            Self::Gguf(header) => header.declared_license.as_deref(),
            Self::Safetensors(header) => header.declared_license.as_deref(),
        }
    }
}

/// Bound on the source file this workbench will digest/convert. Not a real
/// technical limit, just a sanity guard against pointing it at something
/// absurd (e.g. a block device).
const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024 * 1024;

fn first_safetensors_file(directory: &Path) -> QuantizationResult<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(directory)
        .map_err(|error| io_at("list source directory", directory, error))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("safetensors"))
        })
        .collect();
    candidates.sort();
    // Prefer the conventional single-shard filename when present, otherwise
    // fall back to the first shard alphabetically (e.g.
    // `model-00001-of-00004.safetensors`) — either way this only sniffs one
    // representative file for format/metadata detection, it never attempts
    // to merge/convert a multi-shard checkout (see the module doc comment).
    if let Some(preferred) = candidates
        .iter()
        .position(|path| path.file_name().and_then(OsStr::to_str) == Some("model.safetensors"))
    {
        return Ok(candidates.remove(preferred));
    }
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| QuantizationError::NotFound(format!("no .safetensors file found in {}", directory.display())))
}

/// Detects the source format of `path` (a single file or a Hugging
/// Face-style checkout directory) and sniffs its header. Returns the exact
/// file that was sniffed/will be digested (equal to `path` for a plain
/// file).
pub fn detect_and_sniff_source(path: &Path) -> QuantizationResult<(PathBuf, SourceFormat, SourceHeader)> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_at("inspect source", path, error))?;
    if metadata.is_dir() {
        let file = first_safetensors_file(path)?;
        let mut header = sniff_safetensors_file(&file)?;
        if header.declared_license.is_none() {
            header.declared_license = config_json_declared_license(path);
        }
        return Ok((file, SourceFormat::Safetensors, SourceHeader::Safetensors(header)));
    }
    if !metadata.is_file() {
        return Err(QuantizationError::Invalid(
            "source path is neither a regular file nor a directory".to_string(),
        ));
    }
    if metadata.len() == 0 || metadata.len() > MAX_SOURCE_BYTES {
        return Err(QuantizationError::Invalid(format!(
            "source file size {} is not between 1 and {MAX_SOURCE_BYTES} bytes",
            metadata.len()
        )));
    }

    let mut magic = [0_u8; 4];
    let mut probe = File::open(path).map_err(|error| io_at("open source", path, error))?;
    probe
        .read_exact(&mut magic)
        .map_err(|error| io_at("read source header", path, error))?;
    if magic == GGUF_MAGIC {
        let header = sniff_gguf_file(path)?;
        return Ok((path.to_path_buf(), SourceFormat::Gguf, SourceHeader::Gguf(header)));
    }
    if let Ok(header) = sniff_safetensors_file(path) {
        return Ok((
            path.to_path_buf(),
            SourceFormat::Safetensors,
            SourceHeader::Safetensors(header),
        ));
    }
    Err(QuantizationError::Unsupported(
        "source does not look like a GGUF file (bad magic) or a safetensors file (bad header)".to_string(),
    ))
}

// ===========================================================================
// License assessment
// ===========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseSource {
    InstalledModelManifest,
    GgufMetadata,
    SafetensorsMetadata,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseRisk {
    Permissive,
    Copyleft,
    Restricted,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseAssessment {
    pub declared_name: Option<String>,
    pub declared_spdx_id: Option<String>,
    pub source: LicenseSource,
    pub risk: LicenseRisk,
    pub warning: Option<String>,
}

/// Heuristic, best-effort license risk classification by keyword matching —
/// not legal advice. Ordered so specific, well-known restricted/copyleft
/// licenses are recognized before falling back to a generic "permissive" or
/// "unknown, review manually" bucket.
const RESTRICTED_MARKERS: &[&str] = &[
    "non-commercial",
    "noncommercial",
    "non commercial",
    "cc-by-nc",
    "research only",
    "research-only",
    "not for commercial",
    "no commercial use",
    "llama2",
    "llama 2",
    "llama3",
    "llama 3",
    "llama4",
    "llama 4",
    "gemma",
    "openrail",
];
const COPYLEFT_MARKERS: &[&str] = &["agpl", "gpl", "lgpl", "mpl", "cc-by-sa", "eupl"];
const PERMISSIVE_MARKERS: &[&str] = &[
    "mit", "apache", "bsd", "isc", "unlicense", "cc0", "cc-by-4", "cc-by ",
];

pub fn assess_license(
    declared_name: Option<&str>,
    declared_spdx_id: Option<&str>,
    source: LicenseSource,
) -> LicenseAssessment {
    let haystack = [declared_name, declared_spdx_id]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();

    let (risk, warning) = if haystack.trim().is_empty() {
        (
            LicenseRisk::Unknown,
            Some(
                "No license declaration was found for this model; verify licensing manually before distributing or serving the converted output.".to_string(),
            ),
        )
    } else if RESTRICTED_MARKERS.iter().any(|marker| haystack.contains(marker)) {
        (
            LicenseRisk::Restricted,
            Some(format!(
                "Declared license \"{}\" appears to carry non-commercial, research-only, or use-based restrictions; review its exact terms before distributing or serving the converted output.",
                declared_name.or(declared_spdx_id).unwrap_or("unknown")
            )),
        )
    } else if COPYLEFT_MARKERS.iter().any(|marker| haystack.contains(marker)) {
        (
            LicenseRisk::Copyleft,
            Some(format!(
                "Declared license \"{}\" is a copyleft license; redistributing the converted output may carry share-alike/source-disclosure obligations.",
                declared_name.or(declared_spdx_id).unwrap_or("unknown")
            )),
        )
    } else if PERMISSIVE_MARKERS.iter().any(|marker| haystack.contains(marker)) {
        (LicenseRisk::Permissive, None)
    } else {
        (
            LicenseRisk::Unknown,
            Some(format!(
                "Declared license \"{}\" was not recognized by this heuristic; review its exact terms manually.",
                declared_name.or(declared_spdx_id).unwrap_or("unknown")
            )),
        )
    };

    LicenseAssessment {
        declared_name: declared_name.map(str::to_string),
        declared_spdx_id: declared_spdx_id.map(str::to_string),
        source,
        risk,
        warning,
    }
}

fn assess_license_from_catalog(license: &M3ModelLicense) -> LicenseAssessment {
    assess_license(
        Some(license.name.as_str()),
        license.spdx_id.as_deref(),
        LicenseSource::InstalledModelManifest,
    )
}

// ===========================================================================
// Quantization type reference table
// ===========================================================================

/// Well-known `llama.cpp` GGUF quantization types this workbench can request
/// from a real quantizer, plus `Copy` (matching `llama-quantize`'s own
/// `COPY` type) for a lossless passthrough. Bits-per-weight classes and
/// tradeoff notes below are widely cited, descriptive figures for these
/// quant families — not benchmark numbers measured by this app for any
/// particular model — and are surfaced to the user with that framing.
// `#[serde(rename_all = "snake_case")]` is deliberately not used here: serde's
// snake_case rule inserts an underscore at every uppercase letter, which
// mangles already-underscored, CLI-style variant names like `Q6_K` into
// `"q6__k"`. `Serialize`/`Deserialize` are implemented manually below instead,
// directly in terms of `cli_name`/`parse`, so the wire format always matches
// the exact string `llama-quantize` itself accepts (e.g. `"Q6_K"`) — the same
// string the `quantization_quant_types` command's `cliName`/`id` fields use.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GgufQuantType {
    Copy,
    F32,
    F16,
    Bf16,
    Q8_0,
    Q6_K,
    Q5_K_M,
    Q5_K_S,
    Q5_0,
    Q4_K_M,
    Q4_K_S,
    Q4_0,
    Q3_K_M,
    Q2_K,
}

impl GgufQuantType {
    pub fn all() -> &'static [GgufQuantType] {
        &[
            Self::Copy,
            Self::F32,
            Self::F16,
            Self::Bf16,
            Self::Q8_0,
            Self::Q6_K,
            Self::Q5_K_M,
            Self::Q5_K_S,
            Self::Q5_0,
            Self::Q4_K_M,
            Self::Q4_K_S,
            Self::Q4_0,
            Self::Q3_K_M,
            Self::Q2_K,
        ]
    }

    /// The exact string `llama-quantize` expects as its `type` argument.
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Copy => "COPY",
            Self::F32 => "F32",
            Self::F16 => "F16",
            Self::Bf16 => "BF16",
            Self::Q8_0 => "Q8_0",
            Self::Q6_K => "Q6_K",
            Self::Q5_K_M => "Q5_K_M",
            Self::Q5_K_S => "Q5_K_S",
            Self::Q5_0 => "Q5_0",
            Self::Q4_K_M => "Q4_K_M",
            Self::Q4_K_S => "Q4_K_S",
            Self::Q4_0 => "Q4_0",
            Self::Q3_K_M => "Q3_K_M",
            Self::Q2_K => "Q2_K",
        }
    }

    /// A plain-language, descriptive size/quality tradeoff note. See the
    /// struct-level doc comment about what these figures are (and are not).
    pub fn tradeoff_note(self) -> &'static str {
        match self {
            Self::Copy => "No quantization: bytes are copied verbatim. Used only to re-verify/repackage a GGUF file, never to reduce size.",
            Self::F32 => "32 bits/weight, full precision. Largest size; mainly useful as a conversion source, not for serving.",
            Self::F16 => "16 bits/weight, full half precision. No quantization loss beyond the original training/export precision.",
            Self::Bf16 => "16 bits/weight, brain-float precision. Same size class as F16 with a different exponent/mantissa split.",
            Self::Q8_0 => "~8.5 bits/weight. Near-lossless versus F16 for most tasks; much larger than the K-quants below it.",
            Self::Q6_K => "~6.6 bits/weight. Close to F16 quality; noticeably larger than Q5/Q4 K-quants.",
            Self::Q5_K_M => "~5.7 bits/weight. Higher quality than Q4_K_M at a larger file size; a common \"quality-first\" choice.",
            Self::Q5_K_S => "~5.5 bits/weight. Slightly smaller than Q5_K_M with a small additional quality loss.",
            Self::Q5_0 => "~5.5 bits/weight, legacy linear quantization. Mostly superseded by Q5_K_M/Q5_K_S.",
            Self::Q4_K_M => "~4.8 bits/weight. The most common default: a broadly good size/quality balance for local inference.",
            Self::Q4_K_S => "~4.6 bits/weight. Smaller than Q4_K_M with somewhat more quality loss.",
            Self::Q4_0 => "~4.5 bits/weight, legacy linear quantization. Mostly superseded by Q4_K_M/Q4_K_S.",
            Self::Q3_K_M => "~3.9 bits/weight. Small file size with clearly noticeable quality loss versus Q4/Q5.",
            Self::Q2_K => "~2.6 bits/weight. Smallest common quant; the largest quality loss, mainly used when absolute size is the priority.",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_uppercase();
        Self::all()
            .iter()
            .copied()
            .find(|quant| quant.cli_name() == normalized)
    }
}

impl Serialize for GgufQuantType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.cli_name())
    }
}

impl<'de> Deserialize<'de> for GgufQuantType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        GgufQuantType::parse(&value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown GGUF quantization type '{value}'")))
    }
}

// ===========================================================================
// Backends
// ===========================================================================

#[derive(Debug)]
pub struct BackendOutput {
    pub tool_name: String,
    pub tool_version: Option<String>,
    /// `true` only when the backend genuinely performed the requested
    /// transformation (real quantization); `false` for a passthrough/no-op.
    pub real: bool,
}

pub trait QuantizationBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn is_available(&self) -> bool;
    fn supports_format(&self, format: SourceFormat) -> bool;
    fn convert(
        &self,
        source: &Path,
        output: &Path,
        quant: GgufQuantType,
        allow_requantize: bool,
    ) -> QuantizationResult<BackendOutput>;
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Scans the directories in `path_var` (a `PATH`-style env var value) for an
/// executable named `name`. Takes the PATH value as a plain parameter
/// (rather than reading `std::env` directly) so tests can exercise this with
/// a synthetic PATH instead of mutating real process-wide environment state.
fn discover_in_path_dirs(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Locates the `llama-quantize` binary: first on `PATH`, then in common
/// Homebrew install locations — mirroring `llama.rs`'s
/// `find_llama_server_binary` convention for the sibling `llama-quantize`
/// tool shipped by the same `llama.cpp` formula.
pub fn find_llama_quantize_binary() -> Option<PathBuf> {
    if let Some(found) = discover_in_path_dirs("llama-quantize", std::env::var_os("PATH").as_deref()) {
        return Some(found);
    }
    for base in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let candidate = Path::new(base).join("llama-quantize");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Pulls `build = 1234 (abcdef0)` out of `llama-quantize`'s own stderr
/// banner. Returns `None` (never fabricates a version) when the pattern
/// isn't present, e.g. because the binary predates that banner.
fn extract_llama_cpp_build_version(stderr: &str) -> Option<String> {
    let pattern = Regex::new(r"build\s*=\s*(\d+)\s*\(([0-9a-fA-F]+)\)").ok()?;
    let captures = pattern.captures(stderr)?;
    Some(format!("{}({})", &captures[1], &captures[2]))
}

/// Shells out to a real `llama-quantize` binary to perform genuine GGUF ->
/// GGUF (re)quantization. This is the "real backend" the ROADMAP acceptance
/// criteria describe, used automatically when `llama-quantize` is found on
/// this machine (see [`find_llama_quantize_binary`]).
pub struct LlamaCppQuantizeBackend {
    binary: PathBuf,
}

impl LlamaCppQuantizeBackend {
    pub fn from_binary(binary: impl Into<PathBuf>) -> Self {
        Self { binary: binary.into() }
    }

    pub fn discover() -> Option<Self> {
        find_llama_quantize_binary().map(Self::from_binary)
    }
}

impl QuantizationBackend for LlamaCppQuantizeBackend {
    fn id(&self) -> &'static str {
        "llama-quantize"
    }

    fn is_available(&self) -> bool {
        is_executable_file(&self.binary)
    }

    fn supports_format(&self, format: SourceFormat) -> bool {
        matches!(format, SourceFormat::Gguf)
    }

    fn convert(
        &self,
        source: &Path,
        output: &Path,
        quant: GgufQuantType,
        allow_requantize: bool,
    ) -> QuantizationResult<BackendOutput> {
        let mut command = Command::new(&self.binary);
        if allow_requantize {
            command.arg("--allow-requantize");
        }
        command.arg(source).arg(output).arg(quant.cli_name());
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let result = command.output().map_err(|error| {
            QuantizationError::Backend(format!("failed to spawn {}: {error}", self.binary.display()))
        })?;
        let stderr = String::from_utf8_lossy(&result.stderr).to_string();
        if !result.status.success() {
            return Err(QuantizationError::Backend(format!(
                "{} exited with {}: {}",
                self.binary.display(),
                result.status,
                stderr.trim()
            )));
        }
        Ok(BackendOutput {
            tool_name: "llama-quantize".to_string(),
            tool_version: extract_llama_cpp_build_version(&stderr),
            real: true,
        })
    }
}

/// An honest no-op fallback for when no real GGUF quantizer is available:
/// copies the source file's bytes verbatim. Never claims to have quantized
/// anything — [`QuantizationWorkbench::convert`] refuses to pair this
/// backend with any [`GgufQuantType`] other than [`GgufQuantType::Copy`], and
/// the resulting [`ToolInfo::real`] is always `false`.
pub struct PassthroughGgufRequantize;

impl QuantizationBackend for PassthroughGgufRequantize {
    fn id(&self) -> &'static str {
        "passthrough-copy"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn supports_format(&self, format: SourceFormat) -> bool {
        matches!(format, SourceFormat::Gguf)
    }

    fn convert(
        &self,
        source: &Path,
        output: &Path,
        quant: GgufQuantType,
        _allow_requantize: bool,
    ) -> QuantizationResult<BackendOutput> {
        if quant != GgufQuantType::Copy {
            return Err(QuantizationError::Unsupported(format!(
                "no real GGUF quantizer is available; only {} (verify + recopy, no quantization) is supported without one",
                GgufQuantType::Copy.cli_name()
            )));
        }
        fs::copy(source, output).map_err(|error| io_at("copy passthrough output", output, error))?;
        Ok(BackendOutput {
            tool_name: "passthrough-copy".to_string(),
            tool_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            real: false,
        })
    }
}

// ===========================================================================
// Report + orchestration
// ===========================================================================

pub const QUANTIZATION_REPORT_SCHEMA_VERSION: u32 = 1;
pub const QUANTIZATION_REPORT_FILE: &str = "report.json";
pub const QUANTIZATION_OUTPUT_FILE: &str = "output.gguf";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: PathBuf,
    pub format: SourceFormat,
    pub sha256: String,
    pub size_bytes: u64,
    pub header: SourceHeader,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub backend_id: String,
    pub name: String,
    pub version: Option<String>,
    pub real: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputInfo {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalResult {
    pub method: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionReport {
    pub schema_version: u32,
    pub conversion_id: String,
    pub generated_at_ms: u64,
    pub source: SourceInfo,
    pub quant_choice: GgufQuantType,
    pub allow_requantize: bool,
    pub tool: ToolInfo,
    pub output: OutputInfo,
    pub tradeoff_note: String,
    pub license: LicenseAssessment,
    pub eval: EvalResult,
}

/// What license information to attach to the report: either reused directly
/// from an already-installed, hub-verified model, or left to be sniffed out
/// of the source file/directory itself.
#[derive(Clone, Debug)]
pub enum DeclaredLicense {
    FromInstalledModel(M3ModelLicense),
    SniffFromSource,
}

#[derive(Clone, Debug)]
pub struct ConversionRequest {
    pub source_path: PathBuf,
    pub quant_choice: GgufQuantType,
    pub allow_requantize: bool,
    pub license: DeclaredLicense,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendDescriptor {
    pub id: &'static str,
    pub available: bool,
}

/// Orchestrates source detection/validation, backend selection, digesting,
/// the output smoke-test eval, and reproducible report persistence. Owns a
/// private storage root (one subdirectory per conversion, holding
/// `output.gguf` + `report.json`) independent of the model manifest/blob
/// store in `m3_runtime_hub.rs` — conversions are a separate, simpler
/// lifecycle (no versioning/rollback) from installed models.
pub struct QuantizationWorkbench {
    root: PathBuf,
    backends: Vec<Arc<dyn QuantizationBackend>>,
}

impl QuantizationWorkbench {
    pub fn new(root: impl Into<PathBuf>, backends: Vec<Arc<dyn QuantizationBackend>>) -> Self {
        Self {
            root: root.into(),
            backends,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list_backends(&self) -> Vec<BackendDescriptor> {
        self.backends
            .iter()
            .map(|backend| BackendDescriptor {
                id: backend.id(),
                available: backend.is_available(),
            })
            .collect()
    }

    pub fn quant_types(&self) -> Vec<(GgufQuantType, &'static str, &'static str)> {
        GgufQuantType::all()
            .iter()
            .map(|quant| (*quant, quant.cli_name(), quant.tradeoff_note()))
            .collect()
    }

    fn select_backend(&self, format: SourceFormat, quant: GgufQuantType) -> QuantizationResult<&Arc<dyn QuantizationBackend>> {
        if format != SourceFormat::Gguf {
            return Err(QuantizationError::Unsupported(
                "converting safetensors/Hugging Face sources to GGUF requires an external conversion toolchain (e.g. llama.cpp's convert_hf_to_gguf.py with Python + transformers/torch) that is not bundled with this app; convert to GGUF externally first, then use this workbench to quantize the resulting file.".to_string(),
            ));
        }
        self.backends
            .iter()
            .filter(|backend| backend.supports_format(format) && backend.is_available())
            .find(|backend| quant == GgufQuantType::Copy || backend.id() != "passthrough-copy")
            .or_else(|| {
                self.backends
                    .iter()
                    .find(|backend| backend.supports_format(format) && backend.is_available())
            })
            .ok_or_else(|| {
                QuantizationError::Unsupported(
                    "no available quantization backend supports this source format".to_string(),
                )
            })
    }

    pub fn convert(&self, request: ConversionRequest) -> QuantizationResult<ConversionReport> {
        let (sniff_path, format, header) = detect_and_sniff_source(&request.source_path)?;
        let source_metadata =
            fs::metadata(&sniff_path).map_err(|error| io_at("inspect source", &sniff_path, error))?;
        let source_size = source_metadata.len();
        let source_digest = sha256_file(&sniff_path, source_size)
            .map_err(|error| QuantizationError::Digest(error.to_string()))?;

        let backend = self.select_backend(format, request.quant_choice)?;

        let conversion_id = Uuid::new_v4().to_string();
        let conversion_dir = self.root.join(&conversion_id);
        fs::create_dir_all(&conversion_dir)
            .map_err(|error| io_at("create conversion directory", &conversion_dir, error))?;
        let output_path = conversion_dir.join(QUANTIZATION_OUTPUT_FILE);

        let backend_output = backend.convert(
            &sniff_path,
            &output_path,
            request.quant_choice,
            request.allow_requantize,
        )?;

        let output_metadata =
            fs::metadata(&output_path).map_err(|error| io_at("inspect conversion output", &output_path, error))?;
        let output_size = output_metadata.len();
        let output_digest = sha256_file(&output_path, output_size)
            .map_err(|error| QuantizationError::Digest(error.to_string()))?;

        let eval = match sniff_gguf_file(&output_path) {
            Ok(info) => EvalResult {
                method: "gguf_header_parse".to_string(),
                passed: true,
                detail: format!(
                    "Output parses as a structurally valid GGUF v{} file with {} tensor(s) and {} metadata entries.",
                    info.version, info.tensor_count, info.metadata_kv_count
                ),
            },
            Err(error) => EvalResult {
                method: "gguf_header_parse".to_string(),
                passed: false,
                detail: format!("Output did not parse as a valid GGUF file: {error}"),
            },
        };

        let license = match &request.license {
            DeclaredLicense::FromInstalledModel(license) => assess_license_from_catalog(license),
            DeclaredLicense::SniffFromSource => assess_license(
                header.declared_license(),
                None,
                match format {
                    SourceFormat::Gguf => LicenseSource::GgufMetadata,
                    SourceFormat::Safetensors => LicenseSource::SafetensorsMetadata,
                },
            ),
        };

        let report = ConversionReport {
            schema_version: QUANTIZATION_REPORT_SCHEMA_VERSION,
            conversion_id,
            generated_at_ms: now_ms(),
            source: SourceInfo {
                path: sniff_path,
                format,
                sha256: source_digest,
                size_bytes: source_size,
                header,
            },
            quant_choice: request.quant_choice,
            allow_requantize: request.allow_requantize,
            tool: ToolInfo {
                backend_id: backend.id().to_string(),
                name: backend_output.tool_name,
                version: backend_output.tool_version,
                real: backend_output.real,
            },
            output: OutputInfo {
                path: output_path.clone(),
                sha256: output_digest,
                size_bytes: output_size,
            },
            tradeoff_note: request.quant_choice.tradeoff_note().to_string(),
            license,
            eval,
        };

        let report_path = conversion_dir.join(QUANTIZATION_REPORT_FILE);
        let report_json = serde_json::to_vec_pretty(&report)?;
        fs::write(&report_path, report_json).map_err(|error| io_at("write conversion report", &report_path, error))?;

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn write_u32(buffer: &mut Vec<u8>, value: u32) {
        buffer.extend_from_slice(&value.to_le_bytes());
    }

    fn write_u64(buffer: &mut Vec<u8>, value: u64) {
        buffer.extend_from_slice(&value.to_le_bytes());
    }

    fn write_string(buffer: &mut Vec<u8>, value: &str) {
        write_u64(buffer, value.len() as u64);
        buffer.extend_from_slice(value.as_bytes());
    }

    /// Builds a small, real, structurally valid GGUF v3 file: a handful of
    /// `general.*` string metadata entries and one tiny F32 tensor. Not a
    /// loadable model (no real architecture tensors) — just enough to
    /// exercise header/metadata/tensor-table parsing and the passthrough
    /// backend end to end.
    fn build_minimal_gguf(architecture: &str, license: Option<&str>) -> Vec<u8> {
        build_minimal_gguf_with_template(architecture, license, None)
    }

    fn build_minimal_gguf_with_template(
        architecture: &str,
        license: Option<&str>,
        chat_template: Option<&str>,
    ) -> Vec<u8> {
        build_minimal_gguf_full(architecture, license, chat_template, None)
    }

    fn build_minimal_gguf_full(
        architecture: &str,
        license: Option<&str>,
        chat_template: Option<&str>,
        context_length: Option<u32>,
    ) -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&GGUF_MAGIC);
        write_u32(&mut buffer, 3);
        write_u64(&mut buffer, 1); // tensor_count
        let metadata_count = 1
            + u64::from(license.is_some())
            + u64::from(chat_template.is_some())
            + u64::from(context_length.is_some());
        write_u64(&mut buffer, metadata_count);

        write_string(&mut buffer, "general.architecture");
        write_u32(&mut buffer, GGUF_TYPE_STRING);
        write_string(&mut buffer, architecture);

        if let Some(license) = license {
            write_string(&mut buffer, "general.license");
            write_u32(&mut buffer, GGUF_TYPE_STRING);
            write_string(&mut buffer, license);
        }

        if let Some(chat_template) = chat_template {
            write_string(&mut buffer, "tokenizer.chat_template");
            write_u32(&mut buffer, GGUF_TYPE_STRING);
            write_string(&mut buffer, chat_template);
        }

        if let Some(context_length) = context_length {
            write_string(&mut buffer, &format!("{architecture}.context_length"));
            write_u32(&mut buffer, GGUF_TYPE_UINT32);
            write_u32(&mut buffer, context_length);
        }

        // One tensor: name, 1 dimension, ggml type 0 (F32), offset 0.
        write_string(&mut buffer, "dummy.weight");
        write_u32(&mut buffer, 1);
        write_u64(&mut buffer, 4);
        write_u32(&mut buffer, 0);
        write_u64(&mut buffer, 0);

        // Sixteen bytes of "tensor payload" the header/tensor-table parser
        // never reads.
        buffer.extend_from_slice(&[0_u8; 16]);
        buffer
    }

    fn build_minimal_safetensors(license: Option<&str>) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        let mut tensor = serde_json::Map::new();
        tensor.insert("dtype".to_string(), Value::String("F32".to_string()));
        tensor.insert(
            "shape".to_string(),
            Value::Array(vec![Value::from(4)]),
        );
        tensor.insert(
            "data_offsets".to_string(),
            Value::Array(vec![Value::from(0), Value::from(16)]),
        );
        header.insert("weight".to_string(), Value::Object(tensor));
        if let Some(license) = license {
            let mut meta = serde_json::Map::new();
            meta.insert("license".to_string(), Value::String(license.to_string()));
            header.insert("__metadata__".to_string(), Value::Object(meta));
        }
        let header_bytes = serde_json::to_vec(&Value::Object(header)).unwrap();
        let mut buffer = Vec::new();
        write_u64(&mut buffer, header_bytes.len() as u64);
        buffer.extend_from_slice(&header_bytes);
        buffer.extend_from_slice(&[0_u8; 16]);
        buffer
    }

    #[test]
    fn sniffs_gguf_header_and_license_metadata() {
        let bytes = build_minimal_gguf("llama", Some("apache-2.0"));
        let header = sniff_gguf_header(Cursor::new(bytes)).expect("valid GGUF fixture must parse");
        assert_eq!(header.version, 3);
        assert_eq!(header.tensor_count, 1);
        assert_eq!(header.metadata_kv_count, 2);
        assert_eq!(header.architecture.as_deref(), Some("llama"));
        assert_eq!(header.declared_license.as_deref(), Some("apache-2.0"));
    }

    #[test]
    fn sniffs_context_length_keyed_by_architecture() {
        let bytes = build_minimal_gguf_full("qwen2", None, None, Some(32_768));
        let header = sniff_gguf_header(Cursor::new(bytes)).expect("valid GGUF fixture must parse");
        assert_eq!(header.context_length, Some(32_768));
    }

    #[test]
    fn context_length_absent_when_no_such_key_exists() {
        let bytes = build_minimal_gguf("llama", None);
        let header = sniff_gguf_header(Cursor::new(bytes)).expect("valid GGUF fixture must parse");
        assert_eq!(header.context_length, None);
    }

    #[test]
    fn sniffs_embedded_chat_template_without_serializing_its_body() {
        let template = "{% if tools %}{{ tools | tojson }}{% endif %}";
        let bytes = build_minimal_gguf_with_template("llama", None, Some(template));
        let header = sniff_gguf_header(Cursor::new(bytes)).expect("valid GGUF fixture must parse");

        assert_eq!(header.chat_template.as_deref(), Some(template));
        let wire = serde_json::to_value(&header).unwrap();
        assert!(wire.get("chatTemplate").is_none());
        assert!(!wire.to_string().contains(template));
    }

    #[test]
    fn rejects_bad_gguf_magic() {
        let error = sniff_gguf_header(Cursor::new(b"NOPE0000".to_vec())).unwrap_err();
        assert!(matches!(error, QuantizationError::Invalid(_)));
    }

    #[test]
    fn rejects_unsupported_gguf_version() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&GGUF_MAGIC);
        write_u32(&mut buffer, 1);
        write_u64(&mut buffer, 0);
        write_u64(&mut buffer, 0);
        let error = sniff_gguf_header(Cursor::new(buffer)).unwrap_err();
        assert!(matches!(error, QuantizationError::Unsupported(_)));
    }

    #[test]
    fn rejects_truncated_gguf_metadata_section() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&GGUF_MAGIC);
        write_u32(&mut buffer, 3);
        write_u64(&mut buffer, 0);
        write_u64(&mut buffer, 1); // claims one metadata entry, but supplies none
        let error = sniff_gguf_header(Cursor::new(buffer)).unwrap_err();
        assert!(matches!(error, QuantizationError::Invalid(_)));
    }

    #[test]
    fn sniffs_safetensors_header_and_metadata_license() {
        let bytes = build_minimal_safetensors(Some("mit"));
        let header = sniff_safetensors_header(Cursor::new(bytes)).expect("valid safetensors fixture must parse");
        assert_eq!(header.tensor_count, 1);
        assert_eq!(header.declared_license.as_deref(), Some("mit"));
    }

    #[test]
    fn rejects_non_object_safetensors_header() {
        let mut buffer = Vec::new();
        let header_bytes = serde_json::to_vec(&Value::Array(vec![])).unwrap();
        write_u64(&mut buffer, header_bytes.len() as u64);
        buffer.extend_from_slice(&header_bytes);
        let error = sniff_safetensors_header(Cursor::new(buffer)).unwrap_err();
        assert!(matches!(error, QuantizationError::Invalid(_)));
    }

    #[test]
    fn detects_gguf_source_from_bytes_on_disk() {
        let dir = tempfile_dir();
        let path = dir.join("model.gguf");
        fs::write(&path, build_minimal_gguf("llama", Some("apache-2.0"))).unwrap();
        let (resolved, format, header) = detect_and_sniff_source(&path).expect("must detect GGUF");
        assert_eq!(resolved, path);
        assert_eq!(format, SourceFormat::Gguf);
        assert_eq!(header.declared_license(), Some("apache-2.0"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_safetensors_directory_source() {
        let dir = tempfile_dir();
        fs::write(dir.join("model.safetensors"), build_minimal_safetensors(Some("cc-by-nc-4.0"))).unwrap();
        let (resolved, format, header) = detect_and_sniff_source(&dir).expect("must detect safetensors directory");
        assert_eq!(resolved, dir.join("model.safetensors"));
        assert_eq!(format, SourceFormat::Safetensors);
        assert_eq!(header.declared_license(), Some("cc-by-nc-4.0"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn license_classifier_flags_restricted_and_copyleft_and_permissive() {
        let restricted = assess_license(Some("Llama 3 Community License"), None, LicenseSource::GgufMetadata);
        assert_eq!(restricted.risk, LicenseRisk::Restricted);
        assert!(restricted.warning.is_some());

        let copyleft = assess_license(Some("GPL-3.0"), Some("GPL-3.0"), LicenseSource::GgufMetadata);
        assert_eq!(copyleft.risk, LicenseRisk::Copyleft);

        let permissive = assess_license(Some("Apache License 2.0"), Some("Apache-2.0"), LicenseSource::GgufMetadata);
        assert_eq!(permissive.risk, LicenseRisk::Permissive);
        assert!(permissive.warning.is_none());

        let unknown = assess_license(None, None, LicenseSource::None);
        assert_eq!(unknown.risk, LicenseRisk::Unknown);
        assert!(unknown.warning.is_some());
    }

    #[test]
    fn quant_type_parses_case_insensitively_and_round_trips_cli_name() {
        assert_eq!(GgufQuantType::parse("q4_k_m"), Some(GgufQuantType::Q4_K_M));
        assert_eq!(GgufQuantType::parse("Q4_K_M"), Some(GgufQuantType::Q4_K_M));
        assert_eq!(GgufQuantType::parse("nonsense"), None);
        for quant in GgufQuantType::all() {
            assert_eq!(GgufQuantType::parse(quant.cli_name()), Some(*quant));
            assert!(!quant.tradeoff_note().is_empty());
        }
    }

    #[test]
    fn passthrough_backend_only_supports_copy() {
        let dir = tempfile_dir();
        let source = dir.join("in.gguf");
        let output = dir.join("out.gguf");
        fs::write(&source, build_minimal_gguf("llama", None)).unwrap();
        let backend = PassthroughGgufRequantize;
        let error = backend
            .convert(&source, &output, GgufQuantType::Q4_K_M, false)
            .unwrap_err();
        assert!(matches!(error, QuantizationError::Unsupported(_)));

        let result = backend.convert(&source, &output, GgufQuantType::Copy, false).unwrap();
        assert!(!result.real);
        assert_eq!(fs::read(&source).unwrap(), fs::read(&output).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workbench_runs_full_conversion_with_passthrough_backend_and_persists_report() {
        let dir = tempfile_dir();
        let source_dir = dir.join("source");
        fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("model.gguf");
        fs::write(&source_path, build_minimal_gguf("llama", Some("apache-2.0"))).unwrap();

        let workbench = QuantizationWorkbench::new(
            dir.join("workbench"),
            vec![Arc::new(PassthroughGgufRequantize)],
        );
        let report = workbench
            .convert(ConversionRequest {
                source_path,
                quant_choice: GgufQuantType::Copy,
                allow_requantize: false,
                license: DeclaredLicense::SniffFromSource,
            })
            .expect("passthrough conversion must succeed");

        assert_eq!(report.source.format, SourceFormat::Gguf);
        assert_eq!(report.source.sha256.len(), 64);
        assert_eq!(report.output.sha256, report.source.sha256);
        assert!(report.eval.passed);
        assert_eq!(report.license.risk, LicenseRisk::Permissive);
        assert!(!report.tool.real);

        let report_path = workbench
            .root()
            .join(&report.conversion_id)
            .join(QUANTIZATION_REPORT_FILE);
        let persisted: ConversionReport =
            serde_json::from_slice(&fs::read(&report_path).unwrap()).expect("report.json must parse");
        assert_eq!(persisted.conversion_id, report.conversion_id);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workbench_rejects_safetensors_source_with_a_clear_unsupported_error() {
        let dir = tempfile_dir();
        let source_dir = dir.join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("model.safetensors"), build_minimal_safetensors(None)).unwrap();

        let workbench = QuantizationWorkbench::new(
            dir.join("workbench"),
            vec![Arc::new(PassthroughGgufRequantize)],
        );
        let error = workbench
            .convert(ConversionRequest {
                source_path: source_dir,
                quant_choice: GgufQuantType::Copy,
                allow_requantize: false,
                license: DeclaredLicense::SniffFromSource,
            })
            .unwrap_err();
        assert!(matches!(error, QuantizationError::Unsupported(_)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workbench_flags_output_that_fails_to_parse_as_gguf() {
        struct CorruptingBackend;
        impl QuantizationBackend for CorruptingBackend {
            fn id(&self) -> &'static str {
                "corrupting-test-backend"
            }
            fn is_available(&self) -> bool {
                true
            }
            fn supports_format(&self, format: SourceFormat) -> bool {
                matches!(format, SourceFormat::Gguf)
            }
            fn convert(
                &self,
                _source: &Path,
                output: &Path,
                _quant: GgufQuantType,
                _allow_requantize: bool,
            ) -> QuantizationResult<BackendOutput> {
                fs::write(output, b"not a gguf file").map_err(|error| io_at("write corrupt output", output, error))?;
                Ok(BackendOutput {
                    tool_name: "corrupting-test-backend".to_string(),
                    tool_version: None,
                    real: true,
                })
            }
        }

        let dir = tempfile_dir();
        let source_dir = dir.join("source");
        fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("model.gguf");
        fs::write(&source_path, build_minimal_gguf("llama", None)).unwrap();

        let workbench = QuantizationWorkbench::new(dir.join("workbench"), vec![Arc::new(CorruptingBackend)]);
        let report = workbench
            .convert(ConversionRequest {
                source_path,
                quant_choice: GgufQuantType::Q4_K_M,
                allow_requantize: false,
                license: DeclaredLicense::SniffFromSource,
            })
            .expect("conversion itself should still succeed and report the failed eval");
        assert!(!report.eval.passed);
        fs::remove_dir_all(&dir).ok();
    }

    /// A fake `llama-quantize` stand-in: a tiny shell script that mimics the
    /// real CLI's argument shape and stderr build banner, then just copies
    /// the input to the output. This exercises `LlamaCppQuantizeBackend`'s
    /// real process-invocation/arg-passing/exit-code/stderr-parsing code —
    /// NOT real quantization math, which genuinely needs the real
    /// `llama-quantize` binary and a fully valid model (see the module doc
    /// comment on why that isn't part of this automated test).
    #[cfg(unix)]
    fn write_fake_llama_quantize_script(path: &Path) {
        let script = r#"#!/bin/sh
set -e
allow_requantize=0
if [ "$1" = "--allow-requantize" ]; then
  allow_requantize=1
  shift
fi
src="$1"
dst="$2"
type="$3"
echo "llama_print_build_info: build = 4242 (deadbee)" >&2
if [ ! -f "$src" ]; then
  echo "llama_quantize: failed to quantize model from '$src'" >&2
  exit 1
fi
cp "$src" "$dst"
exit 0
"#;
        fs::write(path, script).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn llama_cpp_backend_invokes_fake_binary_and_parses_its_build_version() {
        let dir = tempfile_dir();
        let script_path = dir.join("fake-llama-quantize.sh");
        write_fake_llama_quantize_script(&script_path);

        let source = dir.join("in.gguf");
        let output = dir.join("out.gguf");
        fs::write(&source, build_minimal_gguf("llama", None)).unwrap();

        let backend = LlamaCppQuantizeBackend::from_binary(&script_path);
        assert!(backend.is_available());
        let result = backend
            .convert(&source, &output, GgufQuantType::Q4_K_M, false)
            .expect("fake backend invocation must succeed");
        assert!(result.real);
        assert_eq!(result.tool_version.as_deref(), Some("4242(deadbee)"));
        assert_eq!(fs::read(&source).unwrap(), fs::read(&output).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn llama_cpp_backend_surfaces_real_failure_from_fake_binary() {
        let dir = tempfile_dir();
        let script_path = dir.join("fake-llama-quantize.sh");
        write_fake_llama_quantize_script(&script_path);

        let missing_source = dir.join("does-not-exist.gguf");
        let output = dir.join("out.gguf");
        let backend = LlamaCppQuantizeBackend::from_binary(&script_path);
        let error = backend
            .convert(&missing_source, &output, GgufQuantType::Q4_K_M, false)
            .unwrap_err();
        assert!(matches!(error, QuantizationError::Backend(_)));
    }

    #[test]
    fn discovers_binary_from_a_synthetic_path_without_touching_real_env() {
        let dir = tempfile_dir();
        let tool_path = dir.join("my-tool");
        fs::write(&tool_path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&tool_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&tool_path, perms).unwrap();
        }
        let path_var = std::ffi::OsString::from(dir.to_string_lossy().to_string());
        let found = discover_in_path_dirs("my-tool", Some(path_var.as_os_str()));
        assert_eq!(found.as_deref(), Some(tool_path.as_path()));
        assert_eq!(discover_in_path_dirs("does-not-exist-tool", Some(path_var.as_os_str())), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_llama_quantize_binary_discovery_is_never_flaky() {
        // Whether or not this sandbox happens to have `llama-quantize`
        // installed, discovery must either return a real, executable path or
        // `None` — never panic or return a bogus path.
        if let Some(path) = find_llama_quantize_binary() {
            assert!(is_executable_file(&path), "{} must be executable", path.display());
        }
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lm-quantization-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
