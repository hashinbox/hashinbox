// Hand-written: what the generated operations (operations.ts) call, and the
// fetch client that does it. cligen (via apps/server/build.rs) writes
// models.ts and operations.ts beside this; it never touches this file.

import type * as models from "./models";
import {
  sessionCreate,
  sessionDelete,
  sessionGetOperation,
} from "./operations";
import { fill, Scopes, type Packed } from "./scope";
import type { Secret } from "./secret";
import { Session } from "./session";

/** An HTTP method. */
export type Method = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

/** An operation's static description. */
export interface Operation {
  id: string;
  method: Method;
  /** The path template: `/v1/wallets/{id}`. */
  path: string;
  /** The request's fields that go in the query. */
  query?: readonly string[];
  /** The scope templates. */
  templates: readonly string[];
  /** A stream's events, by name. */
  events?: readonly string[];
}

/** One server-sent event, as read. */
export interface RawEvent {
  event: string;
  data: string;
  id?: string | undefined;
}

/** What holds scopes: a `Session`'s or a key's `Scopes`. */
export interface Granted {
  covers(scope: string): boolean;
}

/**
 * Sends `operation` for `request` (its path, query and body: see `path`,
 * `query`, `body`): the 2xx body decoded, else an `ApiError`.
 */
export interface Client {
  send<T>(
    operation: Operation,
    request: object,
    signal?: AbortSignal,
  ): Promise<T>;
}

/** Opens `operation` for `request` as a stream of events (see `events`). */
export interface Streamer {
  open<E>(
    operation: Operation,
    request: object,
    signal?: AbortSignal,
  ): EventStream<E>;
}

/**
 * A stream's events across its windows: the server ends each answer after
 * a window (15 s) with `event: close`, and the stream reconnects at once
 * with the last `id` it read as `cursor`; after a failed connect
 * (a network error, a 5xx or 429) it tries again after a backoff. It
 * reconnects until `stop()` (or its `signal`), an answer that ends without
 * `close` (the stream is done), or another error (thrown).
 */
export interface EventStream<E> extends AsyncIterable<E> {
  /** The last `id` read, sent as `cursor` on a reconnect. */
  readonly cursor: string | undefined;
  /** Ends it for good: the answer read now is dropped, no reconnect. */
  stop(): void;
}

/** The wait after a failed connect: `initial` ms, doubled each time up to `max`. */
export interface Backoff {
  initial: number;
  max: number;
}

/** The reasons an `ApiError` names, as the server's `error::reason`: what a client acts on. */
export const Reason = {
  /** A login's email, password or code is wrong (401). */
  WrongCredentials: "wrong_credentials",
  /** The password was right; the two-factor code is needed (401). */
  TotpRequired: "totp_required",
  /** The session is no longer accepted: log in again (401). */
  SessionExpired: "session_expired",
  /** A scope is missing; metadata `scope` names it (403). */
  MissingScope: "missing_scope",
} as const;
export type Reason = (typeof Reason)[keyof typeof Reason];

/** Every `Kind`, to check an answer's. */
const KINDS: Record<models.Kind, true> = {
  invalid_argument: true,
  failed_precondition: true,
  already_exists: true,
  not_found: true,
  unauthenticated: true,
  permission_denied: true,
  resource_exhausted: true,
  deadline_exceeded: true,
  unavailable: true,
  internal: true,
};

/** How much of an answer's text an `ApiError` keeps. */
const KEPT = 1024;

/** `body` when it is the API's error (`Error`): a known kind, a message. */
function errorOf(body: unknown): models.Error | undefined {
  if (typeof body !== "object" || body === null) return undefined;
  const error = body as Partial<Record<keyof models.Error, unknown>>;
  if (typeof error.kind !== "string" || !Object.hasOwn(KINDS, error.kind))
    return undefined;
  if (typeof error.message !== "string") return undefined;
  if (error.reason !== undefined && typeof error.reason !== "string")
    return undefined;
  return body as models.Error;
}

/** A non-2xx answer, its body read: what `ApiError.from` parses. */
class Answer {
  readonly status: number;
  readonly text: string;

  constructor(status: number, text: string) {
    this.status = status;
    this.text = text;
  }
}

/** An `ApiError`'s fields: `kind` `unavailable`, `status` 0, `metadata` empty unless set. */
export interface ApiErrorInit {
  kind?: models.Kind;
  message: string;
  /** What a client acts on: one of `Reason`. */
  reason?: string;
  metadata?: Record<string, string>;
  /** The answer's status; 0 when none came. */
  status?: number;
  cause?: unknown;
}

/**
 * Any failure as one error, the API's: `kind`, `message`, `reason`,
 * `metadata`, and the answer's `status` (0 when none came). A plain value;
 * `from` turns anything into one.
 */
export class ApiError extends Error {
  readonly status: number;
  readonly kind: models.Kind;
  /** What a client acts on: one of `Reason`. */
  readonly reason: string | undefined;
  readonly metadata: Record<string, string>;

  constructor({
    kind = "unavailable",
    message,
    reason,
    metadata = {},
    status = 0,
    cause,
  }: ApiErrorInit) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = "ApiError";
    this.status = status;
    this.kind = kind;
    this.reason = reason;
    this.metadata = metadata;
  }

  /**
   * `error` as an `ApiError`: itself when it is one; an answer in the API's
   * error shape that error, with its status; any other answer (an HTML 502
   * from a proxy, an empty body, other JSON, as `health_get`'s 503
   * `Report`) `unavailable`, its status, its text's first 1024 characters
   * as `metadata.response`; anything thrown (a network error, a timeout,
   * an abort, anything else) `unavailable`, its message, `error` as `cause`.
   */
  static from(error: unknown): ApiError {
    if (error instanceof ApiError) return error;
    if (!(error instanceof Answer))
      return new ApiError({
        message: error instanceof Error ? error.message : String(error),
        cause: error,
      });
    const { status, text } = error;
    let body: unknown;
    try {
      body = JSON.parse(text);
    } catch {
      body = undefined;
    }
    const parsed = errorOf(body);
    if (parsed) return new ApiError({ ...parsed, status });
    return new ApiError({
      message: `could not parse the response (HTTP ${status})`,
      metadata: { response: text.slice(0, KEPT) },
      status,
    });
  }
}

/** The server's 403 for a call `operation` lacking `scope`, refused here, unsent. */
function missingScope(operation: string, scope: string): ApiError {
  return new ApiError({
    kind: "permission_denied",
    message: `${operation} needs scope ${scope}`,
    reason: Reason.MissingScope,
    metadata: { scope, operation },
    status: 403,
  });
}

/** `operation`'s path for `request`, each `{name}` its field, URI-encoded. */
export function path(operation: Operation, request: object): string {
  return fill(operation.path, request, encodeURIComponent);
}

/** The query pairs of `request`: its fields `operation` names, if set. */
export function query(
  operation: Operation,
  request: object,
): [string, string][] {
  const values = request as Record<string, unknown>;
  return (operation.query ?? [])
    .filter((name) => values[name] !== undefined && values[name] !== null)
    .map((name): [string, string] => [name, String(values[name])]);
}

/** `request.body` as JSON, if it has one. */
export function body(request: object): string | undefined {
  const value = (request as { body?: unknown }).body;
  return value === undefined ? undefined : JSON.stringify(value);
}

/** The scopes `operation` needs for `request`, each hole escaped. */
export function scopes(operation: Operation, request: object): string[] {
  return operation.templates.map((template) => fill(template, request));
}

/** Whether `granted` holds every scope `operation` needs for `request`. */
export function can(
  granted: Granted,
  operation: Operation,
  request: object,
): boolean {
  return scopes(operation, request).every((scope) => granted.covers(scope));
}

/**
 * A stream's typed events from its raw ones: those `operation.events` names,
 * their data decoded (any other skipped); without names, every raw event.
 */
export async function* events<E>(
  operation: Operation,
  raw: AsyncIterable<RawEvent>,
): AsyncIterable<E> {
  for await (const event of raw) {
    const typed = typedEvent<E>(operation, event);
    if (typed !== undefined) yield typed;
  }
}

/** `event` typed by `operation.events`: undefined for one it does not name. */
function typedEvent<E>(operation: Operation, event: RawEvent): E | undefined {
  if (!operation.events) return event as E;
  if (!operation.events.includes(event.event)) return undefined;
  return {
    event: event.event,
    data: JSON.parse(event.data) as unknown,
    id: event.id,
  } as E;
}

/** Whether a failed connect is tried again: a network error, a 5xx or a 429. */
function retried(error: unknown): boolean {
  if (!(error instanceof ApiError)) return true;
  return error.status >= 500 || error.status === 429;
}

/** Waits `ms`, or until `signal` aborts. */
function pause(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

/** The default `Backoff`: half a second, up to 8. */
const BACKOFF: Backoff = { initial: 500, max: 8000 };

/** One window's answer body, from `cursor` on; null: none. */
type Connect = (
  cursor: string | undefined,
  signal: AbortSignal,
) => Promise<ReadableStream<Uint8Array> | null>;

/** An `EventStream` over `connect`: one answer's body per window. */
class Reconnecting<E> implements EventStream<E> {
  #cursor: string | undefined;
  readonly #stopped = new AbortController();
  readonly #signal: AbortSignal;
  readonly operation: Operation;
  readonly connect: Connect;
  readonly backoff: Backoff;

  constructor(
    operation: Operation,
    connect: Connect,
    backoff: Backoff,
    signal?: AbortSignal,
  ) {
    this.operation = operation;
    this.connect = connect;
    this.backoff = backoff;
    this.#signal = signal
      ? AbortSignal.any([signal, this.#stopped.signal])
      : this.#stopped.signal;
  }

  get cursor(): string | undefined {
    return this.#cursor;
  }

  stop(): void {
    this.#stopped.abort();
  }

  async *[Symbol.asyncIterator](): AsyncIterator<E> {
    const signal = this.#signal;
    let wait = this.backoff.initial;
    try {
      while (!signal.aborted) {
        let body: ReadableStream<Uint8Array> | null;
        try {
          body = await this.connect(this.#cursor, signal);
        } catch (error) {
          if (signal.aborted) return;
          if (!retried(error)) throw error;
          await pause(wait, signal);
          wait = Math.min(wait * 2, this.backoff.max);
          continue;
        }
        wait = this.backoff.initial;
        if (!body) return;
        let closed = false;
        try {
          for await (const event of readEvents(body)) {
            if (event.id !== undefined) this.#cursor = event.id;
            if (event.event === "close") closed = true;
            const typed = closed
              ? undefined
              : typedEvent<E>(this.operation, event);
            if (typed !== undefined) yield typed;
          }
        } catch (error) {
          if (signal.aborted) return;
          if (!retried(error)) throw error;
          await pause(wait, signal);
          wait = Math.min(wait * 2, this.backoff.max);
          continue;
        }
        if (!closed) return;
      }
    } finally {
      this.stop();
    }
  }
}

/** The 401 reasons that mean wrong input, not a lost session. */
const WRONG_INPUT: readonly string[] = [
  Reason.WrongCredentials,
  Reason.TotpRequired,
];

/** How long before its `exp` a self-signed session is replaced, seconds. */
const REFRESH = 60;

/** `HashinboxClient`'s options. */
export interface ClientOptions {
  /** The session to send (a JWT from a login, or from a backend). */
  session?: Session;
  /**
   * A key: the client signs its own sessions with it (its secret is never
   * sent), anew within 60 s of their expiry or on a 401 `session_expired`.
   */
  secret?: Secret;
  /** What a `secret`'s sessions hold; the key's own scopes (read once) by default. */
  scopes?: Scopes | Packed | string;
  /** A `secret`'s sessions' life, seconds: 600 by default, at most 86400. */
  ttl?: number;
  fetch?: typeof fetch;
  /** Milliseconds a call may take (a stream: until it answers); none by default. */
  timeout?: number;
  /** A stream's wait after a failed connect: 500 ms doubling up to 8 s by default. */
  backoff?: Backoff;
  /** Called with each new session (a login, a self-signed one), and `undefined` once it is dropped. */
  onSession?: (session: Session | undefined) => void;
  /** Called when the server refuses the session sent (a 401 other than wrong input). */
  onUnauthorized?: (error: ApiError) => void;
  /** Refuse locally (the server's `missing_scope` `ApiError`, unsent) a call the session's scopes do not cover. */
  checkScopes?: boolean;
}

/** What `login` sends. */
export interface Login {
  email: string;
  password?: string;
  /** The recovery passphrase, in place of `password`. */
  passphrase?: string;
  /** The two-factor code, once the server asks (`totp_required`). */
  totp?: string;
}

/**
 * The hashinbox API over `fetch`: a `Client` and a `Streamer` for every
 * generated operation, with the session it sends.
 */
export class HashinboxClient implements Client, Streamer {
  readonly baseUrl: string;
  #session: Session | undefined;
  #secret: Secret | undefined;
  #scopes: Scopes | undefined;
  #signing: Promise<Session> | undefined;
  #options: ClientOptions;

  constructor(baseUrl = "", options: ClientOptions = {}) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.#options = options;
    this.#session = options.session;
    this.#secret = options.secret;
    const scopes = options.scopes;
    this.#scopes =
      scopes === undefined || scopes instanceof Scopes
        ? scopes
        : Scopes.parse(scopes);
  }

  /** The session sent; setting it calls `onSession`. */
  get session(): Session | undefined {
    return this.#session;
  }

  set session(session: Session | undefined) {
    this.#session = session;
    this.#options.onSession?.(session);
  }

  async send<T>(
    operation: Operation,
    request: object,
    signal?: AbortSignal,
  ): Promise<T> {
    const { response, done } = await this.#exchange(operation, request, signal);
    try {
      const text = await response.text();
      return (text === "" ? undefined : JSON.parse(text)) as T;
    } finally {
      done();
    }
  }

  open<E>(
    operation: Operation,
    request: object,
    signal?: AbortSignal,
  ): EventStream<E> {
    return this.stream<E>(operation, request, signal);
  }

  /**
   * `operation`'s server-sent events, raw or typed by `operation.events`,
   * window after window (see `EventStream`) until `stop()`, `signal`, or
   * the stream's end.
   */
  stream<E = RawEvent>(
    operation: Operation,
    request: object = {},
    signal?: AbortSignal,
  ): EventStream<E> {
    const connect = async (
      cursor: string | undefined,
      stopped: AbortSignal,
    ) => {
      const from = cursor === undefined ? request : { ...request, cursor };
      const { response, done } = await this.#exchange(
        operation,
        from,
        stopped,
        "text/event-stream",
      );
      done();
      return response.body;
    };
    const backoff = this.#options.backoff ?? BACKOFF;
    return new Reconnecting<E>(operation, connect, backoff, signal);
  }

  /**
   * Logs in (no `Authorization` sent): the session kept and handed to
   * `onSession`. A wrong email, password, passphrase or code throws an
   * `ApiError` with reason `wrong_credentials`; a missing code,
   * `totp_required` (ask it, call again with `totp`).
   */
  async login(
    login: Login,
    signal?: AbortSignal,
  ): Promise<models.IssuedSession> {
    const issued = await sessionCreate(
      {
        send: <T>(op: Operation, request: object, sig?: AbortSignal) =>
          this.#anonymous<T>(op, request, sig),
      },
      { body: login },
      signal,
    );
    this.session = Session.parse(issued.token);
    return issued;
  }

  /**
   * Forgets the session; with `all`, first has the server refuse every
   * session its issuer issued (its error then thrown).
   */
  async logout({ all = false } = {}, signal?: AbortSignal): Promise<void> {
    try {
      await sessionDelete(this, { all }, signal);
    } catch (error) {
      if (all) throw error;
    } finally {
      this.session = undefined;
    }
  }

  /** `operation` sent with no `Authorization`, its 2xx body decoded. */
  async #anonymous<T>(
    operation: Operation,
    request: object,
    signal?: AbortSignal,
  ): Promise<T> {
    const { response, done } = await this.#exchange(
      operation,
      request,
      signal,
      undefined,
      true,
    );
    try {
      return (await response.json()) as T;
    } finally {
      done();
    }
  }

  /**
   * `operation` sent with the session (signed anew once on a
   * `session_expired` when a `secret` signs them): the 2xx response, else
   * its `ApiError` thrown.
   */
  async #exchange(
    operation: Operation,
    request: object,
    signal: AbortSignal | undefined,
    accept = "application/json",
    anonymous = false,
  ): Promise<{ response: Response; done: () => void }> {
    for (let attempt = 0; ; attempt++) {
      const session = anonymous ? undefined : await this.#current();
      const missing =
        session && this.#options.checkScopes !== false
          ? scopes(operation, request).find(
              (scope) => !session.scopes.covers(scope),
            )
          : undefined;
      if (missing !== undefined) throw missingScope(operation.id, missing);
      const sent = await this.#fetch(
        operation,
        request,
        signal,
        accept,
        session,
      );
      if (sent.response.ok) return sent;
      const text = await sent.response.text().finally(sent.done);
      const error = ApiError.from(new Answer(sent.response.status, text));
      const expired =
        error.status === 401 && error.reason === Reason.SessionExpired;
      if (expired && this.#secret && attempt === 0) {
        this.#session = undefined;
        continue;
      }
      const wrongInput = WRONG_INPUT.includes(error.reason ?? "");
      if (error.status === 401 && session && !wrongInput)
        this.#options.onUnauthorized?.(error);
      throw error;
    }
  }

  /** One request: its response once it answers, `done` ending its timeout. */
  async #fetch(
    operation: Operation,
    request: object,
    signal: AbortSignal | undefined,
    accept: string,
    session: Session | undefined,
  ): Promise<{ response: Response; done: () => void }> {
    const pairs = query(operation, request);
    const search = pairs.length
      ? `?${new URLSearchParams(pairs).toString()}`
      : "";
    const url = `${this.baseUrl}${path(operation, request)}${search}`;
    const headers = new Headers({ Accept: accept });
    const json = body(request);
    if (json !== undefined) headers.set("Content-Type", "application/json");
    if (session) headers.set("Authorization", `Bearer ${session.token}`);
    const timeout = new AbortController();
    const timer =
      this.#options.timeout === undefined
        ? undefined
        : setTimeout(
            () => timeout.abort(new DOMException("timed out", "TimeoutError")),
            this.#options.timeout,
          );
    const done = () => clearTimeout(timer);
    const signals = signal ? [signal, timeout.signal] : [timeout.signal];
    try {
      const response = await (this.#options.fetch ?? fetch)(url, {
        method: operation.method,
        headers,
        body: json ?? null,
        signal: AbortSignal.any(signals),
      });
      return { response, done };
    } catch (error) {
      done();
      throw error;
    }
  }

  /** The session to send: the one kept, or one the `secret` signs when it lapses within a minute. */
  async #current(): Promise<Session | undefined> {
    const kept = this.#session;
    const secret = this.#secret;
    if (!secret) return kept;
    if (kept && kept.expiresAt - Date.now() / 1000 > REFRESH) return kept;
    this.#signing ??= this.#sign(secret).finally(
      () => (this.#signing = undefined),
    );
    return this.#signing;
  }

  /** A new session of `secret`: `scopes`, or the key's own, read with a scopeless session. */
  async #sign(secret: Secret): Promise<Session> {
    if (!this.#scopes) {
      const probe = await secret.issue({ scopes: "", ttl: REFRESH });
      const { response, done } = await this.#fetch(
        sessionGetOperation,
        {},
        undefined,
        "application/json",
        probe,
      );
      const text = await response.text().finally(done);
      if (!response.ok) throw ApiError.from(new Answer(response.status, text));
      const read = JSON.parse(text) as models.SessionRead;
      this.#scopes = Scopes.parse(read.key?.scopes ?? {});
    }
    const session = await secret.issue({
      scopes: this.#scopes,
      ttl: this.#options.ttl ?? 600,
    });
    this.session = session;
    return session;
  }
}

/** The events of a `text/event-stream` body. */
async function* readEvents(
  body: ReadableStream<Uint8Array>,
): AsyncIterable<RawEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let event: { event?: string; data: string[]; id?: string | undefined } = {
    data: [],
  };
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer += decoder.decode(value, { stream: true });
      // A `\r` at the end may be half of a `\r\n`: kept for the next chunk.
      const lines = buffer.split(/\r\n|\r(?!$)|\n/);
      buffer = lines.pop() ?? "";
      for (const line of lines) {
        if (line === "") {
          if (event.data.length > 0)
            yield {
              event: event.event ?? "message",
              data: event.data.join("\n"),
              id: event.id,
            };
          event = { data: [], id: event.id };
          continue;
        }
        const colon = line.indexOf(":");
        if (colon === 0) continue;
        const field = colon < 0 ? line : line.slice(0, colon);
        const value = colon < 0 ? "" : line.slice(colon + 1).replace(/^ /, "");
        if (field === "event") event.event = value;
        else if (field === "data") event.data.push(value);
        else if (field === "id") event.id = value;
      }
    }
  } finally {
    await reader.cancel().catch(() => undefined);
  }
}
