// The typed client. Every path, body and response comes from schema.d.ts,
// which `npm run api` generates from docs/openapi.json.
import createClient, { type Middleware } from 'openapi-fetch';
import { settings } from '@/settings';
import type { components, paths } from './schema';

export type Schemas = components['schemas'];
export type Wallet = Schemas['Wallet'];
export type WalletIndex = Schemas['WalletIndex'];
export type Movement = Schemas['Movement'];
export type Network = Schemas['Network'];
export type Listing = Schemas['Listing'];
export type Transaction = Schemas['Transaction'];
export type ConfiguredNetwork = Schemas['ConfiguredNetwork'];
export type Trigger = Schemas['Trigger'];
export type TriggerRun = Schemas['TriggerRun'];
export type Job = Schemas['Job'];
export type ChainBlock = Schemas['Block'];
export type ChainTransaction = Schemas['Transaction'];
export type User = Schemas['User'];
export type UserKey = Schemas['UserKey'];
export type UserRead = Schemas['UserRead'];
/** Scopes as the server packs them: `{"com.hashinbox": ["wallets.$read"]}`. */
export type Packed = Record<string, string[]>;

/** Raised for any non-2xx answer; `message` and `code` (its `reason`, or `code`) are the server's own. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly code?: string,
  ) {
    super(message);
  }
}

/** A string field of an error body, read whatever its schema. */
function field(body: unknown, name: string): string | undefined {
  if (!body || typeof body !== 'object' || !(name in body)) return undefined;
  const value: unknown = (body as Record<string, unknown>)[name];
  return typeof value === 'string' ? value : undefined;
}

/**
 * Routes that answer 401 for a wrong password or code, not a lost session:
 * the form shows it, and the next other call finds out if the session is gone.
 */
const ASKS = ['/v1/user/2fa', '/v1/user/2fa/confirm'];

let token: string | null = null;
let onUnauthorized: (() => void) | null = null;

export function setToken(value: string | null): void {
  token = value;
}

export function whenUnauthorized(handler: () => void): void {
  onUnauthorized = handler;
}

const auth: Middleware = {
  onRequest({ request }) {
    if (token) request.headers.set('Authorization', `Bearer ${token}`);
    return request;
  },
  onResponse({ request, response }) {
    // Only a refused token is news; a server that wants one when none was
    // sent is the ordinary locked state.
    const path = new URL(request.url).pathname;
    const asks = ASKS.some((route) => path.endsWith(route));
    if (response.status === 401 && token && !asks) onUnauthorized?.();
    return response;
  },
};

const baseUrl = settings.apiBase;

export const client = createClient<paths>({ baseUrl });

client.use(auth);

/**
 * Unwraps an openapi-fetch result, turning an error body into an ApiError.
 * Any 2xx is success; one with no body (204) resolves with no data.
 */
export async function call<T>(
  request: Promise<{ data?: T; error?: unknown; response: Response }>,
): Promise<T> {
  const { data, error, response } = await request;
  if (response.ok) return data as T;
  const message = field(error, 'message') ?? `${response.status} ${response.statusText}`;
  throw new ApiError(response.status, message, field(error, 'reason') ?? field(error, 'code'));
}

/** A key's id as its routes take it: without `hip/`. */
export function keyPath(id: string): string {
  return id.replace(/^hip\//, '');
}

/** One server-sent event: its name and its JSON data. */
export interface StreamEvent {
  event: string;
  data: unknown;
}

/**
 * Reads the `text/event-stream` at `path` with this tab's token, handing each
 * event to `onEvent`; resolves when the server ends the stream. Not
 * EventSource: it cannot send the bearer header.
 */
export async function events(
  path: string,
  onEvent: (event: StreamEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const headers = new Headers({ Accept: 'text/event-stream' });
  if (token) headers.set('Authorization', `Bearer ${token}`);
  const response = await fetch(`${baseUrl}${path}`, { headers, ...(signal && { signal }) });
  if (response.status === 401 && token) onUnauthorized?.();
  if (!response.ok || !response.body) {
    const body: unknown = await response.json().catch(() => null);
    const message =
      body && typeof body === 'object' && 'message' in body && typeof body.message === 'string'
        ? body.message
        : `${response.status} ${response.statusText}`;
    throw new ApiError(response.status, message);
  }
  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let buffer = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return;
    buffer += value;
    // An event ends at a blank line; its `data:` lines join with newlines.
    for (let end = buffer.indexOf('\n\n'); end >= 0; end = buffer.indexOf('\n\n')) {
      const lines = buffer.slice(0, end).split('\n');
      buffer = buffer.slice(end + 2);
      const event =
        lines
          .find((l) => l.startsWith('event:'))
          ?.slice(6)
          .trim() ?? 'message';
      const data = lines.filter((l) => l.startsWith('data:')).map((l) => l.slice(5).trimStart());
      if (data.length > 0) onEvent({ event, data: JSON.parse(data.join('\n')) as unknown });
    }
  }
}

/** The server's own OpenAPI document; public, so it needs no token. */
export async function openapi(): Promise<unknown> {
  const response = await fetch(`${baseUrl}/openapi.json`, {
    headers: { Accept: 'application/json' },
  });
  if (!response.ok)
    throw new ApiError(response.status, `${response.status} ${response.statusText}`);
  return response.json();
}

/** One request and its answer, whatever the status: the docs console's view. */
export interface Exchange {
  status: number;
  statusText: string;
  millis: number;
  contentType: string | null;
  body: string;
}

/**
 * Sends `method path` exactly as the docs console composed it, with this tab's
 * token when it holds one. Any status is an answer, not an error; only a
 * request that never reached the server throws.
 */
export async function exchange(method: string, path: string, body?: string): Promise<Exchange> {
  const headers = new Headers({ Accept: 'application/json' });
  if (body !== undefined) headers.set('Content-Type', 'application/json');
  if (token) headers.set('Authorization', `Bearer ${token}`);
  const started = performance.now();
  const response = await fetch(`${baseUrl}${path}`, {
    method: method.toUpperCase(),
    headers,
    ...(body === undefined ? {} : { body }),
  });
  const text = await response.text();
  return {
    status: response.status,
    statusText: response.statusText,
    millis: Math.round(performance.now() - started),
    contentType: response.headers.get('Content-Type'),
    body: text,
  };
}

/** The absolute URL of `path` on this server, for a copied command. */
export function address(path: string): string {
  return new URL(`${baseUrl}${path}`, window.location.href).toString();
}
