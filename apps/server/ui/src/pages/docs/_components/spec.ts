// The OpenAPI 3.1 document as the docs page reads it: just the parts it
// renders, a check that what the server sent is one, and the lookups every
// view shares — operations by tag, `$ref` resolution, type labels, examples.

export interface Schema {
  $ref?: string;
  type?: string | string[];
  format?: string;
  description?: string;
  properties?: Record<string, Schema>;
  required?: string[];
  items?: Schema;
  additionalProperties?: Schema | boolean;
  oneOf?: Schema[];
  anyOf?: Schema[];
  allOf?: Schema[];
  enum?: unknown[];
  const?: unknown;
  default?: unknown;
  example?: unknown;
  minimum?: number;
  maximum?: number;
  minItems?: number;
  maxItems?: number;
  pattern?: string;
}

export interface Parameter {
  name: string;
  in: 'path' | 'query' | 'header' | 'cookie';
  description?: string;
  required?: boolean;
  schema?: Schema;
}

interface Content {
  content?: Record<string, { schema?: Schema }>;
}

export interface Response extends Content {
  description: string;
}

export interface Operation {
  operationId?: string;
  summary?: string;
  description?: string;
  tags?: string[];
  parameters?: Parameter[];
  requestBody?: Content & { required?: boolean; description?: string };
  responses: Record<string, Response>;
  security?: Record<string, string[]>[];
}

export interface Document {
  openapi: string;
  info: { title: string; version: string; description?: string };
  tags?: { name: string; description?: string }[];
  paths: Record<string, Record<string, Operation>>;
  components?: {
    schemas?: Record<string, Schema>;
    securitySchemes?: Record<string, { type: string; scheme?: string; description?: string }>;
  };
}

/** One operation where the page lists it: its method, path and id. */
export interface Endpoint {
  id: string;
  method: string;
  path: string;
  operation: Operation;
}

export interface Group {
  tag: string;
  description: string | undefined;
  endpoints: Endpoint[];
}

const METHODS = ['get', 'post', 'put', 'patch', 'delete'];

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Whether `value` is an OpenAPI 3.1 document this page can render. */
export function isDocument(value: unknown): value is Document {
  return (
    record(value) &&
    typeof value.openapi === 'string' &&
    value.openapi.startsWith('3.1') &&
    record(value.info) &&
    record(value.paths)
  );
}

/** Every operation, grouped by its first tag, groups in the document's tag order. */
export function groups(document: Document): Group[] {
  const byTag = new Map<string, Endpoint[]>();
  for (const [path, item] of Object.entries(document.paths)) {
    for (const method of METHODS) {
      const operation = item[method];
      if (!operation) continue;
      const tag = operation.tags?.[0] ?? 'other';
      const id = operation.operationId ?? `${method}-${path}`;
      byTag.set(tag, [...(byTag.get(tag) ?? []), { id, method, path, operation }]);
    }
  }
  const declared = (document.tags ?? []).map((t) => t.name);
  const order = [...declared, ...[...byTag.keys()].filter((t) => !declared.includes(t))];
  return order
    .filter((tag) => byTag.has(tag))
    .map((tag) => ({
      tag,
      description: document.tags?.find((t) => t.name === tag)?.description,
      endpoints: byTag.get(tag) ?? [],
    }));
}

/** The component name a `$ref` points at. */
export function refName(ref: string): string {
  return ref.slice(ref.lastIndexOf('/') + 1);
}

/** The schema behind a `$ref`, or the schema itself. */
export function resolve(document: Document, schema: Schema): Schema {
  if (!schema.$ref) return schema;
  const target = document.components?.schemas?.[refName(schema.$ref)];
  if (!target) return schema;
  // Sibling keywords (a property's own description) override the target's.
  const siblings: Schema = { ...schema };
  delete siblings.$ref;
  return { ...target, ...siblings };
}

/** The variants of a union, with `null` taken out and reported separately. */
export function variants(schema: Schema): { options: Schema[]; nullable: boolean } {
  const union = schema.oneOf ?? schema.anyOf ?? [];
  const options = union.filter((s) => s.type !== 'null');
  return { options, nullable: options.length < union.length };
}

function types(schema: Schema): string[] {
  if (schema.type === undefined) return [];
  return Array.isArray(schema.type) ? schema.type : [schema.type];
}

/** A one-line type: `Wallet`, `string<uuid>`, `Movement[]`, `"a" | "b"`, `integer | null`. */
export function label(schema: Schema): string {
  if (schema.$ref) return refName(schema.$ref);
  if (schema.enum) return schema.enum.map((v) => JSON.stringify(v)).join(' | ');
  if (schema.const !== undefined) return JSON.stringify(schema.const);
  const { options, nullable } = variants(schema);
  if (options.length > 0) {
    return [...options.map(label), ...(nullable ? ['null'] : [])].join(' | ');
  }
  const named = types(schema);
  const orNull = named.includes('null');
  const base = named.find((t) => t !== 'null') ?? (schema.properties ? 'object' : 'any');
  let text = base;
  if (base === 'array' && schema.items) text = `${label(schema.items)}[]`;
  else if (base === 'object' && isMap(schema)) text = `map<string, ${label(mapValue(schema))}>`;
  else if (schema.format) text = `${base}<${schema.format}>`;
  return orNull ? `${text} | null` : text;
}

export function isMap(schema: Schema): boolean {
  return !schema.properties && typeof schema.additionalProperties === 'object';
}

function mapValue(schema: Schema): Schema {
  return typeof schema.additionalProperties === 'object' ? schema.additionalProperties : {};
}

/** Whether a schema, once resolved, has fields worth unfolding. */
export function nested(document: Document, schema: Schema): Schema | null {
  const resolved = resolve(document, schema);
  if (resolved.properties) return resolved;
  if (resolved.items) return nested(document, resolved.items);
  if (isMap(resolved)) return nested(document, mapValue(resolved));
  const { options } = variants(resolved);
  if (options.length === 1 && options[0]) return nested(document, options[0]);
  return null;
}

/** Constraints worth showing beside a type: bounds, sizes, pattern, default. */
export function constraints(schema: Schema): string[] {
  const out: string[] = [];
  if (schema.minimum !== undefined) out.push(`≥ ${schema.minimum}`);
  if (schema.maximum !== undefined) out.push(`≤ ${schema.maximum}`);
  if (schema.minItems !== undefined) out.push(`min ${schema.minItems} items`);
  if (schema.maxItems !== undefined) out.push(`max ${schema.maxItems} items`);
  if (schema.pattern) out.push(`/${schema.pattern}/`);
  if (schema.default !== undefined) out.push(`default ${JSON.stringify(schema.default)}`);
  return out;
}

/** A plausible value for `schema`, used to prefill a request body. */
export function example(document: Document, schema: Schema, seen: string[] = []): unknown {
  if (schema.$ref) {
    const name = refName(schema.$ref);
    if (seen.includes(name)) return {};
    return example(document, resolve(document, schema), [...seen, name]);
  }
  if (schema.example !== undefined) return schema.example;
  if (schema.default !== undefined) return schema.default;
  if (schema.const !== undefined) return schema.const;
  if (schema.enum?.length) return schema.enum[0];
  const { options } = variants(schema);
  if (options[0]) return example(document, options[0], seen);
  if (schema.allOf?.[0]) return example(document, schema.allOf[0], seen);
  const base = types(schema).find((t) => t !== 'null') ?? (schema.properties ? 'object' : '');
  switch (base) {
    case 'object': {
      const fields = Object.entries(schema.properties ?? {}).filter(
        ([name]) => schema.required?.includes(name) ?? false,
      );
      return Object.fromEntries(fields.map(([name, s]) => [name, example(document, s, seen)]));
    }
    case 'array':
      return schema.items ? [example(document, schema.items, seen)] : [];
    case 'integer':
    case 'number':
      return schema.minimum ?? 0;
    case 'boolean':
      return false;
    case 'string':
      return schema.format === 'uuid' ? '00000000-0000-0000-0000-000000000000' : '';
    default:
      return null;
  }
}

/** Whether an operation needs the bearer token. */
export function guarded(operation: Operation): boolean {
  return (operation.security ?? []).some((requirement) => Object.keys(requirement).length > 0);
}
