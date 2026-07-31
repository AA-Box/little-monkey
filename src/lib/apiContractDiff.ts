/**
 * API Contract Diff and Mock Lab (ROADMAP Phase 7, item 23) — MVP scope:
 * OpenAPI JSON/YAML only, two local files (an "old" and a "new" version)
 * loaded through the existing file-open dialog (see
 * `apiContractDiffStore.ts`). GraphQL/protobuf/webhook/event-schema support
 * is an explicit follow-up, not attempted here.
 *
 * This module is pure TS (no React/store import), mirroring `sopCompiler.ts`
 * and `riskJudge.ts`'s dependency-injection pattern: the one-shot
 * client-impact-note model call takes a `callModel` closure rather than
 * importing `turnEngine.ts` directly, so `apiContractDiffStore.ts` is the one
 * that builds that closure around `agentLoop.ts`'s `resolveTarget` and
 * `turnEngine.ts`'s `attemptStream`.
 *
 * Pipeline: `parseOpenApiDocument` (hand-rolled JSON/YAML-subset parser ->
 * a small structural model of paths/methods/params/schemas) ->
 * `diffApiDocuments` (structural diff, classifying each change as
 * `breaking` or `non-breaking`) -> `generateMockResponses` /
 * `generateContractTestStub` (derived from the NEW document) ->
 * `draftClientImpactNotes` (one batched model call covering every breaking
 * change, in the same strict-single-line-JSON-reply style `sopCompiler.ts`
 * uses).
 */
import type { ChatMessage } from './llamaClient';
import { errorMessage } from "./errors";

// ---------------------------------------------------------------------------
// Generic JSON/YAML-subset value parsing
// ---------------------------------------------------------------------------

export type JsonScalar = string | number | boolean | null;
export type JsonValue = JsonScalar | JsonValue[] | { [key: string]: JsonValue };

/** Caps how much of a document is scanned/rendered — generous for a real
 * OpenAPI spec, but bounded so a huge file can't hang the UI. */
export const MAX_DOCUMENT_CHARS = 400_000;

function isPlainObject(value: JsonValue): value is { [key: string]: JsonValue } {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function coerceYamlScalar(raw: string): JsonValue {
  const trimmed = raw.trim();
  if (trimmed === '' || trimmed === '~' || trimmed === 'null' || trimmed === 'Null' || trimmed === 'NULL') return null;
  if (trimmed === 'true' || trimmed === 'True' || trimmed === 'TRUE') return true;
  if (trimmed === 'false' || trimmed === 'False' || trimmed === 'FALSE') return false;
  if (/^-?\d+$/.test(trimmed)) return Number.parseInt(trimmed, 10);
  if (/^-?\d+\.\d+$/.test(trimmed)) return Number.parseFloat(trimmed);
  if ((trimmed.startsWith('"') && trimmed.endsWith('"')) || (trimmed.startsWith("'") && trimmed.endsWith("'"))) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}

/** Parses a single flow-style scalar/array/object fragment, e.g. `[a, b, c]`
 * or `{name: foo, in: query}` — the subset actually seen in real OpenAPI
 * YAML (enum lists, short param objects), not general YAML flow syntax. */
function parseFlowValue(raw: string): JsonValue {
  const trimmed = raw.trim();
  if (trimmed.startsWith('[') && trimmed.endsWith(']')) {
    const inner = trimmed.slice(1, -1).trim();
    if (!inner) return [];
    return splitFlowItems(inner).map((item) => parseFlowValue(item));
  }
  if (trimmed.startsWith('{') && trimmed.endsWith('}')) {
    const inner = trimmed.slice(1, -1).trim();
    const result: { [key: string]: JsonValue } = {};
    if (!inner) return result;
    for (const item of splitFlowItems(inner)) {
      const colonIndex = item.indexOf(':');
      if (colonIndex === -1) continue;
      const key = item.slice(0, colonIndex).trim().replace(/^['"]|['"]$/g, '');
      result[key] = parseFlowValue(item.slice(colonIndex + 1));
    }
    return result;
  }
  return coerceYamlScalar(trimmed);
}

function splitFlowItems(inner: string): string[] {
  const items: string[] = [];
  let depth = 0;
  let current = '';
  for (const char of inner) {
    if (char === '[' || char === '{') depth += 1;
    if (char === ']' || char === '}') depth -= 1;
    if (char === ',' && depth === 0) {
      items.push(current);
      current = '';
    } else {
      current += char;
    }
  }
  if (current.trim()) items.push(current);
  return items;
}

function indentOf(line: string): number {
  let count = 0;
  while (count < line.length && line[count] === ' ') count += 1;
  return count;
}

/** Minimal indentation-based YAML-subset parser: block mappings, block
 * sequences (`- item`), flow scalars/arrays/objects, quoted strings, and
 * basic scalar coercion. Does not attempt anchors/aliases, multi-document
 * streams, or block scalars (`|`/`>`) beyond treating them as a plain
 * string of their first line — real OpenAPI YAML rarely leans on those for
 * the paths/schemas structure this feature cares about. */
export function parseYamlOrJson(text: string): JsonValue {
  const trimmed = text.trim();
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    return JSON.parse(trimmed) as JsonValue;
  }

  const rawLines = trimmed.split(/\r?\n/);
  const lines: { indent: number; content: string }[] = [];
  for (const rawLine of rawLines) {
    const withoutComment = stripYamlComment(rawLine);
    if (!withoutComment.trim()) continue;
    if (withoutComment.trim() === '---' || withoutComment.trim() === '...') continue;
    lines.push({ indent: indentOf(withoutComment), content: withoutComment.trim() });
  }

  let cursor = 0;
  function parseBlock(minIndent: number): JsonValue {
    if (cursor >= lines.length || lines[cursor].indent < minIndent) return null;
    const blockIndent = lines[cursor].indent;
    if (lines[cursor].content.startsWith('- ') || lines[cursor].content === '-') {
      const items: JsonValue[] = [];
      while (cursor < lines.length && lines[cursor].indent === blockIndent && (lines[cursor].content === '-' || lines[cursor].content.startsWith('- '))) {
        const rest = lines[cursor].content === '-' ? '' : lines[cursor].content.slice(2);
        if (!rest.trim()) {
          cursor += 1;
          items.push(parseBlock(blockIndent + 1));
        } else if (/^[A-Za-z0-9_.$-]+\s*:(\s|$)/.test(rest)) {
          // Inline "- key: value" starting a mapping item — synthesize a
          // pseudo-line at the item's own content indent so the nested
          // mapping parser below picks up the rest of its sibling keys.
          const syntheticIndent = blockIndent + 2;
          lines[cursor] = { indent: syntheticIndent, content: rest };
          items.push(parseBlock(syntheticIndent));
        } else {
          cursor += 1;
          items.push(parseFlowValue(rest));
        }
      }
      return items;
    }

    const map: { [key: string]: JsonValue } = {};
    while (cursor < lines.length && lines[cursor].indent === blockIndent) {
      const line = lines[cursor];
      const colonIndex = findMappingColon(line.content);
      if (colonIndex === -1) {
        cursor += 1;
        continue;
      }
      const rawKey = line.content.slice(0, colonIndex).trim();
      const key = rawKey.replace(/^['"]|['"]$/g, '');
      const valuePart = line.content.slice(colonIndex + 1).trim();
      cursor += 1;
      if (!valuePart) {
        const next = cursor < lines.length ? lines[cursor] : null;
        if (next && next.indent > blockIndent) {
          map[key] = parseBlock(next.indent);
        } else {
          map[key] = null;
        }
      } else if (valuePart === '|' || valuePart === '>' || valuePart === '|-' || valuePart === '>-') {
        const parts: string[] = [];
        while (cursor < lines.length && lines[cursor].indent > blockIndent) {
          parts.push(lines[cursor].content);
          cursor += 1;
        }
        map[key] = parts.join(valuePart.startsWith('|') ? '\n' : ' ');
      } else {
        map[key] = parseFlowValue(valuePart);
      }
    }
    return map;
  }

  const result = parseBlock(0);
  return result ?? {};
}

function stripYamlComment(line: string): string {
  let inSingle = false;
  let inDouble = false;
  for (let i = 0; i < line.length; i += 1) {
    const char = line[i];
    if (char === "'" && !inDouble) inSingle = !inSingle;
    else if (char === '"' && !inSingle) inDouble = !inDouble;
    else if (char === '#' && !inSingle && !inDouble && (i === 0 || line[i - 1] === ' ')) {
      return line.slice(0, i);
    }
  }
  return line;
}

function findMappingColon(content: string): number {
  let inSingle = false;
  let inDouble = false;
  let depth = 0;
  for (let i = 0; i < content.length; i += 1) {
    const char = content[i];
    if (char === "'" && !inDouble) inSingle = !inSingle;
    else if (char === '"' && !inSingle) inDouble = !inDouble;
    else if (!inSingle && !inDouble) {
      if (char === '[' || char === '{') depth += 1;
      else if (char === ']' || char === '}') depth -= 1;
      else if (char === ':' && depth === 0 && (i === content.length - 1 || content[i + 1] === ' ')) return i;
    }
  }
  return -1;
}

// ---------------------------------------------------------------------------
// OpenAPI structural model
// ---------------------------------------------------------------------------

export interface ApiSchema {
  type?: string;
  format?: string;
  properties?: Record<string, ApiSchema>;
  required?: string[];
  items?: ApiSchema;
  enum?: JsonScalar[];
  nullable?: boolean;
  ref?: string;
  description?: string;
}

export interface ApiParameter {
  name: string;
  in: string;
  required: boolean;
  schema?: ApiSchema;
}

export interface ApiResponse {
  status: string;
  schema?: ApiSchema;
}

export interface ApiOperation {
  path: string;
  method: string;
  operationId?: string;
  summary?: string;
  parameters: ApiParameter[];
  requestBodySchema?: ApiSchema;
  requestBodyRequired: boolean;
  responses: ApiResponse[];
}

export interface ApiDocument {
  title: string;
  version: string;
  operations: ApiOperation[];
  schemas: Record<string, ApiSchema>;
}

const HTTP_METHODS = ['get', 'put', 'post', 'delete', 'options', 'head', 'patch', 'trace'];

function asRecord(value: JsonValue | undefined): Record<string, JsonValue> {
  return isPlainObject(value as JsonValue) ? (value as Record<string, JsonValue>) : {};
}

function refName(ref: unknown): string | undefined {
  if (typeof ref !== 'string') return undefined;
  const parts = ref.split('/');
  return parts[parts.length - 1];
}

function toApiSchema(value: JsonValue | undefined): ApiSchema | undefined {
  if (!isPlainObject(value as JsonValue)) return undefined;
  const record = value as Record<string, JsonValue>;
  if (record['$ref'] !== undefined) {
    return { ref: refName(record['$ref']) };
  }
  const schema: ApiSchema = {};
  if (typeof record.type === 'string') schema.type = record.type;
  if (typeof record.format === 'string') schema.format = record.format;
  if (typeof record.description === 'string') schema.description = record.description;
  if (record.nullable === true) schema.nullable = true;
  if (Array.isArray(record.required)) {
    schema.required = record.required.filter((entry): entry is string => typeof entry === 'string');
  }
  if (Array.isArray(record.enum)) {
    schema.enum = record.enum.filter((entry): entry is JsonScalar => entry === null || typeof entry !== 'object');
  }
  if (isPlainObject(record.properties)) {
    const properties: Record<string, ApiSchema> = {};
    for (const [key, propValue] of Object.entries(record.properties as Record<string, JsonValue>)) {
      const propSchema = toApiSchema(propValue);
      if (propSchema) properties[key] = propSchema;
    }
    schema.properties = properties;
    if (!schema.type) schema.type = 'object';
  }
  if (record.items !== undefined) {
    const itemsSchema = toApiSchema(record.items);
    if (itemsSchema) schema.items = itemsSchema;
    if (!schema.type) schema.type = 'array';
  }
  return schema;
}

function extractParameters(value: JsonValue | undefined): ApiParameter[] {
  if (!Array.isArray(value)) return [];
  const parameters: ApiParameter[] = [];
  for (const entry of value) {
    if (!isPlainObject(entry)) continue;
    const record = entry as Record<string, JsonValue>;
    const name = typeof record.name === 'string' ? record.name : null;
    if (!name) continue;
    parameters.push({
      name,
      in: typeof record.in === 'string' ? record.in : 'query',
      required: record.required === true,
      schema: toApiSchema(record.schema),
    });
  }
  return parameters;
}

function extractRequestBody(value: JsonValue | undefined): { schema?: ApiSchema; required: boolean } {
  const record = asRecord(value);
  const required = record.required === true;
  const content = asRecord(record.content);
  const jsonContent = asRecord(content['application/json']);
  return { schema: toApiSchema(jsonContent.schema), required };
}

function extractResponses(value: JsonValue | undefined): ApiResponse[] {
  const record = asRecord(value);
  const responses: ApiResponse[] = [];
  for (const [status, responseValue] of Object.entries(record)) {
    const responseRecord = asRecord(responseValue);
    const content = asRecord(responseRecord.content);
    const jsonContent = asRecord(content['application/json']);
    responses.push({ status, schema: toApiSchema(jsonContent.schema) });
  }
  return responses.sort((left, right) => left.status.localeCompare(right.status));
}

/**
 * Parses a whole OpenAPI 3.x (or Swagger 2.0-shaped, best-effort) JSON/YAML
 * document into the structural model above. Throws a plain `Error` with a
 * user-facing message on anything that doesn't look like an OpenAPI
 * document — callers must fail closed rather than diff garbage.
 */
export function parseOpenApiDocument(text: string, sourceLabel?: string): ApiDocument {
  if (text.length > MAX_DOCUMENT_CHARS) {
    throw new Error(`${sourceLabel ?? 'This document'} is larger than ${Math.floor(MAX_DOCUMENT_CHARS / 1000)}KB — split it before diffing.`);
  }
  let parsed: JsonValue;
  try {
    parsed = parseYamlOrJson(text);
  } catch (err) {
    throw new Error(`Could not parse ${sourceLabel ?? 'document'} as JSON or YAML: ${errorMessage(err)}`);
  }
  if (!isPlainObject(parsed)) {
    throw new Error(`${sourceLabel ?? 'This document'} does not look like an OpenAPI document (expected an object at the top level).`);
  }
  const record = parsed as Record<string, JsonValue>;
  if (record.openapi === undefined && record.swagger === undefined) {
    throw new Error(`${sourceLabel ?? 'This document'} is missing an "openapi" or "swagger" version field — is it really an OpenAPI spec?`);
  }
  const info = asRecord(record.info);
  const pathsRecord = asRecord(record.paths);
  const operations: ApiOperation[] = [];
  for (const [path, pathItemValue] of Object.entries(pathsRecord)) {
    const pathItem = asRecord(pathItemValue);
    const pathLevelParams = extractParameters(pathItem.parameters);
    for (const method of HTTP_METHODS) {
      const opValue = pathItem[method];
      if (!isPlainObject(opValue)) continue;
      const opRecord = opValue as Record<string, JsonValue>;
      const ownParams = extractParameters(opRecord.parameters);
      const mergedParams = [...pathLevelParams.filter((p) => !ownParams.some((o) => o.name === p.name && o.in === p.in)), ...ownParams];
      const requestBody = extractRequestBody(opRecord.requestBody);
      operations.push({
        path,
        method: method.toUpperCase(),
        operationId: typeof opRecord.operationId === 'string' ? opRecord.operationId : undefined,
        summary: typeof opRecord.summary === 'string' ? opRecord.summary : undefined,
        parameters: mergedParams,
        requestBodySchema: requestBody.schema,
        requestBodyRequired: requestBody.required,
        responses: extractResponses(opRecord.responses),
      });
    }
  }
  const componentsRecord = asRecord(record.components);
  const schemasRecord = asRecord(componentsRecord.schemas ?? record.definitions);
  const schemas: Record<string, ApiSchema> = {};
  for (const [name, schemaValue] of Object.entries(schemasRecord)) {
    const schema = toApiSchema(schemaValue);
    if (schema) schemas[name] = schema;
  }

  return {
    title: typeof info.title === 'string' ? info.title : sourceLabel ?? 'Untitled API',
    version: typeof info.version === 'string' ? info.version : 'unknown',
    operations: operations.sort((left, right) => `${left.path} ${left.method}`.localeCompare(`${right.path} ${right.method}`)),
    schemas,
  };
}

/** Resolves a `$ref`-only schema node against the document's component
 * schemas — bounded depth so a self-referential schema can't recurse
 * forever. */
export function resolveSchema(schema: ApiSchema | undefined, schemas: Record<string, ApiSchema>, depth = 0): ApiSchema | undefined {
  if (!schema) return undefined;
  if (schema.ref && depth < 8) {
    return resolveSchema(schemas[schema.ref], schemas, depth + 1);
  }
  return schema;
}

// ---------------------------------------------------------------------------
// Structural diff
// ---------------------------------------------------------------------------

export type ChangeSeverity = 'breaking' | 'non-breaking';

export type ChangeKind =
  | 'endpoint-removed'
  | 'endpoint-added'
  | 'param-removed'
  | 'param-added'
  | 'param-now-required'
  | 'param-now-optional'
  | 'param-type-changed'
  | 'field-removed'
  | 'field-added'
  | 'field-now-required'
  | 'field-now-optional'
  | 'field-type-changed'
  | 'response-removed'
  | 'response-added'
  | 'enum-value-removed'
  | 'enum-value-added';

export interface ApiChange {
  id: string;
  severity: ChangeSeverity;
  kind: ChangeKind;
  operationLabel: string;
  detail: string;
}

let changeCounter = 0;
function nextChangeId(): string {
  changeCounter += 1;
  return `change-${changeCounter}`;
}

function pushChange(changes: ApiChange[], severity: ChangeSeverity, kind: ChangeKind, operationLabel: string, detail: string): void {
  changes.push({ id: nextChangeId(), severity, kind, operationLabel, detail });
}

function diffSchemaNode(
  oldSchema: ApiSchema | undefined,
  newSchema: ApiSchema | undefined,
  oldSchemas: Record<string, ApiSchema>,
  newSchemas: Record<string, ApiSchema>,
  operationLabel: string,
  contextLabel: string,
  changes: ApiChange[],
): void {
  const resolvedOld = resolveSchema(oldSchema, oldSchemas);
  const resolvedNew = resolveSchema(newSchema, newSchemas);
  if (!resolvedOld && !resolvedNew) return;
  if (resolvedOld && !resolvedNew) {
    pushChange(changes, 'breaking', 'field-removed', operationLabel, `${contextLabel} was removed entirely.`);
    return;
  }
  if (!resolvedOld && resolvedNew) {
    return; // A brand-new schema appearing where there was none is reported by the caller as a field/response add.
  }
  const before = resolvedOld as ApiSchema;
  const after = resolvedNew as ApiSchema;

  if (before.type && after.type && before.type !== after.type) {
    pushChange(changes, 'breaking', 'field-type-changed', operationLabel, `${contextLabel} changed type from \`${before.type}\` to \`${after.type}\`.`);
  }

  if (before.enum && after.enum) {
    const removedValues = before.enum.filter((value) => !after.enum!.includes(value));
    const addedValues = after.enum.filter((value) => !before.enum!.includes(value));
    if (removedValues.length > 0) {
      pushChange(changes, 'breaking', 'enum-value-removed', operationLabel, `${contextLabel} removed enum value(s): ${removedValues.map((v) => JSON.stringify(v)).join(', ')}.`);
    }
    if (addedValues.length > 0) {
      pushChange(changes, 'non-breaking', 'enum-value-added', operationLabel, `${contextLabel} added enum value(s): ${addedValues.map((v) => JSON.stringify(v)).join(', ')}.`);
    }
  }

  if (before.type === 'object' || after.type === 'object' || before.properties || after.properties) {
    const beforeProps = before.properties ?? {};
    const afterProps = after.properties ?? {};
    const beforeRequired = new Set(before.required ?? []);
    const afterRequired = new Set(after.required ?? []);
    const allKeys = new Set([...Object.keys(beforeProps), ...Object.keys(afterProps)]);
    for (const key of allKeys) {
      const fieldLabel = `${contextLabel} field \`${key}\``;
      const beforeField = beforeProps[key];
      const afterField = afterProps[key];
      if (beforeField && !afterField) {
        pushChange(changes, 'breaking', 'field-removed', operationLabel, `${fieldLabel} was removed.`);
        continue;
      }
      if (!beforeField && afterField) {
        const isRequired = afterRequired.has(key);
        pushChange(
          changes,
          isRequired ? 'breaking' : 'non-breaking',
          isRequired ? 'field-now-required' : 'field-added',
          operationLabel,
          isRequired ? `${fieldLabel} was added as a NEW REQUIRED field.` : `${fieldLabel} was added as an optional field.`,
        );
        continue;
      }
      if (beforeField && afterField) {
        const wasRequired = beforeRequired.has(key);
        const isRequired = afterRequired.has(key);
        if (!wasRequired && isRequired) {
          pushChange(changes, 'breaking', 'field-now-required', operationLabel, `${fieldLabel} became required.`);
        } else if (wasRequired && !isRequired) {
          pushChange(changes, 'non-breaking', 'field-now-optional', operationLabel, `${fieldLabel} is no longer required.`);
        }
        diffSchemaNode(beforeField, afterField, oldSchemas, newSchemas, operationLabel, fieldLabel, changes);
      }
    }
  }

  if (before.items || after.items) {
    diffSchemaNode(before.items, after.items, oldSchemas, newSchemas, operationLabel, `${contextLabel}[] item`, changes);
  }
}

function diffParameters(
  oldParams: ApiParameter[],
  newParams: ApiParameter[],
  operationLabel: string,
  oldSchemas: Record<string, ApiSchema>,
  newSchemas: Record<string, ApiSchema>,
  changes: ApiChange[],
): void {
  const key = (p: ApiParameter) => `${p.in}:${p.name}`;
  const oldByKey = new Map(oldParams.map((p) => [key(p), p]));
  const newByKey = new Map(newParams.map((p) => [key(p), p]));
  const allKeys = new Set([...oldByKey.keys(), ...newByKey.keys()]);
  for (const paramKey of allKeys) {
    const before = oldByKey.get(paramKey);
    const after = newByKey.get(paramKey);
    const label = `param \`${paramKey.split(':')[1]}\` (${paramKey.split(':')[0]})`;
    if (before && !after) {
      pushChange(changes, 'breaking', 'param-removed', operationLabel, `${label} was removed.`);
      continue;
    }
    if (!before && after) {
      pushChange(
        changes,
        after.required ? 'breaking' : 'non-breaking',
        after.required ? 'param-now-required' : 'param-added',
        operationLabel,
        after.required ? `${label} was added as a NEW REQUIRED parameter.` : `${label} was added as an optional parameter.`,
      );
      continue;
    }
    if (before && after) {
      if (!before.required && after.required) {
        pushChange(changes, 'breaking', 'param-now-required', operationLabel, `${label} became required.`);
      } else if (before.required && !after.required) {
        pushChange(changes, 'non-breaking', 'param-now-optional', operationLabel, `${label} is no longer required.`);
      }
      const beforeSchema = resolveSchema(before.schema, oldSchemas);
      const afterSchema = resolveSchema(after.schema, newSchemas);
      if (beforeSchema?.type && afterSchema?.type && beforeSchema.type !== afterSchema.type) {
        pushChange(changes, 'breaking', 'param-type-changed', operationLabel, `${label} changed type from \`${beforeSchema.type}\` to \`${afterSchema.type}\`.`);
      }
    }
  }
}

/**
 * Structurally diffs two parsed OpenAPI documents, classifying every change
 * as `breaking` (removed endpoint/param/field, a param/field that became
 * newly required, a narrowed/changed type, a removed enum value, a removed
 * response status) or `non-breaking` (added endpoint, added optional
 * param/field, a param/field that became optional, an added enum value,
 * an added response status).
 */
export function diffApiDocuments(oldDoc: ApiDocument, newDoc: ApiDocument): ApiChange[] {
  const changes: ApiChange[] = [];
  const key = (op: ApiOperation) => `${op.method} ${op.path}`;
  const oldByKey = new Map(oldDoc.operations.map((op) => [key(op), op]));
  const newByKey = new Map(newDoc.operations.map((op) => [key(op), op]));
  const allKeys = new Set([...oldByKey.keys(), ...newByKey.keys()]);

  for (const opKey of allKeys) {
    const before = oldByKey.get(opKey);
    const after = newByKey.get(opKey);
    if (before && !after) {
      pushChange(changes, 'breaking', 'endpoint-removed', opKey, `Endpoint \`${opKey}\` was removed.`);
      continue;
    }
    if (!before && after) {
      pushChange(changes, 'non-breaking', 'endpoint-added', opKey, `Endpoint \`${opKey}\` was added.`);
      continue;
    }
    if (before && after) {
      diffParameters(before.parameters, after.parameters, opKey, oldDoc.schemas, newDoc.schemas, changes);
      if (before.requestBodySchema || after.requestBodySchema) {
        if (!before.requestBodyRequired && after.requestBodyRequired) {
          pushChange(changes, 'breaking', 'field-now-required', opKey, 'The request body became required.');
        } else if (before.requestBodyRequired && !after.requestBodyRequired) {
          pushChange(changes, 'non-breaking', 'field-now-optional', opKey, 'The request body is no longer required.');
        }
        diffSchemaNode(before.requestBodySchema, after.requestBodySchema, oldDoc.schemas, newDoc.schemas, opKey, 'request body', changes);
      }

      const oldResponses = new Map(before.responses.map((r) => [r.status, r]));
      const newResponses = new Map(after.responses.map((r) => [r.status, r]));
      const allStatuses = new Set([...oldResponses.keys(), ...newResponses.keys()]);
      for (const status of allStatuses) {
        const beforeResponse = oldResponses.get(status);
        const afterResponse = newResponses.get(status);
        if (beforeResponse && !afterResponse) {
          pushChange(changes, 'breaking', 'response-removed', opKey, `Response \`${status}\` was removed.`);
          continue;
        }
        if (!beforeResponse && afterResponse) {
          pushChange(changes, 'non-breaking', 'response-added', opKey, `Response \`${status}\` was added.`);
          continue;
        }
        if (beforeResponse && afterResponse) {
          diffSchemaNode(beforeResponse.schema, afterResponse.schema, oldDoc.schemas, newDoc.schemas, opKey, `response \`${status}\` body`, changes);
        }
      }
    }
  }
  return changes;
}

export function isReleaseReady(changes: ApiChange[], contractTests?: ContractTestReport | null): boolean {
  return Boolean(contractTests?.clean) && changes.every((change) => change.severity !== 'breaking');
}

export function breakingChangeCount(changes: ApiChange[]): number {
  return changes.filter((change) => change.severity === 'breaking').length;
}

// ---------------------------------------------------------------------------
// Mock response generation
// ---------------------------------------------------------------------------

function exampleForLeaf(schema: ApiSchema): JsonValue {
  if (schema.enum && schema.enum.length > 0) return schema.enum[0];
  switch (schema.type) {
    case 'string':
      if (schema.format === 'date-time') return '2024-01-01T00:00:00Z';
      if (schema.format === 'date') return '2024-01-01';
      if (schema.format === 'uuid') return '00000000-0000-4000-8000-000000000000';
      if (schema.format === 'email') return 'user@example.com';
      return 'string';
    case 'integer':
      return 0;
    case 'number':
      return 0;
    case 'boolean':
      return true;
    default:
      return null;
  }
}

/** Generates one example JSON value for a schema — bounded recursion depth
 * so a self-referential `$ref` chain can't hang the generator. */
export function generateMockValue(schema: ApiSchema | undefined, schemas: Record<string, ApiSchema>, depth = 0): JsonValue {
  const resolved = resolveSchema(schema, schemas);
  if (!resolved || depth > 6) return null;
  if (resolved.type === 'array' || resolved.items) {
    return [generateMockValue(resolved.items, schemas, depth + 1)];
  }
  if (resolved.type === 'object' || resolved.properties) {
    const example: { [key: string]: JsonValue } = {};
    for (const [key, propSchema] of Object.entries(resolved.properties ?? {})) {
      example[key] = generateMockValue(propSchema, schemas, depth + 1);
    }
    return example;
  }
  return exampleForLeaf(resolved);
}

export interface MockExample {
  operationLabel: string;
  status: string;
  example: JsonValue;
}

/** One example JSON response body per (operation, response-status) pair
 * that carries a schema, generated from the given document. */
export function generateMockResponses(doc: ApiDocument): MockExample[] {
  const mocks: MockExample[] = [];
  for (const operation of doc.operations) {
    for (const response of operation.responses) {
      if (!response.schema) continue;
      mocks.push({
        operationLabel: `${operation.method} ${operation.path}`,
        status: response.status,
        example: generateMockValue(response.schema, doc.schemas),
      });
    }
  }
  return mocks;
}

// ---------------------------------------------------------------------------
// Executable generated contract tests
// ---------------------------------------------------------------------------

export type ContractTestKind = 'request' | 'response';

export interface ContractTestResult {
  id: string;
  label: string;
  kind: ContractTestKind;
  passed: boolean;
  errors: string[];
}

export interface ContractTestReport {
  generatedAt: number;
  results: ContractTestResult[];
  passCount: number;
  failCount: number;
  /** Empty suites are not evidence and therefore never count as clean. */
  clean: boolean;
}

interface GeneratedContractCase {
  id: string;
  label: string;
  kind: ContractTestKind;
  schema: ApiSchema;
  sample: JsonValue;
}

function generatedContractCases(doc: ApiDocument): GeneratedContractCase[] {
  const cases: GeneratedContractCase[] = [];
  for (const operation of doc.operations) {
    const operationLabel = `${operation.method} ${operation.path}`;
    if (operation.requestBodySchema) {
      cases.push({
        id: `${operationLabel}:request`,
        label: `${operationLabel} request body`,
        kind: 'request',
        schema: operation.requestBodySchema,
        sample: generateMockValue(operation.requestBodySchema, doc.schemas),
      });
    }
    for (const response of operation.responses) {
      if (!response.schema) continue;
      cases.push({
        id: `${operationLabel}:response:${response.status}`,
        label: `${operationLabel} response ${response.status}`,
        kind: 'response',
        schema: response.schema,
        sample: generateMockValue(response.schema, doc.schemas),
      });
    }
  }
  return cases;
}

/** Recursively executes the JSON-schema subset supported by this lab against
 * a concrete value. This is shared by the in-app run and mirrored in the
 * exported Vitest artifact, so readiness comes from executed cases rather
 * than a mutable checkbox or unfilled TODO. */
export function validateContractValue(
  value: JsonValue,
  schema: ApiSchema | undefined,
  schemas: Record<string, ApiSchema>,
  path = '$',
  depth = 0,
): string[] {
  if (depth > 12) return [`${path}: schema recursion exceeded the safety limit.`];
  const resolved = resolveSchema(schema, schemas);
  if (!resolved) return [];
  if (value === null) return resolved.nullable ? [] : [`${path}: expected ${resolved.type ?? 'a value'}, got null.`];
  if (resolved.enum && !resolved.enum.some((candidate) => Object.is(candidate, value))) {
    return [`${path}: value is not one of the declared enum members.`];
  }
  if (resolved.type === 'string' && typeof value !== 'string') return [`${path}: expected string.`];
  if (resolved.type === 'integer' && (typeof value !== 'number' || !Number.isInteger(value))) return [`${path}: expected integer.`];
  if (resolved.type === 'number' && (typeof value !== 'number' || !Number.isFinite(value))) return [`${path}: expected finite number.`];
  if (resolved.type === 'boolean' && typeof value !== 'boolean') return [`${path}: expected boolean.`];
  if (resolved.type === 'array' || resolved.items) {
    if (!Array.isArray(value)) return [`${path}: expected array.`];
    return value.flatMap((item, index) => validateContractValue(item, resolved.items, schemas, `${path}[${index}]`, depth + 1));
  }
  if (resolved.type === 'object' || resolved.properties) {
    if (!isPlainObject(value)) return [`${path}: expected object.`];
    const errors: string[] = [];
    for (const field of resolved.required ?? []) {
      if (!Object.prototype.hasOwnProperty.call(value, field)) errors.push(`${path}.${field}: required field is missing.`);
    }
    for (const [field, childSchema] of Object.entries(resolved.properties ?? {})) {
      if (Object.prototype.hasOwnProperty.call(value, field)) {
        errors.push(...validateContractValue(value[field], childSchema, schemas, `${path}.${field}`, depth + 1));
      }
    }
    return errors;
  }
  return [];
}

export function runGeneratedContractTests(doc: ApiDocument): ContractTestReport {
  const results = generatedContractCases(doc).map((testCase): ContractTestResult => {
    const errors = validateContractValue(testCase.sample, testCase.schema, doc.schemas);
    return { id: testCase.id, label: testCase.label, kind: testCase.kind, passed: errors.length === 0, errors };
  });
  const passCount = results.filter((result) => result.passed).length;
  const failCount = results.length - passCount;
  return {
    generatedAt: Date.now(),
    results,
    passCount,
    failCount,
    clean: results.length > 0 && failCount === 0,
  };
}

/**
 * Renders a complete, runnable Vitest artifact with concrete generated
 * request/response examples. It contains no TODO payloads: every case calls
 * an embedded recursive validator and fails when its example violates the
 * new contract.
 */
export function generateContractTestStub(doc: ApiDocument): string {
  const cases = generatedContractCases(doc);
  const lines: string[] = [
    '/**',
    ` * Executable contract tests generated by the API Contract Diff and Mock Lab`,
    ` * for "${doc.title}" v${doc.version}.`,
    ' * Run with: `pnpm exec vitest run <this file>`.',
    ' */',
    "import { describe, expect, it } from 'vitest';",
    '',
    `const schemas = ${JSON.stringify(doc.schemas, null, 2)} as Record<string, any>;`,
    `const cases = ${JSON.stringify(cases, null, 2)} as Array<{ label: string; schema: any; sample: unknown }>;`,
    '',
    'function resolve(schema: any, depth = 0): any {',
    '  if (!schema || depth > 12) return schema;',
    "  return schema.ref && schemas[schema.ref] ? resolve(schemas[schema.ref], depth + 1) : schema;",
    '}',
    '',
    "function validate(value: unknown, input: any, path = '$', depth = 0): string[] {",
    "  if (depth > 12) return [path + ': schema recursion exceeded'];",
    '  const schema = resolve(input);',
    '  if (!schema) return [];',
    "  if (value === null) return schema.nullable ? [] : [path + ': unexpected null'];",
    "  if (schema.enum && !schema.enum.some((item: unknown) => Object.is(item, value))) return [path + ': enum mismatch'];",
    "  if (schema.type === 'string' && typeof value !== 'string') return [path + ': expected string'];",
    "  if (schema.type === 'integer' && (typeof value !== 'number' || !Number.isInteger(value))) return [path + ': expected integer'];",
    "  if (schema.type === 'number' && (typeof value !== 'number' || !Number.isFinite(value))) return [path + ': expected number'];",
    "  if (schema.type === 'boolean' && typeof value !== 'boolean') return [path + ': expected boolean'];",
    "  if (schema.type === 'array' || schema.items) {",
    "    if (!Array.isArray(value)) return [path + ': expected array'];",
    "    return value.flatMap((item, index) => validate(item, schema.items, path + '[' + index + ']', depth + 1));",
    '  }',
    "  if (schema.type === 'object' || schema.properties) {",
    "    if (!value || typeof value !== 'object' || Array.isArray(value)) return [path + ': expected object'];",
    '    const record = value as Record<string, unknown>;',
    '    const errors: string[] = [];',
    "    for (const field of schema.required || []) if (!Object.prototype.hasOwnProperty.call(record, field)) errors.push(path + '.' + field + ': missing');",
    "    for (const [field, child] of Object.entries(schema.properties || {})) if (Object.prototype.hasOwnProperty.call(record, field)) errors.push(...validate(record[field], child, path + '.' + field, depth + 1));",
    '    return errors;',
    '  }',
    '  return [];',
    '}',
    '',
    `describe(${JSON.stringify(`${doc.title} v${doc.version} generated contract`)}, () => {`,
    '  if (cases.length === 0) {',
    "    it('contains at least one schema-backed executable case', () => { expect(cases.length).toBeGreaterThan(0); });",
    '  }',
    '  for (const testCase of cases) {',
    '    it(testCase.label, () => {',
    '      expect(validate(testCase.sample, testCase.schema)).toEqual([]);',
    '    });',
    '  }',
    '});',
  ];
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// Client-impact note drafting (local-model call)
// ---------------------------------------------------------------------------

/** The minimal subset of `turnEngine.ts`'s `AttemptResult` this module needs
 * from `callModel` — dependency-injected exactly like `sopCompiler.ts`'s
 * `SopCompilerCallResult`, so this file stays pure TS. */
export interface ApiContractDiffCallResult {
  content: string;
  streamError: string | null;
}

export interface ClientImpactNote {
  changeId: string;
  impact: string;
  migration: string;
}

/** Caps how many breaking changes go into a single batched model call — a
 * huge breaking-change list gets its first N drafted rather than blowing up
 * a local model's context window; the rest still show up in the report
 * un-annotated. */
export const MAX_CHANGES_PER_NOTE_BATCH = 25;

export function buildClientImpactMessages(changes: ApiChange[]): ChatMessage[] {
  const numbered = changes.slice(0, MAX_CHANGES_PER_NOTE_BATCH).map((change, index) => `${index + 1}. [id=${change.id}] ${change.operationLabel}: ${change.detail}`).join('\n');
  return [
    {
      role: 'system',
      content: [
        'You are an API release engineer writing short client-impact notes for a list of BREAKING changes detected between two versions of an OpenAPI contract.',
        'For EACH numbered change, write a one-to-two sentence plain-English "impact" (who/what breaks and how) and a one-to-two sentence "migration" suggestion (what a client team should do about it).',
        'Reply with ONLY a single-line JSON array, no markdown, no other text, of this exact shape:',
        '[{"id":"change-1","impact":"...","migration":"..."}]',
        'The "id" in each entry must exactly match the `id=...` value given for that change. Include an entry for every change listed.',
      ].join(' '),
    },
    {
      role: 'user',
      content: `Breaking changes:\n${numbered}`,
    },
  ];
}

function asNonEmptyString(value: unknown): string | null {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null;
}

export function parseClientImpactResponse(content: string): ClientImpactNote[] {
  const candidates = [content.trim()];
  const embedded = content.match(/\[[\s\S]*\]/);
  if (embedded) candidates.push(embedded[0]);

  for (const candidate of candidates) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(candidate);
    } catch {
      continue;
    }
    if (!Array.isArray(parsed)) continue;
    const notes: ClientImpactNote[] = [];
    for (const entry of parsed) {
      if (!entry || typeof entry !== 'object') continue;
      const record = entry as Record<string, unknown>;
      const changeId = asNonEmptyString(record.id);
      const impact = asNonEmptyString(record.impact);
      const migration = asNonEmptyString(record.migration);
      if (!changeId || !impact) continue;
      notes.push({ changeId, impact, migration: migration ?? 'No specific migration guidance was provided — review the change manually.' });
    }
    if (notes.length > 0) return notes;
  }
  return [];
}

/**
 * Runs the one-shot, non-streaming, tool-less client-impact-note call for
 * every breaking change, returning notes keyed by `ApiChange.id`. Fails
 * closed (empty array) rather than fabricating a note when the model's
 * reply cannot be parsed — the UI must show that drafting failed, not a
 * silently-empty-but-successful state.
 */
export async function draftClientImpactNotes(
  changes: ApiChange[],
  callModel: (messages: ChatMessage[], signal?: AbortSignal) => Promise<ApiContractDiffCallResult>,
  signal?: AbortSignal,
): Promise<ClientImpactNote[]> {
  const breaking = changes.filter((change) => change.severity === 'breaking');
  if (breaking.length === 0) return [];
  const result = await callModel(buildClientImpactMessages(breaking), signal);
  if (result.streamError) {
    throw new Error(result.streamError);
  }
  const notes = parseClientImpactResponse(result.content);
  if (notes.length === 0) {
    throw new Error('The model did not return parsable client-impact notes. Try again.');
  }
  return notes;
}
