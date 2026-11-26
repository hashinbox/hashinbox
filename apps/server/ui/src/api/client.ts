// The API: one HashinboxClient (@hashinbox/client) for the whole app. Pages
// call its generated operations (`walletGet(api, { id })`) and take their
// types from its models; this tab's session is kept in sessionStorage.
import { HashinboxClient, Session } from '@hashinbox/client';
import { settings } from '@/settings';

export { ApiError } from '@hashinbox/client';

/** Where this tab keeps its session's JWT. */
const KEY = 'hashinbox.session';

/** The session this tab kept, if any (and readable). */
function stored(): Session | undefined {
  try {
    const token = sessionStorage.getItem(KEY);
    return token === null ? undefined : Session.parse(token);
  } catch {
    return undefined;
  }
}

function store(session: Session | undefined): void {
  try {
    if (session) sessionStorage.setItem(KEY, session.token);
    else sessionStorage.removeItem(KEY);
  } catch {
    /* private window */
  }
}

let unauthorized: (() => void) | undefined;

const restored = stored();

export const api = new HashinboxClient(settings.apiBase, {
  ...(restored && { session: restored }),
  onSession: store,
  // A 401 for a session sent (not a wrong password or code): it is gone.
  onUnauthorized: () => {
    api.session = undefined;
    unauthorized?.();
  },
  // The server decides; a missing scope is its 403, shown as it words it.
  checkScopes: false,
});

/** Called once the server refuses this tab's session. */
export function whenUnauthorized(handler: () => void): void {
  unauthorized = handler;
}

/** A key's id as its routes take it: without `hip/`. */
export function keyPath(id: string): string {
  return id.replace(/^hip\//, '');
}

const baseUrl = settings.apiBase;

// Raw requests, kept outside the SDK on purpose: the OpenAPI document (read
// with no session), and the docs console's arbitrary request (any method,
// path and body, any status an answer).

/** The server's own OpenAPI document; public, so it needs no token. */
export async function openapi(): Promise<unknown> {
  const response = await fetch(`${baseUrl}/openapi.json`, {
    headers: { Accept: 'application/json' },
  });
  if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
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
  if (api.session) headers.set('Authorization', `Bearer ${api.session.token}`);
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
