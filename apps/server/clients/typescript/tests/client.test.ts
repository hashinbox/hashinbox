// HashinboxClient against a fake fetch: URLs, bearer, errors, 401 rules,
// self-signed sessions, login, SSE.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  ApiError,
  HashinboxClient,
  keyRevoke,
  logsStream,
  MissingScope,
  Secret,
  Session,
  versionGet,
  walletGet,
  walletsList,
} from "../src/index";

const vectors = JSON.parse(
  readFileSync(new URL("../../_golden/session.json", import.meta.url), "utf8"),
) as { id: string; secret: string; user: { token: string } };

interface Sent {
  url: string;
  method: string;
  auth: string | null;
  body: string | undefined;
}

/** A fetch answering each call with the next of `answers`, recording what it was sent. */
function fake(...answers: (() => Response)[]) {
  const sent: Sent[] = [];
  const fetch = (url: string | URL | Request, init?: RequestInit) => {
    const headers = new Headers(init?.headers);
    sent.push({
      url: String(url),
      method: init?.method ?? "GET",
      auth: headers.get("Authorization"),
      body: init?.body as string | undefined,
    });
    const answer = answers.shift();
    assert.ok(answer, `unexpected call ${String(url)}`);
    return Promise.resolve(answer());
  };
  return { fetch: fetch as typeof globalThis.fetch, sent };
}

const json = (status: number, body: unknown) => () =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
const empty = () => new Response(null, { status: 204 });

const user = Session.parse(vectors.user.token);

test("send builds the URL, query and body, sends the bearer, decodes", async () => {
  const { fetch, sent } = fake(json(200, { wallets: [] }), empty);
  const client = new HashinboxClient("https://h.example/", {
    session: user,
    fetch,
  });
  assert.deepEqual(await walletsList(client, { limit: 5 }), { wallets: [] });
  assert.equal(await keyRevoke(client, { id: "hip/a b" }), undefined);
  assert.equal(sent[0].url, "https://h.example/v1/wallets?limit=5");
  assert.equal(sent[0].auth, `Bearer ${user.token}`);
  assert.equal(sent[1].url, "https://h.example/v1/user/keys/hip%2Fa%20b");
  assert.equal(sent[1].method, "DELETE");
});

test("an error body is an ApiError; 401 calls onUnauthorized but for wrong input", async () => {
  const unauthorized: ApiError[] = [];
  const { fetch } = fake(
    json(404, { kind: "not_found", message: "no such wallet" }),
    json(401, {
      kind: "unauthenticated",
      message: "no",
      reason: "session_expired",
    }),
    json(401, {
      kind: "unauthenticated",
      message: "no",
      reason: "wrong_credentials",
    }),
  );
  const client = new HashinboxClient("", {
    session: user,
    fetch,
    onUnauthorized: (error) => unauthorized.push(error),
  });
  const notFound = await walletGet(client, { id: "a.b.c" }).catch(
    (e: unknown) => e,
  );
  assert.ok(notFound instanceof ApiError);
  assert.equal(notFound.status, 404);
  assert.equal(notFound.kind, "not_found");
  assert.equal(notFound.message, "no such wallet");
  await assert.rejects(walletGet(client, { id: "a.b.c" }));
  await assert.rejects(walletGet(client, { id: "a.b.c" }), {
    reason: "wrong_credentials",
  });
  assert.deepEqual(
    unauthorized.map((e) => e.reason),
    ["session_expired"],
  );
});

test("a call the session cannot make is refused locally", async () => {
  const key = Secret.parse(vectors.id, vectors.secret);
  const narrow = await key.issue({
    scopes: "com.hashinbox.wallets[a.b.c].$read",
  });
  const { fetch, sent } = fake(json(200, {}));
  const client = new HashinboxClient("", { session: narrow, fetch });
  await walletGet(client, { id: "a.b.c" });
  await assert.rejects(walletGet(client, { id: "a.b.d" }), MissingScope);
  assert.equal(sent.length, 1);
  const unchecked = new HashinboxClient("", {
    session: narrow,
    checkScopes: false,
    fetch: fake(json(403, { kind: "permission_denied", message: "no" })).fetch,
  });
  await assert.rejects(walletGet(unchecked, { id: "a.b.d" }), { status: 403 });
});

test("a secret signs its own sessions: the key's scopes read once, re-signed on session_expired", async () => {
  const key = Secret.parse(vectors.id, vectors.secret);
  const sessions: (Session | undefined)[] = [];
  const { fetch, sent } = fake(
    json(200, { key: { scopes: { "com.hashinbox": ["wallets.$read"] } } }),
    json(200, { id: "a.b.c" }),
    json(401, {
      kind: "unauthenticated",
      message: "x",
      reason: "session_expired",
    }),
    json(200, { id: "a.b.c" }),
  );
  const client = new HashinboxClient("", {
    secret: key,
    fetch,
    onSession: (session) => sessions.push(session),
  });
  await walletGet(client, { id: "a.b.c" });
  await walletGet(client, { id: "a.b.c" });
  assert.equal(sent.length, 4);
  assert.ok(
    sent.every((s) => s.auth?.startsWith("Bearer ey")),
    "never the secret",
  );
  assert.equal(Session.parse(sent[0].auth!.slice(7)).scopes.toString(), "");
  const signed = Session.parse(sent[1].auth!.slice(7));
  assert.equal(signed.scopes.toString(), "com.hashinbox.wallets.$read");
  assert.equal(signed.expiresAt - signed.issuedAt, 600);
  assert.equal(sessions.length, 2);
  assert.ok(sessions.every((s) => s?.issuer === vectors.id));
});

test("a second session_expired surfaces", async () => {
  const key = Secret.parse(vectors.id, vectors.secret);
  const expired = json(401, {
    kind: "unauthenticated",
    message: "x",
    reason: "session_expired",
  });
  const { fetch, sent } = fake(expired, expired);
  const client = new HashinboxClient("", {
    secret: key,
    scopes: "com.hashinbox.*.$*",
    fetch,
  });
  await assert.rejects(versionGet(client), { reason: "session_expired" });
  assert.equal(sent.length, 2);
});

test("login sends no bearer, keeps the session; totp_required surfaces", async () => {
  const sessions: (Session | undefined)[] = [];
  const { fetch, sent } = fake(
    json(401, {
      kind: "unauthenticated",
      message: "code",
      reason: "totp_required",
    }),
    json(200, { token: vectors.user.token, expires_at: 1, scopes: {} }),
    empty,
  );
  const client = new HashinboxClient("", {
    session: user,
    fetch,
    onSession: (s) => sessions.push(s),
    onUnauthorized: () => assert.fail("not a lost session"),
  });
  await assert.rejects(client.login({ email: "a@b", password: "p" }), {
    reason: "totp_required",
  });
  await client.login({ email: "a@b", password: "p", totp: "123456" });
  assert.equal(sent[0].auth, null);
  assert.deepEqual(JSON.parse(sent[1].body!), {
    email: "a@b",
    password: "p",
    totp: "123456",
  });
  assert.equal(client.session?.token, vectors.user.token);
  await client.logout({ all: true });
  assert.equal(sent[2].url, "/v1/session?all=true");
  assert.equal(client.session, undefined);
  assert.equal(sessions.length, 2);
});

test("a stream yields its events, split across chunks", async () => {
  const chunks = [
    ': keep-alive\n\nevent: missed\ndata: {"missed"',
    ": 2}\r",
    '\n\r\ndata: {"id":0}\nid: 7\n\n',
  ];
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks)
        controller.enqueue(new TextEncoder().encode(chunk));
      controller.close();
    },
  });
  const { fetch, sent } = fake(() => new Response(body, { status: 200 }));
  const client = new HashinboxClient("", { session: user, fetch });
  const seen = [];
  for await (const event of logsStream(client, { level: "info" }))
    seen.push(event);
  assert.deepEqual(seen, [
    { event: "missed", data: '{"missed": 2}', id: undefined },
    { event: "message", data: '{"id":0}', id: "7" },
  ]);
  assert.equal(sent[0].url, "/v1/logs/stream?level=info");
});

test("a timeout aborts the call", async () => {
  const fetch = ((_url: string, init?: RequestInit) =>
    new Promise((_, reject) =>
      init?.signal?.addEventListener("abort", () =>
        reject(init.signal?.reason),
      ),
    )) as typeof globalThis.fetch;
  const client = new HashinboxClient("", { fetch, timeout: 10 });
  await assert.rejects(versionGet(client), { name: "TimeoutError" });
});
