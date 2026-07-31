import { invoke } from "@tauri-apps/api/core";
import type { DurableArtifactBlob } from "./durableArtifacts";

export interface BrowserLimits {
  timeoutMs: number;
  maxSessionMs: number;
  maxActions: number;
  maxDomBytes: number;
  maxScreenshotBytes: number;
  maxLogEntries: number;
  maxDiskBytes: number;
}

export interface BrowserSessionView {
  sessionId: string;
  runId: string;
  currentUrl: string;
  startedAtMs: number;
  actionCount: number;
  cancelled: boolean;
  viewport: BrowserViewport;
}

export interface BrowserViewport {
  width: number;
  height: number;
  deviceScaleFactor: number;
  mobile: boolean;
}

export interface BrowserEvidence {
  screenshot: DurableArtifactBlob | null;
  dom: DurableArtifactBlob | null;
  accessibility: DurableArtifactBlob | null;
  console: DurableArtifactBlob | null;
  network: DurableArtifactBlob | null;
  performance: DurableArtifactBlob | null;
  actionCount: number;
}

export interface BrowserActionResult {
  ok: boolean;
  url: string;
  evidence: BrowserEvidence;
}

export interface BrowserInspection {
  url: string;
  title: string;
  dom: DurableArtifactBlob;
  accessibility: DurableArtifactBlob;
  accessibilityIssues: string[];
}

export interface BrowserAnnotation {
  url: string;
  selector: string;
  tag: string;
  role: string;
  ariaLabel: string;
  text: string;
  rect: { x: number; y: number; width: number; height: number };
  evidence: BrowserEvidence;
}

export const DEFAULT_BROWSER_LIMITS: BrowserLimits = {
  timeoutMs: 60_000,
  maxSessionMs: 10 * 60_000,
  maxActions: 100,
  maxDomBytes: 4 * 1024 * 1024,
  maxScreenshotBytes: 12 * 1024 * 1024,
  maxLogEntries: 2_000,
  maxDiskBytes: 256 * 1024 * 1024,
};

export function exactBrowserOrigin(value: string): string {
  const url = new URL(value);
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new TypeError("Only http: and https: URLs are supported");
  }
  if (url.username || url.password) throw new TypeError("Browser URLs cannot contain credentials");
  return url.origin;
}

export function isLoopbackBrowserUrl(value: string): boolean {
  const host = new URL(value).hostname.toLowerCase();
  return host === "localhost" || host === "127.0.0.1" || host === "[::1]" || host === "::1";
}

export function startBrowserSession(input: {
  runId: string;
  url: string;
  allowLoopback: boolean;
  limits?: BrowserLimits;
}): Promise<BrowserSessionView> {
  return invoke("browser_start", {
    request: {
      runId: input.runId,
      url: input.url,
      grant: { allowedOrigins: [exactBrowserOrigin(input.url)], allowLoopback: input.allowLoopback },
      limits: input.limits ?? DEFAULT_BROWSER_LIMITS,
    },
  });
}

export function listBrowserSessions(): Promise<BrowserSessionView[]> {
  return invoke("browser_list");
}

export function navigateBrowser(sessionId: string, url: string): Promise<BrowserActionResult> {
  return invoke("browser_navigate", { sessionId, url });
}

export function reloadBrowser(sessionId: string): Promise<BrowserActionResult> {
  return invoke("browser_reload", { sessionId });
}

export function setBrowserViewport(sessionId: string, viewport: BrowserViewport): Promise<BrowserActionResult> {
  return invoke("browser_set_viewport", { sessionId, viewport });
}

export function inspectBrowser(sessionId: string): Promise<BrowserInspection> {
  return invoke("browser_inspect", { sessionId });
}

export function annotateBrowser(sessionId: string, selector: string): Promise<BrowserAnnotation> {
  return invoke("browser_annotate", { sessionId, selector });
}

export function clickBrowser(sessionId: string, selector: string): Promise<BrowserActionResult> {
  return invoke("browser_click", { sessionId, selector });
}

export function typeBrowserText(sessionId: string, selector: string, text: string): Promise<BrowserActionResult> {
  return invoke("browser_type_text", { sessionId, selector, text });
}

export function scrollBrowser(sessionId: string, x: number, y: number): Promise<BrowserActionResult> {
  return invoke("browser_scroll", { sessionId, x, y });
}

export function captureBrowserEvidence(sessionId: string): Promise<BrowserEvidence> {
  return invoke("browser_capture_evidence", { sessionId });
}

export function stopBrowserSession(sessionId: string): Promise<void> {
  return invoke("browser_stop", { sessionId });
}
