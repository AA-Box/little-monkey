/**
 * Knowledge Graph Explorer (ROADMAP.md Phase 7, item 10) — pure entity/
 * relationship extraction and graph-query logic, built entirely from data
 * already available locally: Knowledge 2.0 stack retrieval hits
 * (`knowledgeV2Store.ts`'s `query`) and the current chat session transcript
 * (`sessionStore.ts`). No new backend primitive is added — the caller
 * (`knowledgeGraphStore.ts`) gathers source text via those existing stores
 * and a one-shot local-model call (the SAME dependency-injected `callModel`
 * shape `riskJudge.ts`'s `classifyToolCall` uses, wired to
 * `turnEngine.ts`'s `attemptStream` against `agentLoop.ts`'s `resolveTarget`
 * — see that store for the wiring), then hands the result to this module's
 * pure functions.
 *
 * Every extracted edge is required (by the extraction prompt, and re-checked
 * defensively while parsing) to cite at least one tagged source span from
 * the batch it came from — that citation is preserved end-to-end as
 * `EvidenceSpan[]`, which is what `answerRelationQuery` surfaces as the
 * "evidence behind the answer" the acceptance criterion asks for.
 */
import type { ChatMessage } from './llamaClient';
import { parseModelJsonCandidates } from './modelJson';

export type GraphNodeKind = 'person' | 'file' | 'decision' | 'term' | 'other';
export type GraphRelation = 'mentions' | 'relates_to' | 'depends_on' | 'owns' | 'conflicts_with';

const NODE_KINDS: readonly GraphNodeKind[] = ['person', 'file', 'decision', 'term', 'other'];
const RELATIONS: readonly GraphRelation[] = ['mentions', 'relates_to', 'depends_on', 'owns', 'conflicts_with'];

/** One quoted, source-attributed span of evidence backing a node or edge —
 * shown verbatim in the Knowledge Graph Explorer's evidence side panel. */
export interface EvidenceSpan {
  sourceType: 'knowledge_stack' | 'chat_session';
  sourceId: string;
  sourceLabel: string;
  quote: string;
  /** Human-readable pointer to where in the source this came from — a
   * `canonical_uri#char_start-char_end` for a knowledge-stack chunk, or
   * `message #<index> (<role>)` for a chat transcript turn. Not machine-
   * navigable (no in-app "jump to" exists yet for either source), but
   * enough for a user to go verify the claim themselves. */
  locator: string;
}

export interface GraphNode {
  id: string;
  label: string;
  kind: GraphNodeKind;
  /** How many source batches mentioned this entity — a crude "how central
   * is this node" signal surfaced in the panel, not a real centrality score. */
  mentions: number;
}

export interface GraphEdge {
  id: string;
  source: string;
  target: string;
  relation: GraphRelation;
  evidence: EvidenceSpan[];
}

export interface KnowledgeGraph {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

export function emptyGraph(): KnowledgeGraph {
  return { nodes: [], edges: [] };
}

/** One tagged source span offered to the extraction model, addressable by a
 * short per-batch marker (`S1`, `S2`, …) the model must cite back as
 * evidence — this is what turns "the model said so" into "the model cited
 * span S3, which is this exact quote from this exact source". */
export interface SourceSpanCandidate {
  marker: string;
  quote: string;
  locator: string;
}

/** A bounded batch of source text sent to the model in one extraction call.
 * Kept small (see `chunkIntoSourceBatches`) so one huge source never blows
 * up a single request, and so a slow/failed batch never sinks the whole
 * build — `buildKnowledgeGraph` processes batches independently. */
export interface SourceBatch {
  id: string;
  sourceType: 'knowledge_stack' | 'chat_session';
  sourceId: string;
  sourceLabel: string;
  spans: SourceSpanCandidate[];
}

const MAX_QUOTE_CHARS = 600;

function truncate(text: string, max: number): string {
  const collapsed = text.replace(/\s+/g, ' ').trim();
  return collapsed.length > max ? `${collapsed.slice(0, max)}…` : collapsed;
}

/**
 * Splits a flat list of `(quote, locator)` pairs from one source into one or
 * more `SourceBatch`es, each under `maxBatchChars` of quoted text and
 * `maxSpansPerBatch` spans — bounding both the prompt size and the number of
 * distinct extraction calls a build issues. Marker ids (`S1`, `S2`, …) reset
 * per batch since they only need to be unique WITHIN the one extraction call
 * that references them.
 */
export function chunkIntoSourceBatches(
  sourceType: SourceBatch['sourceType'],
  sourceId: string,
  sourceLabel: string,
  rawSpans: Array<{ quote: string; locator: string }>,
  idPrefix: string,
  maxBatchChars = 3500,
  maxSpansPerBatch = 10,
): SourceBatch[] {
  const batches: SourceBatch[] = [];
  let current: SourceSpanCandidate[] = [];
  let currentChars = 0;
  let batchIndex = 0;

  const flush = () => {
    if (current.length === 0) return;
    batches.push({
      id: `${idPrefix}-${batchIndex}`,
      sourceType,
      sourceId,
      sourceLabel,
      spans: current,
    });
    batchIndex += 1;
    current = [];
    currentChars = 0;
  };

  for (const raw of rawSpans) {
    const quote = truncate(raw.quote, MAX_QUOTE_CHARS);
    if (!quote) continue;
    if (current.length >= maxSpansPerBatch || (current.length > 0 && currentChars + quote.length > maxBatchChars)) {
      flush();
    }
    current.push({ marker: `S${current.length + 1}`, quote, locator: raw.locator });
    currentChars += quote.length;
  }
  flush();

  return batches;
}

function buildExtractionMessages(batch: SourceBatch): ChatMessage[] {
  const spansText = batch.spans.map((span) => `[${span.marker}] ${span.quote}`).join('\n\n');
  return [
    {
      role: 'system',
      content:
        'You are a knowledge-graph extraction assistant, running as a strict, non-conversational extractor over a small batch of tagged source spans from a local knowledge base. ' +
        'Identify entities — people, files/paths, decisions, and terms/concepts — and relationships between them, using ONLY facts stated in the spans below; never invent an entity or relationship the spans do not support. ' +
        'Every edge you output MUST cite at least one span marker (e.g. "S1") whose text actually supports it. ' +
        'Reply with ONLY a single-line JSON object of the exact shape ' +
        '{"nodes":[{"id":"short-lowercase-hyphenated-id","label":"Display label","kind":"person|file|decision|term|other"}],' +
        '"edges":[{"source":"id","target":"id","relation":"mentions|relates_to|depends_on|owns|conflicts_with","evidence":["S1"]}]} ' +
        '— ids must be unique within this reply and reused consistently for the same entity across nodes/edges; no markdown, no other text.',
    },
    {
      role: 'user',
      content: `Source: ${batch.sourceLabel} (${batch.sourceType})\n\n${spansText}`,
    },
  ];
}

interface RawExtractedNode {
  id: string;
  label: string;
  kind: GraphNodeKind;
}
interface RawExtractedEdge {
  source: string;
  target: string;
  relation: GraphRelation;
  evidence: string[];
}
interface RawExtraction {
  nodes: RawExtractedNode[];
  edges: RawExtractedEdge[];
}

/**
 * Strict parse of one batch's extraction reply — anything not matching the
 * `{nodes:[...], edges:[...]}` shape (malformed JSON, wrong field types, an
 * out-of-enum `kind`/`relation`) drops just that malformed item rather than
 * failing the whole batch, except when the top-level shape itself is wrong,
 * which returns `null` (fails that batch closed — see `buildKnowledgeGraph`,
 * which records it as a batch error and moves on rather than aborting the
 * whole build). JSON transport is shared with `riskJudge.ts`: raw replies,
 * fenced JSON, and complete objects in surrounding prose are accepted.
 */
export function parseExtractionResponse(content: string): RawExtraction | null {
  for (const parsed of parseModelJsonCandidates(content, 'object')) {
    const rawNodes = parsed.nodes;
    const rawEdges = parsed.edges;
    if (!Array.isArray(rawNodes) || !Array.isArray(rawEdges)) continue;

    const nodes: RawExtractedNode[] = [];
    for (const candidateNode of rawNodes) {
      if (!candidateNode || typeof candidateNode !== 'object') continue;
      const id = (candidateNode as { id?: unknown }).id;
      const label = (candidateNode as { label?: unknown }).label;
      const kind = (candidateNode as { kind?: unknown }).kind;
      if (typeof id !== 'string' || !id.trim() || typeof label !== 'string' || !label.trim()) continue;
      nodes.push({ id: id.trim(), label: label.trim(), kind: NODE_KINDS.includes(kind as GraphNodeKind) ? (kind as GraphNodeKind) : 'other' });
    }

    const knownIds = new Set(nodes.map((n) => n.id));
    const edges: RawExtractedEdge[] = [];
    for (const candidateEdge of rawEdges) {
      if (!candidateEdge || typeof candidateEdge !== 'object') continue;
      const source = (candidateEdge as { source?: unknown }).source;
      const target = (candidateEdge as { target?: unknown }).target;
      const relation = (candidateEdge as { relation?: unknown }).relation;
      const evidence = (candidateEdge as { evidence?: unknown }).evidence;
      if (typeof source !== 'string' || typeof target !== 'string' || !knownIds.has(source) || !knownIds.has(target)) continue;
      if (!RELATIONS.includes(relation as GraphRelation)) continue;
      if (!Array.isArray(evidence) || evidence.length === 0) continue;
      const markers = evidence.filter((m): m is string => typeof m === 'string');
      if (markers.length === 0) continue;
      edges.push({ source, target, relation: relation as GraphRelation, evidence: markers });
    }

    return { nodes, edges };
  }
  return null;
}

/** Normalizes a display label into the id merge nodes from different
 * batches are keyed by — case/whitespace-insensitive so "Alice", "alice",
 * and " Alice " all collapse onto one node instead of three. */
export function slugify(label: string): string {
  return label
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '') || 'entity';
}

function edgeKey(source: string, relation: GraphRelation, target: string): string {
  return `${source} ${relation} ${target}`;
}

/** Minimal shape `buildKnowledgeGraph` needs from a model call — the SAME
 * subset `riskJudge.ts`'s `JudgeCallResult` uses, so both modules can share
 * a single `callModel` closure built once per turn/build. */
export interface GraphCallResult {
  content: string;
  streamError: string | null;
}

export interface BuildGraphResult extends KnowledgeGraph {
  /** One entry per batch that errored (streamError) or came back
   * unparseable — surfaced in the panel so a partial build is never
   * silently indistinguishable from a complete one. */
  batchErrors: string[];
}

/**
 * Extracts and merges a graph from every batch, one one-shot non-streaming
 * `callModel` call per batch. A batch failing (stream error, unparseable
 * reply) is recorded in `batchErrors` and skipped — it never aborts the
 * rest of the build, since batches are independent by construction.
 */
export async function buildKnowledgeGraph(
  batches: SourceBatch[],
  callModel: (messages: ChatMessage[], signal?: AbortSignal) => Promise<GraphCallResult>,
  signal?: AbortSignal,
): Promise<BuildGraphResult> {
  const nodesById = new Map<string, GraphNode>();
  const edgesByKey = new Map<string, GraphEdge>();
  const batchErrors: string[] = [];

  for (const batch of batches) {
    if (signal?.aborted) break;
    let result: GraphCallResult;
    try {
      result = await callModel(buildExtractionMessages(batch), signal);
    } catch (err) {
      batchErrors.push(`${batch.sourceLabel}: ${err instanceof Error ? err.message : String(err)}`);
      continue;
    }
    if (result.streamError) {
      batchErrors.push(`${batch.sourceLabel}: ${result.streamError}`);
      continue;
    }
    const parsed = parseExtractionResponse(result.content);
    if (!parsed) {
      batchErrors.push(`${batch.sourceLabel}: model reply was not valid extraction JSON`);
      continue;
    }

    const spanByMarker = new Map(batch.spans.map((span) => [span.marker, span] as const));
    // Local (per-batch) id -> canonical (global, label-slug) id — nodes
    // merge across batches by normalized label, not by the model's own
    // (only-locally-unique) id string.
    const localToCanonical = new Map<string, string>();

    for (const node of parsed.nodes) {
      const canonicalId = slugify(node.label);
      localToCanonical.set(node.id, canonicalId);
      const existing = nodesById.get(canonicalId);
      if (existing) {
        existing.mentions += 1;
      } else {
        nodesById.set(canonicalId, { id: canonicalId, label: node.label, kind: node.kind, mentions: 1 });
      }
    }

    for (const edge of parsed.edges) {
      const sourceCanonical = localToCanonical.get(edge.source);
      const targetCanonical = localToCanonical.get(edge.target);
      if (!sourceCanonical || !targetCanonical || sourceCanonical === targetCanonical) continue;

      const evidence: EvidenceSpan[] = [];
      for (const marker of edge.evidence) {
        const span = spanByMarker.get(marker);
        if (!span) continue;
        evidence.push({
          sourceType: batch.sourceType,
          sourceId: batch.sourceId,
          sourceLabel: batch.sourceLabel,
          quote: span.quote,
          locator: span.locator,
        });
      }
      if (evidence.length === 0) continue;

      const key = edgeKey(sourceCanonical, edge.relation, targetCanonical);
      const existing = edgesByKey.get(key);
      if (existing) {
        for (const span of evidence) {
          const alreadyPresent = existing.evidence.some((e) => e.sourceId === span.sourceId && e.locator === span.locator && e.quote === span.quote);
          if (!alreadyPresent) existing.evidence.push(span);
        }
      } else {
        edgesByKey.set(key, {
          id: key,
          source: sourceCanonical,
          target: targetCanonical,
          relation: edge.relation,
          evidence,
        });
      }
    }
  }

  return {
    nodes: Array.from(nodesById.values()).sort((a, b) => a.label.localeCompare(b.label)),
    edges: Array.from(edgesByKey.values()),
    batchErrors,
  };
}

/**
 * Finds nodes whose label plausibly matches free-typed `text` — exact
 * (normalized) match first, then substring either direction — sorted best
 * match first. Returns an empty array rather than throwing when nothing
 * matches, so callers (`answerRelationQuery`) can turn that into a normal
 * "entity not found" result instead of an exception.
 */
export function findNodesMatching(graph: KnowledgeGraph, text: string): GraphNode[] {
  const needle = text.trim().toLowerCase();
  if (!needle) return [];
  const needleSlug = slugify(text);

  const scored = graph.nodes
    .map((node) => {
      const label = node.label.toLowerCase();
      let score = 0;
      if (node.id === needleSlug || label === needle) score = 3;
      else if (label.includes(needle) || needle.includes(label)) score = 2;
      else if (node.id.includes(needleSlug) || needleSlug.includes(node.id)) score = 1;
      return { node, score };
    })
    .filter((entry) => entry.score > 0)
    .sort((a, b) => b.score - a.score || b.node.mentions - a.node.mentions);

  return scored.map((entry) => entry.node);
}

/**
 * Breadth-first shortest path between two node ids, treating every edge as
 * undirected (relationship queries are symmetric — "how is X related to Y"
 * should find a `Y depends_on X` edge just as readily as `X depends_on Y`).
 * Returns the ordered edges walked, or `null` if either id is unknown to the
 * graph or no path connects them.
 */
export function shortestPath(graph: KnowledgeGraph, fromId: string, toId: string): GraphEdge[] | null {
  if (fromId === toId) return [];
  const nodeIds = new Set(graph.nodes.map((n) => n.id));
  if (!nodeIds.has(fromId) || !nodeIds.has(toId)) return null;

  const adjacency = new Map<string, GraphEdge[]>();
  for (const edge of graph.edges) {
    if (!adjacency.has(edge.source)) adjacency.set(edge.source, []);
    if (!adjacency.has(edge.target)) adjacency.set(edge.target, []);
    adjacency.get(edge.source)!.push(edge);
    adjacency.get(edge.target)!.push(edge);
  }

  const visited = new Set<string>([fromId]);
  const predecessor = new Map<string, { via: GraphEdge; from: string }>();
  const queue: string[] = [fromId];

  while (queue.length > 0) {
    const current = queue.shift()!;
    if (current === toId) break;
    for (const edge of adjacency.get(current) ?? []) {
      const next = edge.source === current ? edge.target : edge.source;
      if (visited.has(next)) continue;
      visited.add(next);
      predecessor.set(next, { via: edge, from: current });
      queue.push(next);
    }
  }

  if (!visited.has(toId)) return null;

  const path: GraphEdge[] = [];
  let cursor = toId;
  while (cursor !== fromId) {
    const step = predecessor.get(cursor);
    if (!step) return null;
    path.unshift(step.via);
    cursor = step.from;
  }
  return path;
}

/** Free-text "how is X related to Y" parser — tries a few common phrasings
 * before falling back to a plain `"X and Y"` / `"X to Y"` split. Returns
 * `null` when no two entity names could be pulled out at all. */
export function parseRelationQuery(text: string): { fromText: string; toText: string } | null {
  const trimmed = text.trim();
  if (!trimmed) return null;

  const patterns: RegExp[] = [
    /how\s+(?:is|are|does|do)\s+(.+?)\s+(?:related|connected|linked)\s+to\s+(.+?)[?.!]?$/i,
    /relationship\s+between\s+(.+?)\s+and\s+(.+?)[?.!]?$/i,
    /connection\s+between\s+(.+?)\s+and\s+(.+?)[?.!]?$/i,
    /how\s+does\s+(.+?)\s+depend\s+on\s+(.+?)[?.!]?$/i,
  ];
  for (const pattern of patterns) {
    const match = trimmed.match(pattern);
    if (match && match[1]?.trim() && match[2]?.trim()) {
      return { fromText: match[1].trim(), toText: match[2].trim() };
    }
  }

  // Fallback: a plain "X and Y" / "X to Y" — only when it splits into
  // exactly two non-empty, reasonably short parts, so an ordinary sentence
  // that happens to contain "and" doesn't get misread as two entity names.
  const fallback = trimmed.match(/^(.{1,60}?)\s+(?:and|to|<->|->)\s+(.{1,60})$/i);
  if (fallback && fallback[1]?.trim() && fallback[2]?.trim()) {
    return { fromText: fallback[1].trim(), toText: fallback[2].trim() };
  }

  return null;
}

export interface RelationQueryResult {
  queryText: string;
  fromText: string;
  toText: string;
  fromNode: GraphNode | null;
  toNode: GraphNode | null;
  path: GraphEdge[] | null;
  evidence: EvidenceSpan[];
  explanation: string;
  error: string | null;
}

function dedupeEvidence(spans: EvidenceSpan[]): EvidenceSpan[] {
  const seen = new Set<string>();
  const result: EvidenceSpan[] = [];
  for (const span of spans) {
    const key = `${span.sourceId} ${span.locator} ${span.quote}`;
    if (seen.has(key)) continue;
    seen.add(key);
    result.push(span);
  }
  return result;
}

function nodeLabel(graph: KnowledgeGraph, id: string): string {
  return graph.nodes.find((n) => n.id === id)?.label ?? id;
}

/**
 * Answers a free-text "how is X related to Y" query by parsing out the two
 * entity mentions, resolving each to the best-matching graph node, walking
 * the shortest path between them, and returning that path together with the
 * deduplicated evidence spans for every edge on it — the "graph evidence
 * behind the answer" the acceptance criterion asks for. Every failure mode
 * (unparseable query, unknown entity, no path) is returned as a normal
 * result with `error` set and empty evidence, never thrown.
 */
export function answerRelationQuery(graph: KnowledgeGraph, queryText: string): RelationQueryResult {
  const base = { queryText, fromText: '', toText: '', fromNode: null, toNode: null, path: null, evidence: [] };

  const parsed = parseRelationQuery(queryText);
  if (!parsed) {
    return {
      ...base,
      explanation: '',
      error: 'Could not find two entity names in that question. Try phrasing it as "How is X related to Y?"',
    };
  }
  const { fromText, toText } = parsed;

  const fromMatches = findNodesMatching(graph, fromText);
  const toMatches = findNodesMatching(graph, toText);
  if (fromMatches.length === 0) {
    return { ...base, fromText, toText, explanation: '', error: `No entity matching "${fromText}" was found in the graph.` };
  }
  if (toMatches.length === 0) {
    return { ...base, fromText, toText, explanation: '', error: `No entity matching "${toText}" was found in the graph.` };
  }

  const fromNode = fromMatches[0];
  const toNode = toMatches[0];

  if (fromNode.id === toNode.id) {
    return {
      ...base,
      fromText,
      toText,
      fromNode,
      toNode,
      explanation: `"${fromText}" and "${toText}" both resolved to the same entity, ${fromNode.label}.`,
      error: null,
    };
  }

  const path = shortestPath(graph, fromNode.id, toNode.id);
  if (!path) {
    return {
      ...base,
      fromText,
      toText,
      fromNode,
      toNode,
      explanation: '',
      error: `No connection was found between ${fromNode.label} and ${toNode.label} in the current graph. Try rebuilding it after syncing more sources.`,
    };
  }

  const evidence = dedupeEvidence(path.flatMap((edge) => edge.evidence));
  const hops = path.map((edge) => `${nodeLabel(graph, edge.source)} --${edge.relation}--> ${nodeLabel(graph, edge.target)}`);
  const explanation =
    path.length === 0
      ? `${fromNode.label} and ${toNode.label} are the same entity.`
      : `${fromNode.label} is connected to ${toNode.label} via: ${hops.join('; ')}.`;

  return { queryText, fromText, toText, fromNode, toNode, path, evidence, explanation, error: null };
}

function mermaidSafeId(id: string): string {
  const safe = id.replace(/[^A-Za-z0-9_]/g, '_');
  return /^[A-Za-z_]/.test(safe) ? `n_${safe}` : `n_${safe || 'x'}`;
}

function mermaidEscapeLabel(label: string): string {
  return label.replace(/"/g, "'").replace(/\r?\n/g, ' ');
}

const MAX_MERMAID_NODES = 80;

/**
 * Renders the graph (or a highlighted subset of it) as a Mermaid
 * `flowchart` diagram string — the app's existing `mermaid` dependency
 * renders this directly, so no new graph-visualization library is needed.
 * When `highlightEdgeIds` is given (the edges of a resolved relation-query
 * path), those edges and their endpoint nodes get a `highlight` CSS class
 * so the panel can visually pick out the answered path within the full
 * graph. Truncates to `MAX_MERMAID_NODES` highest-mention nodes (keeping
 * every node the diagram is asked to highlight) so one very large graph
 * still renders something useful instead of a hung/garbled diagram.
 */
export function toMermaidFlowchart(graph: KnowledgeGraph, highlightEdgeIds: string[] = []): string {
  const highlightSet = new Set(highlightEdgeIds);
  const highlightedNodeIds = new Set<string>();
  for (const edge of graph.edges) {
    if (highlightSet.has(edge.id)) {
      highlightedNodeIds.add(edge.source);
      highlightedNodeIds.add(edge.target);
    }
  }

  let nodes = graph.nodes;
  if (nodes.length > MAX_MERMAID_NODES) {
    const keep = new Set(highlightedNodeIds);
    const ranked = [...nodes].sort((a, b) => b.mentions - a.mentions);
    for (const node of ranked) {
      if (keep.size >= MAX_MERMAID_NODES) break;
      keep.add(node.id);
    }
    nodes = nodes.filter((n) => keep.has(n.id));
  }
  const keptIds = new Set(nodes.map((n) => n.id));
  const edges = graph.edges.filter((e) => keptIds.has(e.source) && keptIds.has(e.target));

  const lines: string[] = ['flowchart LR'];
  for (const node of nodes) {
    lines.push(`  ${mermaidSafeId(node.id)}["${mermaidEscapeLabel(node.label)} (${node.kind})"]`);
  }
  for (const edge of edges) {
    lines.push(`  ${mermaidSafeId(edge.source)} -->|${edge.relation}| ${mermaidSafeId(edge.target)}`);
  }
  if (highlightedNodeIds.size > 0) {
    lines.push('  classDef highlight stroke:#f59e0b,stroke-width:3px;');
    const highlightIds = [...highlightedNodeIds].filter((id) => keptIds.has(id)).map(mermaidSafeId);
    if (highlightIds.length > 0) lines.push(`  class ${highlightIds.join(',')} highlight;`);
  }
  if (nodes.length === 0) lines.push('  empty["No entities extracted yet"]');
  return lines.join('\n');
}
