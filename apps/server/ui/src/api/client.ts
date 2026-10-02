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
export type Fee = Schemas['Fee'];

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
 * The health report. `/healthz` answers 503 with the same body when the server
 * is not ready, which the typed client would treat as an error; a not-ready
 * report is still a report, so it is read directly.
 */
export async function health(): Promise<Report> {
  const response = await fetch(`${baseUrl}/healthz`, { headers: { Accept: 'application/json' } });
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
