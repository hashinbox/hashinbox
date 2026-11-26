// Against a running server: skipped unless HASHINBOX_URL is set (with
// HASHINBOX_EMAIL, HASHINBOX_PASSWORD and, with two-factor on, HASHINBOX_TOTP).
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  HashinboxClient,
  MissingScope,
  networksList,
  sessionCreate,
  Session,
  versionGet,
  walletsList,
} from "../src/index";

const url = process.env.HASHINBOX_URL;
const email = process.env.HASHINBOX_EMAIL ?? "";
const password = process.env.HASHINBOX_PASSWORD ?? "";
const totp = process.env.HASHINBOX_TOTP;

test(
  "login, version, a scoped call allowed and one refused",
  { skip: !url },
  async () => {
    const client = new HashinboxClient(url, { timeout: 10_000 });
    assert.ok((await versionGet(client)).version);
    await client.login({ email, password, totp });
    assert.ok(client.session);

    const issued = await sessionCreate(new HashinboxClient(url), {
      body: {
        email,
        password,
        totp,
        scopes: { "com.hashinbox": ["wallets.$read"] },
      },
    });
    const narrow = Session.parse(issued.token);
    const checked = new HashinboxClient(url, { session: narrow });
    assert.ok(Array.isArray((await walletsList(checked)).wallets));
    await assert.rejects(networksList(checked), MissingScope);
    const unchecked = new HashinboxClient(url, {
      session: narrow,
      checkScopes: false,
    });
    await assert.rejects(networksList(unchecked), {
      status: 403,
      reason: "missing_scope",
    });
  },
);
