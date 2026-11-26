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
  id?: string;
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
  ): AsyncIterable<E>;
}

/**
 * A non-2xx answer: its status, the API error body's fields (`kind`,
 * `message`, `reason`, `metadata`), and the body as sent (`text`, `body`
 * when JSON: another model, as `health_get`'s 503 `Report`).
 */
export class ApiError extends Error {
  readonly status: number;
  readonly kind: models.Kind | undefined;
  /** What a client acts on: `wrong_credentials`, `totp_required`, `session_expired`, `missing_scope`. */
  readonly reason: string | undefined;
  readonly metadata: Record<string, string>;
  readonly text: string;
  readonly body: unknown;

  constructor(status: number, text: string) {
    let body: unknown;
    try {
      body = JSON.parse(text);
    } catch {
      body = undefined;
    }
    const error = (
      typeof body === "object" && body ? body : {}
    ) as Partial<models.Error>;
    super(typeof error.message === "string" ? error.message : `HTTP ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.kind = error.kind;
    this.reason = error.reason;
    this.metadata = error.metadata ?? {};
    this.text = text;
    this.body = body;
  }

  /** `response`'s error, its body read. */
  static async read(response: Response): Promise<ApiError> {
    return new ApiError(response.status, await response.text());
  }
}

/** A call the session cannot make: refused here, before it is sent. */
export class MissingScope extends Error {
  readonly operation: string;
  readonly scope: string;

  constructor(operation: string, scope: string) {
    super(`${operation} needs scope ${scope}`);
    this.name = "MissingScope";
    this.operation = operation;
    this.scope = scope;
  }
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
    if (!operation.events) {
      yield event as E;
    } else if (operation.events.includes(event.event)) {
      yield {
        event: event.event,
        data: JSON.parse(event.data) as unknown,
        id: event.id,
      } as E;
    }
  }
}

/** The 401 reasons that mean wrong input, not a lost session. */
const WRONG_INPUT = ["wrong_credentials", "totp_required"];

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
  /** Called with each new session (a login, a self-signed one), and `undefined` once it is dropped. */
  onSession?: (session: Session | undefined) => void;
  /** Called when the server refuses the session sent (a 401 other than wrong input). */
  onUnauthorized?: (error: ApiError) => void;
  /** Refuse locally (`MissingScope`) a call the session's scopes do not cover. */
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
  ): AsyncIterable<E> {
    return this.stream<E>(operation, request, signal);
  }

  /** `operation`'s server-sent events: raw, or typed by `operation.events`; ends with the stream or `signal`. */
  async *stream<E = RawEvent>(
    operation: Operation,
    request: object = {},
    signal?: AbortSignal,
  ): AsyncIterable<E> {
    const { response, done } = await this.#exchange(
      operation,
      request,
      signal,
      "text/event-stream",
    );
    done();
    if (!response.body) return;
    yield* events<E>(operation, readEvents(response.body));
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
      if (missing !== undefined) throw new MissingScope(operation.id, missing);
      const sent = await this.#fetch(
        operation,
        request,
        signal,
        accept,
        session,
      );
      if (sent.response.ok) return sent;
      const error = await ApiError.read(sent.response).finally(sent.done);
      const expired =
        error.status === 401 && error.reason === "session_expired";
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
        body: json,
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
      if (!response.ok) throw new ApiError(response.status, text);
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
  let event: { event?: string; data: string[]; id?: string } = { data: [] };
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
