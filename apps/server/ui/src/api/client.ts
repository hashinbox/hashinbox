// The typed client. Every path, body and response comes from schema.d.ts,
// which `npm run api` generates from docs/openapi.json.
import createClient, { type Middleware } from 'openapi-fetch';
import { settings } from '@/settings';
import type { components, paths } from './schema';

export type Schemas = components['schemas'];
export type Wallet = Schemas['Wallet'];
export type Listing = Schemas['Listing'];
export type Balance = Schemas['Balance'];
export type Transaction = Schemas['Transaction'];
export type TransactionPage = Schemas['TransactionPage'];
export type Report = Schemas['Report'];
export type ConfiguredNetwork = Schemas['ConfiguredNetwork'];
export type Tag = Schemas['Tag'];
export type Trigger = Schemas['Trigger'];
export type TriggerRun = Schemas['TriggerRun'];
export type TriggerPreview = Schemas['TriggerPreview'];

/** Raised for any non-2xx answer; `message` is the server's own. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

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
  onResponse({ response }) {
    // Only a refused token is news; a server that wants one when none was
    // sent is the ordinary locked state.
    if (response.status === 401 && token) onUnauthorized?.();
    return response;
  },
};

const baseUrl = settings.apiBase;

export const client = createClient<paths>({ baseUrl });

/**
 * The health report. `/health` answers 503 with the same body when the server
 * is not ready, which the typed client would treat as an error; a not-ready
 * report is still a report, so it is read directly.
 */
export async function health(): Promise<Report> {
  const response = await fetch(`${baseUrl}/health`, { headers: { Accept: 'application/json' } });
  return (await response.json()) as Report;
}
client.use(auth);

/** Unwraps an openapi-fetch result, turning an error body into an ApiError. */
export async function call<T>(
  request: Promise<{ data?: T; error?: unknown; response: Response }>,
): Promise<T> {
  const { data, error, response } = await request;
  if (data !== undefined) return data;
  const message =
    error && typeof error === 'object' && 'message' in error && typeof error.message === 'string'
      ? error.message
      : `${response.status} ${response.statusText}`;
  throw new ApiError(response.status, message);
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
