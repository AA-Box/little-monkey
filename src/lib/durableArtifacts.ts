import { invoke } from "@tauri-apps/api/core";

/** Content-addressed blob metadata returned only after the Rust store has
 * verified size and SHA-256 content. */
export interface DurableArtifactBlob {
  id: string;
  size: number;
}

export interface DurableArtifactContent {
  blob: DurableArtifactBlob;
  contentBase64: string;
}

export interface DurableArtifactIntegrityIssue {
  path: string;
  blobId: string | null;
  message: string;
}

export interface DurableArtifactIntegrityReport {
  checkedBlobs: number;
  validBlobs: number;
  validBytes: number;
  issues: DurableArtifactIntegrityIssue[];
}

export function importDurableArtifactFile(path: string): Promise<DurableArtifactBlob> {
  return invoke<DurableArtifactBlob>("artifact_blob_import_file", { path });
}

export function putDurableArtifactBase64(contentBase64: string): Promise<DurableArtifactBlob> {
  return invoke<DurableArtifactBlob>("artifact_blob_put_base64", { contentBase64 });
}

export function readDurableArtifact(id: string): Promise<DurableArtifactContent> {
  return invoke<DurableArtifactContent>("artifact_blob_read_base64", { id });
}

export function durableArtifactExists(id: string): Promise<boolean> {
  return invoke<boolean>("artifact_blob_exists", { id });
}

export function scanDurableArtifactIntegrity(): Promise<DurableArtifactIntegrityReport> {
  return invoke<DurableArtifactIntegrityReport>("artifact_blob_scan_integrity");
}

/** Build a model/UI data URL only at the edge. Persist the content hash, not
 * this repeatedly expanded base64 representation. */
export function artifactDataUrl(mediaType: string, contentBase64: string): string {
  const normalized = mediaType.trim().toLowerCase();
  if (!/^[a-z0-9][a-z0-9!#$&^_.+-]*\/[a-z0-9][a-z0-9!#$&^_.+-]*$/.test(normalized)) {
    throw new TypeError("Invalid artifact media type");
  }
  if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(contentBase64)) {
    throw new TypeError("Invalid artifact base64 content");
  }
  return `data:${normalized};base64,${contentBase64}`;
}
