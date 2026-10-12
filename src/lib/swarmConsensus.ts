/**
 * Strictly non-authoritative inter-agent consensus summary.
 *
 * Untrusted model reports can be parsed into independent reviewer votes; the
 * only success state is "reviewer_agreement", NEVER "verified". Verification
 * still requires deterministic tests and source inspection. Missing,
 * malformed or conflicting reports cannot silently become consensus.
 */
import { unwrapUntrustedContent } from './untrustedContent';

export type ClaimVote = 'support' | 'reject' | 'unknown';
export interface SwarmClaimVote {
  id: string;
  verdict: ClaimVote;
  evidence: string[];
}
export interface SwarmReviewerReport {
  reviewer: string;
  claims: SwarmClaimVote[];
}
export interface SwarmConsensusClaim {
  id: string;
  status: 'reviewer_agreement' | 'disputed' | 'insufficient_evidence';
  supportingReviewers: string[];
  rejectingReviewers: string[];
  evidence: string[];
}
export interface SwarmConsensusResult {
  status: 'reviewer_agreement' | 'disputed' | 'inconclusive';
  parsedReviewerCount: number;
  claims: SwarmConsensusClaim[];
}

const ID = /^[a-zA-Z0-9_./:#-]{1,120}$/;
const MAX_REPORT_CHARS = 8_000;
const MAX_CLAIMS = 12;

export function parseSwarmReviewerReport(reviewer: string, raw: string): SwarmReviewerReport | null {
  if (!reviewer.trim() || raw.length > MAX_REPORT_CHARS) return null;
  const content = unwrapUntrustedContent(raw).trim();
  const fenced = content.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/);
  const source = fenced ? fenced[1] : content;
  let parsed: unknown;
  try { parsed = JSON.parse(source); } catch { return null; }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
  const claims = (parsed as { claims?: unknown }).claims;
  if (!Array.isArray(claims) || !claims.length || claims.length > MAX_CLAIMS) return null;
  const normalized: SwarmClaimVote[] = [];
  const ids = new Set<string>();
  for (const rawClaim of claims) {
    if (!rawClaim || typeof rawClaim !== 'object' || Array.isArray(rawClaim)) return null;
    const value = rawClaim as Record<string, unknown>;
    if (typeof value.id !== 'string' || !ID.test(value.id) || value.id.includes('..') || value.id.startsWith('/') ||
      !['support', 'reject', 'unknown'].includes(String(value.verdict)) ||
      !Array.isArray(value.evidence) || value.evidence.length > 4 ||
      !value.evidence.every((entry: unknown) => typeof entry === 'string' && entry.length <= 250)) return null;
    if (ids.has(value.id)) return null;
    ids.add(value.id);
    normalized.push({ id: value.id, verdict: value.verdict as ClaimVote, evidence: [...value.evidence] as string[] });
  }
  return { reviewer: reviewer.slice(0, 120), claims: normalized };
}

/** A claim counts as reviewer agreement only if two independent reviewers
 * explicitly support it without contradicting votes AND cite evidence.
 * A statement in a model report is never elevated to verified truth. */
export function reconcileSwarmReviewers(
  reports: readonly { reviewer: string; report: string }[],
): SwarmConsensusResult {
  const parsed = reports.flatMap(item => {
    const report = parseSwarmReviewerReport(item.reviewer, item.report);
    return report ? [report] : [];
  });
  const claims = new Map<string, { support: Set<string>; reject: Set<string>; evidence: Set<string> }>();
  for (const reviewer of parsed) {
    for (const vote of reviewer.claims) {
      const entry = claims.get(vote.id) ?? { support: new Set<string>(), reject: new Set<string>(), evidence: new Set<string>() };
      if (vote.verdict === 'support') {
        entry.support.add(reviewer.reviewer);
        vote.evidence.filter(Boolean).forEach(e => entry.evidence.add(e));
      }
      if (vote.verdict === 'reject') entry.reject.add(reviewer.reviewer);
      claims.set(vote.id, entry);
    }
  }
  const result: SwarmConsensusClaim[] = [...claims].sort(([a], [b]) => a.localeCompare(b)).map(([id, entry]) => ({
    id,
    status: entry.reject.size && entry.support.size
      ? 'disputed'
      : entry.support.size >= 2 && entry.reject.size === 0 && entry.evidence.size > 0
        ? 'reviewer_agreement' : 'insufficient_evidence',
    supportingReviewers: [...entry.support],
    rejectingReviewers: [...entry.reject],
    evidence: [...entry.evidence].slice(0, 8),
  }));
  return {
    status: result.some(claim => claim.status === 'disputed') ? 'disputed' :
      result.length > 0 && result.every(claim => claim.status === 'reviewer_agreement')
        ? 'reviewer_agreement' : 'inconclusive',
    parsedReviewerCount: parsed.length,
    claims: result.slice(0, MAX_CLAIMS),
  };
}
