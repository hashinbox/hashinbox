// ../_golden/session.json, written by the server's own `Session::to_jwt`.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { Secret, Session, walletGetOperation } from "../src/index";

interface Vectors {
  id: string;
  secret: string;
  sessions: {
    sub: string | null;
    scopes: string;
    iat: number;
    exp: number;
    token: string;
  }[];
  user: {
    token: string;
    iss: string;
    sub: string;
    iat: number;
    exp: number;
    scopes: string;
    amr: string[];
  };
}

const vectors = JSON.parse(
  readFileSync(new URL("../../_golden/session.json", import.meta.url), "utf8"),
) as Vectors;

test("a key's session is the server's, byte for byte", async () => {
  const secret = Secret.parse(vectors.id, vectors.secret);
  for (const vector of vectors.sessions) {
    const session = await secret.issue({
      scopes: vector.scopes,
      subject: vector.sub ?? undefined,
      ttl: vector.exp - vector.iat,
      issuedAt: vector.iat,
    });
    assert.equal(session.token, vector.token);
    assert.equal(session.issuer, vectors.id);
  }
});

test("a secret with a typo, too long a session are refused", async () => {
  const typo =
    vectors.secret.slice(0, 10) + (vectors.secret[10] === "a" ? "b" : "a");
  assert.throws(() =>
    Secret.parse(vectors.id, typo + vectors.secret.slice(11)),
  );
  assert.throws(() => Secret.parse(vectors.id, "his/0OIl"));
  assert.throws(() => Secret.parse("user/x", vectors.secret));
  const secret = Secret.parse(vectors.id, vectors.secret);
  await assert.rejects(secret.issue({ scopes: "", ttl: 86401 }));
});

test("a server-made user session parses", () => {
  const { user } = vectors;
  const session = Session.parse(user.token);
  assert.equal(session.issuer, user.iss);
  assert.equal(session.subject, user.sub);
  assert.equal(session.issuedAt, user.iat);
  assert.equal(session.expiresAt, user.exp);
  assert.equal(session.scopes.toString(), user.scopes);
  assert.deepEqual(session.amr, user.amr);
  assert.equal(session.expired(user.exp - 1), false);
  assert.equal(session.expired(user.exp), true);
  assert.equal(session.can(walletGetOperation, { id: "a.b.c" }), true);
  assert.throws(() => Session.parse("a.b"));
});

test("can() fills each hole, escaped", () => {
  const session = Session.parse(vectors.sessions[0].token);
  assert.equal(
    session.can(walletGetOperation, { id: "ethereum.mainnet.0xabc" }),
    true,
  );
  assert.equal(
    session.can(walletGetOperation, { id: "ethereum.mainnet.0xdef" }),
    false,
  );
});
