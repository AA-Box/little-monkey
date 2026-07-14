//! Production adapters for Knowledge Stacks 2.0.
//!
//! The core pipeline intentionally knows nothing about container formats or
//! network runtimes.  This module supplies inert, bounded extractors for the
//! formats the desktop exposes.  Office files are decoded without invoking an
//! office suite: ZIP members are validated before inflation, XML is streamed,
//! macros/formulas/scripts are never executed, and external relationships are
//! reported but never fetched.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use flate2::read::DeflateDecoder;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::knowledge_pipeline::{
    BoundingBox, DocumentExtractor, DocumentFormat, DocumentLocation, DocumentSecurityDeclaration,
    ExtractedBlock, ExtractedDocument, ExtractionInput, OcrAssetMetadata, OcrPageInput,
    OcrProvider, PipelineError, PipelineLimits, PipelineResult, SourceObject, SourceObjectMetadata,
    EXTRACTOR_CONTRACT_VERSION,
};

const MAX_ZIP_ENTRIES: usize = 20_000;
const MAX_ZIP_MEMBER_BYTES: usize = 64 * 1024 * 1024;
const MAX_ZIP_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_COMPRESSION_RATIO: u64 = 250;

fn sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn stable_hash(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn cancelled(cancel: &CancellationToken) -> PipelineResult<()> {
    if cancel.is_cancelled() {
        Err(PipelineError::Cancelled)
    } else {
        Ok(())
    }
}

pub fn media_type_for_path(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "txt" | "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "kt" | "swift"
        | "c" | "h" | "cpp" | "hpp" | "cs" | "rb" | "php" | "sh" | "zsh" | "fish" | "sql"
        | "toml" | "yaml" | "yml" | "json" | "xml" | "css" | "scss" | "less" | "vue" | "svelte" => {
            Some("text/plain")
        }
        "md" | "markdown" => Some("text/markdown"),
        "html" | "htm" => Some("text/html"),
        "pdf" => Some("application/pdf"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "pptx" => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "tif" | "tiff" => Some("image/tiff"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

pub fn source_object_from_bytes(
    source_id: &str,
    object_id: &str,
    canonical_uri: String,
    media_type: String,
    bytes: Vec<u8>,
    etag: Option<String>,
    modified_unix_ms: Option<u64>,
) -> SourceObject {
    SourceObject {
        metadata: SourceObjectMetadata {
            source_id: source_id.to_string(),
            object_id: object_id.to_string(),
            canonical_uri,
            media_type,
            byte_len: bytes.len() as u64,
            content_sha256: sha256(&bytes),
            etag,
            modified_unix_ms,
            resolved_addresses: Vec::new(),
        },
        bytes,
    }
}

#[derive(Debug, Clone)]
struct ZipMember {
    name: String,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    local_offset: u32,
}

#[derive(Debug)]
struct SafeZip<'a> {
    bytes: &'a [u8],
    members: BTreeMap<String, ZipMember>,
}

impl<'a> SafeZip<'a> {
    fn open(bytes: &'a [u8], limits: &PipelineLimits) -> PipelineResult<Self> {
        let minimum = 22;
        if bytes.len() < minimum {
            return Err(PipelineError::InvalidExtraction(
                "Office container is not a valid ZIP archive".to_string(),
            ));
        }
        let search_start = bytes.len().saturating_sub(65_557);
        let eocd = (search_start..=bytes.len() - minimum)
            .rev()
            .find(|offset| bytes.get(*offset..*offset + 4) == Some(&[0x50, 0x4b, 0x05, 0x06]))
            .ok_or_else(|| {
                PipelineError::InvalidExtraction("Office ZIP directory is missing".to_string())
            })?;
        let disk = u16_at(bytes, eocd + 4)?;
        let directory_disk = u16_at(bytes, eocd + 6)?;
        let disk_entries = u16_at(bytes, eocd + 8)? as usize;
        let entries = u16_at(bytes, eocd + 10)? as usize;
        let directory_size = u32_at(bytes, eocd + 12)? as usize;
        let directory_offset = u32_at(bytes, eocd + 16)? as usize;
        let comment_len = u16_at(bytes, eocd + 20)? as usize;
        if disk != 0
            || directory_disk != 0
            || disk_entries != entries
            || entries > MAX_ZIP_ENTRIES
            || eocd + minimum + comment_len > bytes.len()
            || directory_offset
                .checked_add(directory_size)
                .is_none_or(|end| end > eocd)
        {
            return Err(PipelineError::UnsafeDocument(
                "multi-disk, ZIP64, oversized, or malformed Office ZIP is rejected".to_string(),
            ));
        }
        let mut cursor = directory_offset;
        let mut members = BTreeMap::new();
        let mut total_uncompressed = 0_u64;
        for _ in 0..entries {
            if bytes.get(cursor..cursor + 4) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
                return Err(PipelineError::InvalidExtraction(
                    "malformed Office ZIP central directory".to_string(),
                ));
            }
            let flags = u16_at(bytes, cursor + 8)?;
            let method = u16_at(bytes, cursor + 10)?;
            let crc32 = u32_at(bytes, cursor + 16)?;
            let compressed_size = u32_at(bytes, cursor + 20)?;
            let uncompressed_size = u32_at(bytes, cursor + 24)?;
            let name_len = u16_at(bytes, cursor + 28)? as usize;
            let extra_len = u16_at(bytes, cursor + 30)? as usize;
            let comment_len = u16_at(bytes, cursor + 32)? as usize;
            let local_offset = u32_at(bytes, cursor + 42)?;
            let header_end = cursor.checked_add(46).ok_or_else(zip_overflow)?;
            let name_end = header_end.checked_add(name_len).ok_or_else(zip_overflow)?;
            let entry_end = name_end
                .checked_add(extra_len)
                .and_then(|value| value.checked_add(comment_len))
                .ok_or_else(zip_overflow)?;
            if entry_end > bytes.len() || entry_end > directory_offset + directory_size {
                return Err(PipelineError::InvalidExtraction(
                    "Office ZIP central entry is truncated".to_string(),
                ));
            }
            if flags & 0x0001 != 0 || !matches!(method, 0 | 8) {
                return Err(PipelineError::UnsafeDocument(
                    "encrypted or unsupported Office ZIP members are rejected".to_string(),
                ));
            }
            let name = std::str::from_utf8(&bytes[header_end..name_end])
                .map_err(|_| {
                    PipelineError::InvalidExtraction(
                        "Office ZIP member name is not UTF-8".to_string(),
                    )
                })?
                .to_string();
            validate_member_name(&name)?;
            if uncompressed_size as usize > MAX_ZIP_MEMBER_BYTES
                || uncompressed_size as u64 > limits.max_file_bytes.max(MAX_ZIP_MEMBER_BYTES as u64)
                || (compressed_size == 0 && uncompressed_size != 0)
                || (compressed_size != 0
                    && u64::from(uncompressed_size)
                        > u64::from(compressed_size).saturating_mul(MAX_COMPRESSION_RATIO))
            {
                return Err(PipelineError::LimitExceeded(
                    "Office ZIP member exceeds size or compression-ratio limits".to_string(),
                ));
            }
            total_uncompressed = total_uncompressed
                .checked_add(u64::from(uncompressed_size))
                .ok_or_else(zip_overflow)?;
            if total_uncompressed > MAX_ZIP_TOTAL_BYTES as u64
                || total_uncompressed > limits.max_total_bytes
            {
                return Err(PipelineError::LimitExceeded(
                    "Office ZIP expanded size exceeds the configured limit".to_string(),
                ));
            }
            if members
                .insert(
                    name.clone(),
                    ZipMember {
                        name,
                        flags,
                        method,
                        crc32,
                        compressed_size,
                        uncompressed_size,
                        local_offset,
                    },
                )
                .is_some()
            {
                return Err(PipelineError::UnsafeDocument(
                    "duplicate Office ZIP member names are rejected".to_string(),
                ));
            }
            cursor = entry_end;
        }
        if cursor != directory_offset + directory_size {
            return Err(PipelineError::InvalidExtraction(
                "Office ZIP directory length is inconsistent".to_string(),
            ));
        }
        Ok(Self { bytes, members })
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        self.members.keys().map(String::as_str)
    }

    fn read(&self, name: &str) -> PipelineResult<Option<Vec<u8>>> {
        let Some(member) = self.members.get(name) else {
            return Ok(None);
        };
        let offset = member.local_offset as usize;
        if self.bytes.get(offset..offset + 4) != Some(&[0x50, 0x4b, 0x03, 0x04]) {
            return Err(PipelineError::InvalidExtraction(format!(
                "Office ZIP local header is missing for {}",
                member.name
            )));
        }
        let local_flags = u16_at(self.bytes, offset + 6)?;
        let local_method = u16_at(self.bytes, offset + 8)?;
        let name_len = u16_at(self.bytes, offset + 26)? as usize;
        let extra_len = u16_at(self.bytes, offset + 28)? as usize;
        if local_flags != member.flags || local_method != member.method {
            return Err(PipelineError::UnsafeDocument(
                "Office ZIP central/local metadata mismatch".to_string(),
            ));
        }
        let data_start = offset
            .checked_add(30)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .ok_or_else(zip_overflow)?;
        let data_end = data_start
            .checked_add(member.compressed_size as usize)
            .ok_or_else(zip_overflow)?;
        let compressed = self.bytes.get(data_start..data_end).ok_or_else(|| {
            PipelineError::InvalidExtraction("Office ZIP member is truncated".to_string())
        })?;
        let mut output = Vec::with_capacity(member.uncompressed_size as usize);
        match member.method {
            0 => output.extend_from_slice(compressed),
            8 => {
                let decoder = DeflateDecoder::new(Cursor::new(compressed));
                decoder
                    .take(u64::from(member.uncompressed_size) + 1)
                    .read_to_end(&mut output)?;
            }
            _ => unreachable!(),
        }
        if output.len() != member.uncompressed_size as usize
            || crc32fast::hash(&output) != member.crc32
        {
            return Err(PipelineError::InvalidExtraction(format!(
                "Office ZIP member integrity check failed: {}",
                member.name
            )));
        }
        Ok(Some(output))
    }

    fn require(&self, name: &str) -> PipelineResult<Vec<u8>> {
        self.read(name)?.ok_or_else(|| {
            PipelineError::InvalidExtraction(format!("required Office member is missing: {name}"))
        })
    }
}

fn zip_overflow() -> PipelineError {
    PipelineError::InvalidExtraction("Office ZIP offset overflow".to_string())
}

fn u16_at(bytes: &[u8], offset: usize) -> PipelineResult<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| PipelineError::InvalidExtraction("truncated ZIP integer".to_string()))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> PipelineResult<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| PipelineError::InvalidExtraction("truncated ZIP integer".to_string()))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn validate_member_name(name: &str) -> PipelineResult<()> {
    if name.is_empty()
        || name.len() > 1_024
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
        || name.split('/').any(|part| matches!(part, "" | "." | ".."))
        || name.as_bytes().get(1) == Some(&b':')
    {
        return Err(PipelineError::UnsafeDocument(
            "unsafe Office ZIP member path".to_string(),
        ));
    }
    Ok(())
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

fn attr_value(start: &BytesStart<'_>, wanted: &[u8]) -> Option<String> {
    start
        .attributes()
        .with_checks(false)
        .flatten()
        .find_map(|attribute| {
            (local_name(attribute.key.as_ref()) == wanted)
                .then(|| String::from_utf8_lossy(attribute.value.as_ref()).into_owned())
        })
}

fn external_relationships(zip: &SafeZip<'_>) -> PipelineResult<bool> {
    for name in zip.names().filter(|name| name.ends_with(".rels")) {
        if let Some(bytes) = zip.read(name)? {
            let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
            if text.contains("targetmode=\"external\"") || text.contains("targetmode='external'") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn has_macros(zip: &SafeZip<'_>) -> bool {
    zip.names().any(|name| {
        let lower = name.to_ascii_lowercase();
        lower.ends_with("vbaproject.bin") || lower.contains("/macros/")
    })
}

fn security(zip: &SafeZip<'_>) -> PipelineResult<DocumentSecurityDeclaration> {
    Ok(DocumentSecurityDeclaration {
        macros_present: has_macros(zip),
        scripts_present: false,
        external_relationships_present: external_relationships(zip)?,
        macros_executed: false,
        scripts_executed: false,
        external_resources_fetched: false,
    })
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OfficeOpenXmlExtractor;

const OFFICE_FORMATS: [DocumentFormat; 3] = [
    DocumentFormat::Docx,
    DocumentFormat::Xlsx,
    DocumentFormat::Pptx,
];

impl DocumentExtractor for OfficeOpenXmlExtractor {
    fn extractor_id(&self) -> &str {
        "builtin.office-openxml.v1"
    }

    fn formats(&self) -> &[DocumentFormat] {
        &OFFICE_FORMATS
    }

    fn extract(&self, input: ExtractionInput<'_>) -> PipelineResult<ExtractedDocument> {
        cancelled(input.cancel)?;
        let zip = SafeZip::open(&input.object.bytes, input.limits)?;
        let document_security = security(&zip)?;
        let blocks = match input.format {
            DocumentFormat::Docx => extract_docx(&zip, input.object, input.cancel)?,
            DocumentFormat::Xlsx => extract_xlsx(&zip, input.object, input.cancel)?,
            DocumentFormat::Pptx => extract_pptx(&zip, input.object, input.cancel)?,
            _ => {
                return Err(PipelineError::UnsupportedFormat(format!(
                    "{:?}",
                    input.format
                )))
            }
        };
        let document = ExtractedDocument {
            contract_version: EXTRACTOR_CONTRACT_VERSION,
            extractor_id: self.extractor_id().to_string(),
            extractor_version: "1.0.0".to_string(),
            source: input.object.metadata.clone(),
            format: input.format,
            security: document_security,
            blocks,
            warnings: Vec::new(),
        };
        document.validate(input.policy, input.limits)?;
        Ok(document)
    }
}

fn extract_docx(
    zip: &SafeZip<'_>,
    source: &SourceObject,
    cancel: &CancellationToken,
) -> PipelineResult<Vec<ExtractedBlock>> {
    let xml = zip.require("word/document.xml")?;
    let mut reader = Reader::from_reader(xml.as_slice());
    reader.config_mut().trim_text(true);
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut heading_path = Vec::<String>::new();
    let mut paragraph = 0_u32;
    let mut table = None::<u32>;
    let mut table_count = 0_u32;
    let mut row = 0_u32;
    let mut column = 0_u32;
    let mut in_paragraph = false;
    let mut in_text = false;
    let mut style = None::<String>;
    loop {
        cancelled(cancel)?;
        match reader.read_event() {
            Ok(Event::Start(start)) => match local_name(start.name().as_ref()) {
                b"tbl" => {
                    table_count += 1;
                    table = Some(table_count);
                    row = 0;
                }
                b"tr" => {
                    row += 1;
                    column = 0;
                }
                b"tc" => column += 1,
                b"p" => {
                    paragraph += 1;
                    in_paragraph = true;
                    text.clear();
                    style = None;
                }
                b"pStyle" if in_paragraph => style = attr_value(&start, b"val"),
                b"t" if in_paragraph => in_text = true,
                b"tab" if in_paragraph => text.push('\t'),
                b"br" if in_paragraph => text.push('\n'),
                _ => {}
            },
            Ok(Event::Empty(start)) => match local_name(start.name().as_ref()) {
                b"pStyle" if in_paragraph => style = attr_value(&start, b"val"),
                b"tab" if in_paragraph => text.push('\t'),
                b"br" if in_paragraph => text.push('\n'),
                _ => {}
            },
            Ok(Event::Text(value)) if in_text => {
                text.push_str(
                    &value
                        .decode()
                        .map_err(|error| PipelineError::InvalidExtraction(error.to_string()))?,
                );
            }
            Ok(Event::End(end)) => match local_name(end.name().as_ref()) {
                b"t" => in_text = false,
                b"p" => {
                    in_paragraph = false;
                    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !normalized.is_empty() {
                        if let Some(level) = style
                            .as_deref()
                            .and_then(|value| {
                                value
                                    .to_ascii_lowercase()
                                    .strip_prefix("heading")
                                    .map(str::to_string)
                            })
                            .and_then(|value| value.parse::<usize>().ok())
                            .filter(|level| (1..=9).contains(level))
                        {
                            heading_path.truncate(level.saturating_sub(1));
                            heading_path.push(normalized.clone());
                        }
                        blocks.push(ExtractedBlock {
                            block_id: stable_hash(&[
                                &source.metadata.object_id,
                                "docx",
                                &paragraph.to_string(),
                                &table.unwrap_or(0).to_string(),
                                &row.to_string(),
                                &column.to_string(),
                            ]),
                            text: normalized,
                            location: DocumentLocation::Docx {
                                section: 1,
                                paragraph,
                                table,
                                cell: table.map(|_| format!("R{row}C{column}")),
                            },
                            heading_path: heading_path.clone(),
                            content_type: if table.is_some() {
                                "table_cell".to_string()
                            } else {
                                "paragraph".to_string()
                            },
                        });
                    }
                }
                b"tbl" => table = None,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(PipelineError::InvalidExtraction(format!(
                    "invalid DOCX XML: {error}"
                )))
            }
            _ => {}
        }
    }
    Ok(blocks)
}

fn extract_shared_strings(zip: &SafeZip<'_>) -> PipelineResult<Vec<String>> {
    let Some(xml) = zip.read("xl/sharedStrings.xml")? else {
        return Ok(Vec::new());
    };
    let mut reader = Reader::from_reader(xml.as_slice());
    reader.config_mut().trim_text(true);
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_item = false;
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => match local_name(start.name().as_ref()) {
                b"si" => {
                    in_item = true;
                    current.clear();
                }
                b"t" if in_item => in_text = true,
                _ => {}
            },
            Ok(Event::Text(value)) if in_text => current.push_str(
                &value
                    .decode()
                    .map_err(|error| PipelineError::InvalidExtraction(error.to_string()))?,
            ),
            Ok(Event::End(end)) => match local_name(end.name().as_ref()) {
                b"t" => in_text = false,
                b"si" => {
                    in_item = false;
                    strings.push(current.clone());
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(PipelineError::InvalidExtraction(format!(
                    "invalid XLSX shared strings: {error}"
                )))
            }
            _ => {}
        }
    }
    Ok(strings)
}

fn workbook_sheet_names(zip: &SafeZip<'_>) -> PipelineResult<Vec<String>> {
    let Some(xml) = zip.read("xl/workbook.xml")? else {
        return Ok(Vec::new());
    };
    let mut reader = Reader::from_reader(xml.as_slice());
    let mut names = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(start) | Event::Empty(start))
                if local_name(start.name().as_ref()) == b"sheet" =>
            {
                names.push(
                    attr_value(&start, b"name")
                        .unwrap_or_else(|| format!("Sheet{}", names.len() + 1)),
                );
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(PipelineError::InvalidExtraction(format!(
                    "invalid XLSX workbook XML: {error}"
                )))
            }
            _ => {}
        }
    }
    Ok(names)
}

fn extract_xlsx(
    zip: &SafeZip<'_>,
    source: &SourceObject,
    cancel: &CancellationToken,
) -> PipelineResult<Vec<ExtractedBlock>> {
    let shared = extract_shared_strings(zip)?;
    let sheet_names = workbook_sheet_names(zip)?;
    let mut sheet_members = zip
        .names()
        .filter(|name| name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    sheet_members.sort_by_key(|name| numeric_suffix(name, "xl/worksheets/sheet", ".xml"));
    let mut blocks = Vec::new();
    for (sheet_index, member) in sheet_members.iter().enumerate() {
        cancelled(cancel)?;
        let xml = zip.require(member)?;
        let sheet = sheet_names
            .get(sheet_index)
            .cloned()
            .unwrap_or_else(|| format!("Sheet{}", sheet_index + 1));
        let mut reader = Reader::from_reader(xml.as_slice());
        reader.config_mut().trim_text(true);
        let mut cell = None::<String>;
        let mut cell_type = None::<String>;
        let mut value = String::new();
        let mut in_value = false;
        let mut in_inline = false;
        loop {
            cancelled(cancel)?;
            match reader.read_event() {
                Ok(Event::Start(start)) => match local_name(start.name().as_ref()) {
                    b"c" => {
                        cell = attr_value(&start, b"r");
                        cell_type = attr_value(&start, b"t");
                        value.clear();
                    }
                    b"v" => in_value = true,
                    b"t" if cell_type.as_deref() == Some("inlineStr") => in_inline = true,
                    _ => {}
                },
                Ok(Event::Text(text)) if in_value || in_inline => {
                    value.push_str(
                        &text
                            .decode()
                            .map_err(|error| PipelineError::InvalidExtraction(error.to_string()))?,
                    );
                }
                Ok(Event::End(end)) => match local_name(end.name().as_ref()) {
                    b"v" => in_value = false,
                    b"t" => in_inline = false,
                    b"c" => {
                        if let Some(reference) = cell.take() {
                            let rendered = if cell_type.as_deref() == Some("s") {
                                value
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|index| shared.get(index))
                                    .cloned()
                                    .unwrap_or_else(|| value.clone())
                            } else {
                                value.clone()
                            };
                            let rendered = rendered.trim();
                            if !rendered.is_empty() {
                                blocks.push(ExtractedBlock {
                                    block_id: stable_hash(&[
                                        &source.metadata.object_id,
                                        "xlsx",
                                        &sheet,
                                        &reference,
                                    ]),
                                    text: rendered.to_string(),
                                    location: DocumentLocation::Xlsx {
                                        sheet: sheet.clone(),
                                        cell_range: reference,
                                    },
                                    heading_path: vec![sheet.clone()],
                                    content_type: "cell".to_string(),
                                });
                            }
                        }
                    }
                    _ => {}
                },
                Ok(Event::Eof) => break,
                Err(error) => {
                    return Err(PipelineError::InvalidExtraction(format!(
                        "invalid XLSX worksheet XML: {error}"
                    )))
                }
                _ => {}
            }
        }
    }
    Ok(blocks)
}

fn numeric_suffix(name: &str, prefix: &str, suffix: &str) -> u32 {
    name.strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
        .and_then(|value| value.parse().ok())
        .unwrap_or(u32::MAX)
}

fn extract_pptx(
    zip: &SafeZip<'_>,
    source: &SourceObject,
    cancel: &CancellationToken,
) -> PipelineResult<Vec<ExtractedBlock>> {
    let mut members = zip
        .names()
        .filter_map(|name| {
            if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
                Some((
                    numeric_suffix(name, "ppt/slides/slide", ".xml"),
                    name.to_string(),
                    false,
                ))
            } else if name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml") {
                Some((
                    numeric_suffix(name, "ppt/notesSlides/notesSlide", ".xml"),
                    name.to_string(),
                    true,
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    members.sort_by(|left, right| left.0.cmp(&right.0).then(left.2.cmp(&right.2)));
    let mut blocks = Vec::new();
    for (slide, member, notes) in members {
        cancelled(cancel)?;
        if slide == 0 || slide == u32::MAX {
            continue;
        }
        let xml = zip.require(&member)?;
        let texts = collect_xml_text(&xml, b"t")?;
        for (ordinal, text) in texts
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .enumerate()
        {
            blocks.push(ExtractedBlock {
                block_id: stable_hash(&[
                    &source.metadata.object_id,
                    "pptx",
                    &slide.to_string(),
                    if notes { "notes" } else { "slide" },
                    &ordinal.to_string(),
                ]),
                text,
                location: DocumentLocation::Pptx {
                    slide,
                    shape: Some(if notes {
                        format!("notes-{ordinal}")
                    } else {
                        format!("shape-{ordinal}")
                    }),
                },
                heading_path: vec![format!("Slide {slide}")],
                content_type: if notes {
                    "speaker_notes".to_string()
                } else {
                    "slide_text".to_string()
                },
            });
        }
    }
    Ok(blocks)
}

fn collect_xml_text(xml: &[u8], element: &[u8]) -> PipelineResult<Vec<String>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut values = Vec::new();
    let mut current = String::new();
    let mut inside = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) if local_name(start.name().as_ref()) == element => {
                inside = true;
                current.clear();
            }
            Ok(Event::Text(text)) if inside => current.push_str(
                &text
                    .decode()
                    .map_err(|error| PipelineError::InvalidExtraction(error.to_string()))?,
            ),
            Ok(Event::End(end)) if local_name(end.name().as_ref()) == element => {
                inside = false;
                values.push(current.trim().to_string());
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(PipelineError::InvalidExtraction(format!(
                    "invalid Office XML: {error}"
                )))
            }
            _ => {}
        }
    }
    Ok(values)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HtmlPdfExtractor;

const HTML_PDF_FORMATS: [DocumentFormat; 2] = [DocumentFormat::Html, DocumentFormat::Pdf];

impl DocumentExtractor for HtmlPdfExtractor {
    fn extractor_id(&self) -> &str {
        "builtin.html-pdf.v1"
    }

    fn formats(&self) -> &[DocumentFormat] {
        &HTML_PDF_FORMATS
    }

    fn extract(&self, input: ExtractionInput<'_>) -> PipelineResult<ExtractedDocument> {
        cancelled(input.cancel)?;
        let (blocks, security, warnings) = match input.format {
            DocumentFormat::Html => extract_html(input.object, input.cancel)?,
            DocumentFormat::Pdf => extract_pdf(input.object, input.cancel)?,
            _ => unreachable!(),
        };
        let document = ExtractedDocument {
            contract_version: EXTRACTOR_CONTRACT_VERSION,
            extractor_id: self.extractor_id().to_string(),
            extractor_version: "1.0.0".to_string(),
            source: input.object.metadata.clone(),
            format: input.format,
            security,
            blocks,
            warnings,
        };
        document.validate(input.policy, input.limits)?;
        Ok(document)
    }
}

fn extract_html(
    source: &SourceObject,
    cancel: &CancellationToken,
) -> PipelineResult<(
    Vec<ExtractedBlock>,
    DocumentSecurityDeclaration,
    Vec<String>,
)> {
    let raw = std::str::from_utf8(&source.bytes)
        .map_err(|_| PipelineError::InvalidExtraction("HTML input is not UTF-8".to_string()))?;
    let document = Html::parse_document(raw);
    let selector = Selector::parse("h1,h2,h3,h4,h5,h6,p,li,pre,blockquote,td,th")
        .map_err(|error| PipelineError::InvalidExtraction(error.to_string()))?;
    let script_selector = Selector::parse("script").expect("static selector");
    let external_selector =
        Selector::parse("link[href],img[src],iframe[src],a[href]").expect("static selector");
    let scripts_present = document.select(&script_selector).next().is_some();
    let external_relationships_present = document.select(&external_selector).any(|element| {
        element
            .value()
            .attr("href")
            .or_else(|| element.value().attr("src"))
            .is_some_and(|value| value.starts_with("http://") || value.starts_with("https://"))
    });
    let mut blocks = Vec::new();
    let mut headings = Vec::<String>::new();
    let mut byte_cursor = 0_u64;
    let mut tag_ordinals = HashMap::<String, usize>::new();
    for element in document.select(&selector) {
        cancelled(cancel)?;
        let tag = element.value().name().to_ascii_lowercase();
        let text = element.text().collect::<Vec<_>>().join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        if let Some(level) = tag
            .strip_prefix('h')
            .and_then(|value| value.parse::<usize>().ok())
        {
            headings.truncate(level.saturating_sub(1));
            headings.push(text.clone());
        }
        let ordinal = tag_ordinals.entry(tag.clone()).or_default();
        *ordinal += 1;
        let start = byte_cursor;
        byte_cursor = byte_cursor.saturating_add(text.len().max(1) as u64);
        blocks.push(ExtractedBlock {
            block_id: stable_hash(&[
                &source.metadata.object_id,
                "html",
                &tag,
                &ordinal.to_string(),
            ]),
            text,
            location: DocumentLocation::Html {
                css_path: format!("{tag}:nth-of-type({ordinal})"),
                source_byte_start: start,
                source_byte_end: byte_cursor,
            },
            heading_path: headings.clone(),
            content_type: tag,
        });
        byte_cursor = byte_cursor.saturating_add(1);
    }
    Ok((
        blocks,
        DocumentSecurityDeclaration {
            macros_present: false,
            scripts_present,
            external_relationships_present,
            macros_executed: false,
            scripts_executed: false,
            external_resources_fetched: false,
        },
        if scripts_present {
            vec!["HTML scripts were discarded and not executed".to_string()]
        } else {
            Vec::new()
        },
    ))
}

#[cfg(feature = "pdf-extraction")]
fn extract_pdf(
    source: &SourceObject,
    cancel: &CancellationToken,
) -> PipelineResult<(
    Vec<ExtractedBlock>,
    DocumentSecurityDeclaration,
    Vec<String>,
)> {
    cancelled(cancel)?;
    let text = pdf_extract::extract_text_from_mem(&source.bytes).map_err(|error| {
        PipelineError::InvalidExtraction(format!("PDF text extraction failed: {error}"))
    })?;
    let pages = if text.contains('\u{000c}') {
        text.split('\u{000c}').collect::<Vec<_>>()
    } else {
        vec![text.as_str()]
    };
    let mut blocks = Vec::new();
    for (page_index, page) in pages.iter().enumerate() {
        cancelled(cancel)?;
        for (ordinal, paragraph) in page.split("\n\n").enumerate() {
            let paragraph = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
            if paragraph.is_empty() {
                continue;
            }
            blocks.push(ExtractedBlock {
                block_id: stable_hash(&[
                    &source.metadata.object_id,
                    "pdf",
                    &(page_index + 1).to_string(),
                    &ordinal.to_string(),
                ]),
                text: paragraph,
                location: DocumentLocation::Pdf {
                    page: page_index as u32 + 1,
                    bbox: None,
                },
                heading_path: vec![format!("Page {}", page_index + 1)],
                content_type: "pdf_text".to_string(),
            });
        }
    }
    let mut warnings = Vec::new();
    if blocks.is_empty() {
        warnings.push("No text layer was found; enable local OCR for this PDF".to_string());
    }
    Ok((blocks, DocumentSecurityDeclaration::inert(), warnings))
}

/// Local OCR provider backed by a verified Tesseract-compatible sidecar.
/// The executable is never discovered from `PATH`: callers must pass the
/// exact app-managed or user-approved file. Input/output live in a fresh
/// private temporary directory, execution is time-bounded and cancellable,
/// and only TSV stdout is parsed.
#[derive(Debug, Clone)]
pub struct TesseractOcrProvider {
    executable: PathBuf,
    languages: String,
    timeout: Duration,
    low_confidence_micros: u32,
}

impl TesseractOcrProvider {
    pub fn new(
        executable: impl AsRef<Path>,
        languages: &[String],
        timeout: Duration,
        low_confidence_micros: u32,
    ) -> PipelineResult<Self> {
        let executable = executable.as_ref();
        if !executable.is_absolute()
            || executable.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
        {
            return Err(PipelineError::PathRejected(
                "OCR executable path must be absolute and unambiguous".to_string(),
            ));
        }
        let metadata = fs::symlink_metadata(executable).map_err(|error| {
            PipelineError::PathRejected(format!("{}: {error}", executable.display()))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(PipelineError::PathRejected(
                "OCR executable must be a regular non-symlink file".to_string(),
            ));
        }
        if languages.is_empty()
            || languages.len() > 16
            || languages.iter().any(|language| {
                language.is_empty()
                    || language.len() > 32
                    || !language
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            || timeout < Duration::from_secs(1)
            || timeout > Duration::from_secs(600)
            || low_confidence_micros > 1_000_000
        {
            return Err(PipelineError::InvalidArgument(
                "invalid OCR language, timeout, or confidence configuration".to_string(),
            ));
        }
        Ok(Self {
            executable: fs::canonicalize(executable)?,
            languages: languages.join("+"),
            timeout,
            low_confidence_micros,
        })
    }
}

impl OcrProvider for TesseractOcrProvider {
    fn engine_id(&self) -> &str {
        "tesseract"
    }

    fn recognize_page(
        &self,
        asset: &OcrAssetMetadata,
        page: &OcrPageInput,
        cancel: &CancellationToken,
    ) -> PipelineResult<Vec<ExtractedBlock>> {
        cancelled(cancel)?;
        let extension = match page.media_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/tiff" => "tiff",
            "image/webp" => "webp",
            other => return Err(PipelineError::UnsupportedFormat(other.to_string())),
        };
        let temporary = std::env::temp_dir().join(format!(
            "little-monkey-ocr-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&temporary)?;
        let result = (|| {
            let input_path = temporary.join(format!("page-{}.{}", page.page, extension));
            let stdout_path = temporary.join("result.tsv");
            let stderr_path = temporary.join("stderr.txt");
            let mut input = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&input_path)?;
            input.write_all(&page.bytes)?;
            input.sync_all()?;
            let stdout = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&stdout_path)?;
            let stderr = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&stderr_path)?;
            let mut child = Command::new(&self.executable)
                .arg(&input_path)
                .arg("stdout")
                .arg("-l")
                .arg(&self.languages)
                .arg("tsv")
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|error| {
                    PipelineError::Provider(format!("failed to launch OCR sidecar: {error}"))
                })?;
            let started = Instant::now();
            let status = loop {
                if cancel.is_cancelled() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PipelineError::Cancelled);
                }
                if started.elapsed() > self.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PipelineError::Provider(
                        "OCR sidecar exceeded its time limit".to_string(),
                    ));
                }
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                thread::sleep(Duration::from_millis(20));
            };
            if !status.success() {
                let message = fs::read_to_string(&stderr_path)
                    .unwrap_or_default()
                    .chars()
                    .take(2_000)
                    .collect::<String>();
                return Err(PipelineError::Provider(format!(
                    "OCR sidecar exited with {status}: {message}"
                )));
            }
            let tsv = fs::read_to_string(&stdout_path)?;
            parse_tesseract_tsv(&tsv, &asset.asset_id, page.page, self.low_confidence_micros)
        })();
        let _ = fs::remove_dir_all(&temporary);
        result
    }
}

#[derive(Debug)]
struct OcrLine {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    confidence_total: u64,
    confidence_count: u64,
    words: Vec<String>,
}

fn parse_tesseract_tsv(
    tsv: &str,
    asset_id: &str,
    requested_page: u32,
    low_confidence_micros: u32,
) -> PipelineResult<Vec<ExtractedBlock>> {
    let mut lines = BTreeMap::<(u32, u32, u32, u32), OcrLine>::new();
    for (ordinal, row) in tsv.lines().enumerate() {
        if ordinal == 0 && row.starts_with("level\t") {
            continue;
        }
        let fields = row.splitn(12, '\t').collect::<Vec<_>>();
        if fields.len() != 12 {
            continue;
        }
        let level = fields[0].parse::<u32>().unwrap_or(0);
        let page = fields[1].parse::<u32>().unwrap_or(0);
        let block = fields[2].parse::<u32>().unwrap_or(0);
        let paragraph = fields[3].parse::<u32>().unwrap_or(0);
        let line = fields[4].parse::<u32>().unwrap_or(0);
        let left = fields[6].parse::<f32>().unwrap_or(0.0);
        let top = fields[7].parse::<f32>().unwrap_or(0.0);
        let width = fields[8].parse::<f32>().unwrap_or(0.0);
        let height = fields[9].parse::<f32>().unwrap_or(0.0);
        let confidence = fields[10].parse::<f32>().unwrap_or(-1.0);
        let text = fields[11].trim();
        if level != 5
            || text.is_empty()
            || page == 0
            || width <= 0.0
            || height <= 0.0
            || !confidence.is_finite()
            || confidence < 0.0
        {
            continue;
        }
        let entry = lines
            .entry((page, block, paragraph, line))
            .or_insert(OcrLine {
                left,
                top,
                right: left + width,
                bottom: top + height,
                confidence_total: 0,
                confidence_count: 0,
                words: Vec::new(),
            });
        entry.left = entry.left.min(left);
        entry.top = entry.top.min(top);
        entry.right = entry.right.max(left + width);
        entry.bottom = entry.bottom.max(top + height);
        entry.confidence_total = entry
            .confidence_total
            .saturating_add((confidence.clamp(0.0, 100.0) * 10_000.0).round() as u64);
        entry.confidence_count = entry.confidence_count.saturating_add(1);
        entry.words.push(text.to_string());
    }
    let mut blocks = Vec::new();
    for ((page, block, paragraph, line), value) in lines {
        let confidence = if value.confidence_count == 0 {
            0
        } else {
            (value.confidence_total / value.confidence_count).min(1_000_000) as u32
        };
        let text = value.words.join(" ");
        let actual_page = requested_page.saturating_add(page.saturating_sub(1));
        blocks.push(ExtractedBlock {
            block_id: stable_hash(&[
                asset_id,
                &actual_page.to_string(),
                &block.to_string(),
                &paragraph.to_string(),
                &line.to_string(),
                &text,
            ]),
            text,
            location: DocumentLocation::Ocr {
                asset_id: asset_id.to_string(),
                page: actual_page,
                bbox: BoundingBox {
                    x: value.left,
                    y: value.top,
                    width: value.right - value.left,
                    height: value.bottom - value.top,
                },
                confidence_micros: confidence,
            },
            heading_path: vec![format!("OCR page {actual_page}")],
            content_type: if confidence < low_confidence_micros {
                "ocr_low_confidence".to_string()
            } else {
                "ocr_text".to_string()
            },
        });
    }
    Ok(blocks)
}

#[cfg(not(feature = "pdf-extraction"))]
fn extract_pdf(
    _source: &SourceObject,
    _cancel: &CancellationToken,
) -> PipelineResult<(
    Vec<ExtractedBlock>,
    DocumentSecurityDeclaration,
    Vec<String>,
)> {
    Err(PipelineError::UnsupportedFormat(
        "PDF extraction was disabled at build time".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_pipeline::{
        ocr_character_accuracy_micros, run_ocr, ChunkingSpec, DocumentChunker, ExtractionPolicy,
        ExtractorRegistry, LocationAwareChunker, CHUNKER_CONTRACT_VERSION,
    };
    use base64::Engine as _;

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OcrFixtureCorpus {
        schema_version: u32,
        corpus_id: String,
        minimum_character_accuracy_micros: u32,
        low_confidence_threshold_micros: u32,
        live_image: OcrLiveImage,
        cases: Vec<OcrFixtureCase>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OcrLiveImage {
        media_type: String,
        requested_page: u32,
        expected_text: String,
        base64_fixture: String,
        sha256: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OcrFixtureCase {
        case_id: String,
        asset_id: String,
        requested_page: u32,
        expected_page: u32,
        expected_text: String,
        expected_low_confidence: bool,
        tsv: String,
    }

    fn ocr_fixture_corpus() -> OcrFixtureCorpus {
        serde_json::from_str(include_str!("../fixtures/knowledge-v2/ocr-corpus-v1.json"))
            .expect("parse maintained OCR fixture corpus")
    }

    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let offset = bytes.len() as u32;
            let crc = crc32fast::hash(body);
            bytes.extend_from_slice(&0x04034b50_u32.to_le_bytes());
            bytes.extend_from_slice(&20_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&crc.to_le_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(body);

            central.extend_from_slice(&0x02014b50_u32.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let directory_offset = bytes.len() as u32;
        let directory_size = central.len() as u32;
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(&0x06054b50_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&directory_size.to_le_bytes());
        bytes.extend_from_slice(&directory_offset.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes
    }

    fn object(media_type: &str, bytes: Vec<u8>) -> SourceObject {
        source_object_from_bytes(
            "source-1",
            "object-1",
            "file:///fixture".to_string(),
            media_type.to_string(),
            bytes,
            None,
            None,
        )
    }

    #[test]
    fn docx_preserves_heading_and_table_cell_locations() {
        let bytes = stored_zip(&[(
            "word/document.xml",
            br#"<w:document xmlns:w="w"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell value</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#,
        )]);
        let object = object(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            bytes,
        );
        let mut registry = ExtractorRegistry::default();
        registry.register(Box::new(OfficeOpenXmlExtractor)).unwrap();
        let document = registry
            .extract(
                &object,
                &ExtractionPolicy::default(),
                &PipelineLimits::default(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(document.blocks.len(), 2);
        assert_eq!(document.blocks[0].heading_path, vec!["Title"]);
        assert!(matches!(
            document.blocks[1].location,
            DocumentLocation::Docx {
                table: Some(1),
                cell: Some(ref cell),
                ..
            } if cell == "R1C1"
        ));
    }

    #[test]
    fn xlsx_preserves_sheet_and_cell() {
        let bytes = stored_zip(&[
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Budget"/></sheets></workbook>"#,
            ),
            (
                "xl/sharedStrings.xml",
                br#"<sst><si><t>Hello</t></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet><sheetData><row><c r="B2" t="s"><v>0</v></c></row></sheetData></worksheet>"#,
            ),
        ]);
        let object = object(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            bytes,
        );
        let document = OfficeOpenXmlExtractor
            .extract(ExtractionInput {
                object: &object,
                format: DocumentFormat::Xlsx,
                policy: &ExtractionPolicy::default(),
                limits: &PipelineLimits::default(),
                cancel: &CancellationToken::new(),
            })
            .unwrap();
        assert_eq!(document.blocks[0].text, "Hello");
        assert!(matches!(
            &document.blocks[0].location,
            DocumentLocation::Xlsx { sheet, cell_range } if sheet == "Budget" && cell_range == "B2"
        ));
    }

    #[test]
    fn rejects_zip_slip_before_extraction() {
        let bytes = stored_zip(&[("../word/document.xml", b"bad")]);
        let error = SafeZip::open(&bytes, &PipelineLimits::default()).unwrap_err();
        assert!(matches!(error, PipelineError::UnsafeDocument(_)));
    }

    #[test]
    fn html_discards_scripts_and_preserves_sections() {
        let object = object(
            "text/html",
            b"<h1>Guide</h1><p>Safe text</p><script>steal()</script>".to_vec(),
        );
        let document = HtmlPdfExtractor
            .extract(ExtractionInput {
                object: &object,
                format: DocumentFormat::Html,
                policy: &ExtractionPolicy::default(),
                limits: &PipelineLimits::default(),
                cancel: &CancellationToken::new(),
            })
            .unwrap();
        assert!(document.security.scripts_present);
        assert_eq!(document.blocks.len(), 2);
        assert_eq!(document.blocks[1].heading_path, vec!["Guide"]);
        assert!(!document
            .blocks
            .iter()
            .any(|block| block.text.contains("steal")));
    }

    #[test]
    fn tesseract_tsv_marks_low_confidence_and_keeps_page_boxes() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
5\t1\t1\t1\t1\t1\t10\t20\t30\t10\t92.5\tClear\n\
5\t1\t1\t1\t1\t2\t45\t20\t25\t10\t50.0\tmaybe\n";
        let blocks = parse_tesseract_tsv(tsv, "ocr-asset", 3, 800_000).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Clear maybe");
        assert_eq!(blocks[0].content_type, "ocr_low_confidence");
        assert!(matches!(
            blocks[0].location,
            DocumentLocation::Ocr {
                page: 3,
                confidence_micros: 712_500,
                ..
            }
        ));
    }

    #[test]
    fn maintained_ocr_corpus_meets_accuracy_citation_and_confidence_gates() {
        let corpus = ocr_fixture_corpus();
        assert_eq!(corpus.schema_version, 1);
        assert_eq!(corpus.corpus_id, "little-monkey-ocr-acceptance-v1");
        assert_eq!(corpus.minimum_character_accuracy_micros, 900_000);
        assert!(corpus.cases.len() >= 3);
        let limits = PipelineLimits::default();
        let chunking = ChunkingSpec {
            strategy_version: CHUNKER_CONTRACT_VERSION,
            target_chars: 512,
            overlap_chars: 32,
            min_chars: 8,
        };
        let mut accuracy_total = 0_u64;

        for case in &corpus.cases {
            let blocks = parse_tesseract_tsv(
                &case.tsv,
                &case.asset_id,
                case.requested_page,
                corpus.low_confidence_threshold_micros,
            )
            .unwrap_or_else(|error| panic!("{} TSV must parse: {error}", case.case_id));
            assert_eq!(blocks.len(), 1, "{} block count", case.case_id);
            let block = &blocks[0];
            let accuracy = ocr_character_accuracy_micros(&case.expected_text, &block.text)
                .unwrap_or_else(|error| panic!("{} accuracy must compute: {error}", case.case_id));
            assert!(
                accuracy >= corpus.minimum_character_accuracy_micros,
                "{} OCR accuracy was {accuracy}, expected at least {} (observed {:?})",
                case.case_id,
                corpus.minimum_character_accuracy_micros,
                block.text
            );
            accuracy_total += u64::from(accuracy);
            assert_eq!(
                block.content_type == "ocr_low_confidence",
                case.expected_low_confidence,
                "{} low-confidence marker",
                case.case_id
            );
            assert!(matches!(
                &block.location,
                DocumentLocation::Ocr { asset_id, page, .. }
                    if asset_id == &case.asset_id && *page == case.expected_page
            ));

            let source = source_object_from_bytes(
                "source:ocr-acceptance",
                &case.case_id,
                format!("file:///fixtures/knowledge-v2/{}.png", case.case_id),
                "image/png".to_string(),
                case.case_id.as_bytes().to_vec(),
                None,
                None,
            );
            let document = ExtractedDocument {
                contract_version: EXTRACTOR_CONTRACT_VERSION,
                extractor_id: "fixture.ocr-evaluation.v1".to_string(),
                extractor_version: "1.0.0".to_string(),
                source: source.metadata,
                format: DocumentFormat::ImageOcr,
                security: DocumentSecurityDeclaration::inert(),
                blocks: blocks.clone(),
                warnings: Vec::new(),
            };
            let chunks = LocationAwareChunker
                .chunk(&document, &chunking, &limits, &CancellationToken::new())
                .unwrap_or_else(|error| panic!("{} must chunk: {error}", case.case_id));
            assert_eq!(chunks.len(), 1, "{} chunk count", case.case_id);
            assert_eq!(chunks[0].citation.location, block.location);
            assert_eq!(
                chunks[0].citation.canonical_uri,
                document.source.canonical_uri
            );
            assert!(matches!(
                &chunks[0].citation.location,
                DocumentLocation::Ocr { page, .. } if *page == case.expected_page
            ));
        }

        let mean_accuracy = accuracy_total / corpus.cases.len() as u64;
        assert!(
            mean_accuracy >= u64::from(corpus.minimum_character_accuracy_micros),
            "maintained OCR mean accuracy was {mean_accuracy}"
        );
    }

    #[test]
    #[ignore = "requires LITTLE_MONKEY_TESSERACT_PATH to name a regular Tesseract executable"]
    fn opt_in_real_tesseract_meets_checked_in_accuracy_gate() {
        let Some(executable) = std::env::var_os("LITTLE_MONKEY_TESSERACT_PATH") else {
            eprintln!("LITTLE_MONKEY_TESSERACT_PATH is not set; skipping opt-in OCR run");
            return;
        };
        let corpus = ocr_fixture_corpus();
        assert_eq!(corpus.live_image.base64_fixture, "ocr-live-image-v1.b64");
        let image = base64::engine::general_purpose::STANDARD
            .decode(include_str!("../fixtures/knowledge-v2/ocr-live-image-v1.b64").trim())
            .expect("decode checked-in OCR image");
        assert_eq!(sha256(&image), corpus.live_image.sha256);
        let languages = vec!["eng".to_string()];
        let provider = TesseractOcrProvider::new(
            PathBuf::from(&executable),
            &languages,
            Duration::from_secs(30),
            corpus.low_confidence_threshold_micros,
        )
        .expect("configure opt-in Tesseract provider");
        let executable_bytes = fs::read(PathBuf::from(&executable))
            .expect("read exact opt-in Tesseract executable for fixture provenance");
        let asset = OcrAssetMetadata {
            asset_id: "ocr:live-tesseract-acceptance".to_string(),
            sha256: sha256(&executable_bytes),
            engine: "tesseract".to_string(),
            engine_version: "external-opt-in".to_string(),
            languages,
            license: "user-provided executable".to_string(),
            provenance: "LITTLE_MONKEY_TESSERACT_PATH opt-in acceptance run".to_string(),
        };
        let pages = vec![OcrPageInput {
            page: corpus.live_image.requested_page,
            media_type: corpus.live_image.media_type.clone(),
            bytes: image,
        }];
        let mut progress = Vec::new();
        let blocks = run_ocr(
            &provider,
            &asset,
            &pages,
            &PipelineLimits::default(),
            &CancellationToken::new(),
            &mut |event| progress.push(event),
        )
        .expect("run real opt-in Tesseract fixture");
        let observed = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let accuracy = ocr_character_accuracy_micros(&corpus.live_image.expected_text, &observed)
            .expect("compute live OCR accuracy");
        assert!(
            accuracy >= corpus.minimum_character_accuracy_micros,
            "real Tesseract OCR accuracy was {accuracy}, observed {observed:?}"
        );
        assert!(!blocks.is_empty());
        assert!(blocks.iter().all(|block| matches!(
            block.location,
            DocumentLocation::Ocr { page, .. } if page == corpus.live_image.requested_page
        )));
        assert!(progress
            .last()
            .is_some_and(|event| event.phase == crate::knowledge_pipeline::OcrPhase::Complete));
    }
}
