# 071 — One wallet row; adding a wallet in two steps

Status: built. Deviations: see "As built" at the end.

## Why

A wallet is not a row today. `wallet` holds only addresses whose key is kept
(`secret NOT NULL`), so a watch-only address exists only as `wallet_index`
rows, and its tags need their own `tag` table keyed by network and address.
Every view of wallets stitches `wallet`, `wallet_index` and `tag` together by
(chain, network, address). Being able to sign is a property of a wallet, not
a different entity.

## Model

```sql
-- Every address the server knows on a network, keyed or watch-only.
-- id: "{chain}.{network}.{address}".
CREATE TABLE wallet (
    id      TEXT NOT NULL PRIMARY KEY,
    chain   TEXT NOT NULL,
    network TEXT NOT NULL,
    address TEXT NOT NULL,
    curve   TEXT,            -- NULL: watch-only (no key kept)
    secret  BLOB,            -- NULL: watch-only
    tags    TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),  -- JSON list of tags
    CHECK (id = chain || '.' || network || '.' || address),
    CHECK ((curve IS NULL) = (secret IS NULL)),
    CHECK (curve IS NULL OR curve IN ('secp256k1', 'ed25519'))
);

-- One asset of one wallet being indexed. id: "{wallet_id}.{asset}".
CREATE TABLE wallet_index (
    id             TEXT    NOT NULL PRIMARY KEY,
    wallet_id      TEXT    NOT NULL REFERENCES wallet (id) ON DELETE CASCADE,
    asset          TEXT    NOT NULL,
    requested_from INTEGER NOT NULL,
    birthday       INTEGER NOT NULL,
    indexed_at     TEXT    REFERENCES block (id),
    state          INTEGER NOT NULL,
    reason         TEXT,
    CHECK (id = wallet_id || '.' || asset),
    CHECK (state IN (0, 1, 2))
);
```

- `tag` is dropped. A tag is a string in `wallet.tags` (`[a-z0-9-]{1,64}`,
  each once, kept sorted).
- `wallet_index` reads its chain, network and address through `wallet_id`;
  every query that filtered `wallet_index` by them joins `wallet`.
- The tag and "no tag" selectors become `json_each(wallet.tags)` checks.
- Keyed: `secret IS NOT NULL`. The key store (`base::WalletStore`) reads and
  writes only keyed rows; a watch-only wallet is a row without a key.
- Sending: a trigger's send needs every sender keyed — the round's keyed set
  is the wallets with a `secret`, not every wallet row — and the broadcast job
  signs with the key of that row. Importing a key for a watched address fills
  `curve` and `secret` on its row: from then on it can send; its history and
  tags stay. Recipients need only an address. `SendIntent` and the broadcast
  job do not change.
- No migrations: a fresh database.

## API

Wallets and their indexes are created separately, as the form does:

1. `POST /v1/wallets` — the wallet, one of:
   - `{"chain","network","generate":{}}` — a new key kept here;
   - `{"chain","network","secret":"…"}` — import a raw private key (Ethereum
     hex, Solana base58 secret/keypair, Bitcoin WIF); its address is derived;
     with `"address"` too, the two must match;
   - `{"chain","network","address":"…"}` — watch-only.
   Answers the wallet, with no indexes yet. An existing address answers it as
   is (importing a key for a watched address keeps the key).
2. `POST /v1/wallets/{id}/indexes` — `{"assets":[…],"start":"fresh"|position}`.
   A wallet created by `generate` takes only `"fresh"` (a new key has no
   past). `max_history_days` and the start rules of 070 apply.

Unchanged: pause/resume, tags (`PUT`/`DELETE /v1/wallets/{id}/tags/{tag}`;
`GET /v1/wallets/tags` lists the distinct tags), the wallet list (its `tags`
now come from the row).

Seed phrases and derivation paths are not part of this: a raw key only.
Derivation needs key derivation in `packages/crypto`/`sdk/wallets` and its own
approval.

## UI: "Add wallet", two steps

1. **Wallet** — network; then one of *Generate*, *Private key*, *Address*.
2. **Indexing** — assets; start *From now* or *From block* (number field).
   For a generated wallet the start is *From now* only.

"Watch more assets" on an existing wallet is step 2 alone.

## Order

1. Schema, models, `Stores`/key store, tag storage on the row; selectors.
2. `POST /v1/wallets` (three kinds) and `POST /v1/wallets/{id}/indexes`.
3. The streaming search (base progress callback + SSE endpoint).
4. The two-step form and the search dialog.

## As built

- Body of `POST /v1/wallets`: `{chain, network, secret?, address?}`, no
  `generate: {}`. `secret`: import (with `address`, it must match); `address`
  alone: watch; neither: generate. Unknown fields are refused (400). Created:
  201; an address already kept: 200 with its wallet (a watched one gains the
  key; tags and indexes stay).
- Indexes are a resource of the wallet, `api/wallet_indexes.rs`, one per
  asset: `POST /v1/wallets/{id}/indexes` `{asset, start?}` (one asset; `start`
  a native position, absent = from the tip, replacing `"fresh"`; it starts at
  once, the network's next indexing step picks it; an asset already indexed
  answers its index, 200); `PATCH /v1/wallets/{id}/indexes/{asset}`
  `{index_state: "paused"|"indexing"}` (pause, resume; `no_data` refused);
  `DELETE /v1/wallets/{id}/indexes/{asset}` (forgets the index and its
  progress; kept legs stay, no longer read). Create and patch answer the index
  (`WalletIndexProgress`). `POST /v1/wallets/{id}/pause|resume` are gone: 070's
  "pause and resume act on every index of the wallet" is now one call per
  index (the UI loops).
- Tag routes moved to `api/wallet_tags.rs` (paths unchanged).
- "A generated wallet takes only `fresh`" is not enforced by the server: with
  no stored flag it cannot tell a generated key from an imported one. The UI
  offers only *From now* in step 2 after *Generate* (not in "Watch more
  assets" later); the API documents it.
- Import: `base::WalletFactory::from_pk_text(text)` (each chain parses its
  format, then `from_pk`): Ethereum 64 hex (`0x` optional); Solana Base58 of
  the 32-byte secret or the 64-byte keypair (public half checked); Bitcoin WIF,
  compressed, of the configured network kind. A Bitcoin key derives the
  configured address type; another type's address is a mismatch.
- `GET /v1/wallets/tags` answers `{"tags": ["…"]}`: the distinct tags in use
  (`SELECT DISTINCT` over `json_each(wallet.tags)`), not per-wallet pairs; the
  UI reads a wallet's tags from `GET /v1/wallets`. `wallet_tags` indexes the
  column. The tag pattern is checked by the model, not the schema.
- The wallet list and read come from one model query,
  `wallet::db::find_with_indexes` (wallet ⟕ wallet_index ⟕ block); balances
  are the second query of `usecase::wallets::list`.
- `WalletIndexStore::insert` keeps a watch-only `wallet` row before the index
  (the FK), so `Indexer::watch` alone still works.
- Stats: `wallets` counts keyed rows; `addresses` counts distinct indexed
  wallets; `tags` sums the tags on every row.
- Search: `base::Dated::search(time, progress)` with `Probe {low, high}`
  (exported as `BlockProbe`); `progress` sees the first range (first block,
  tip) and every step after. The SSE route answers 400/404 as JSON for a bad
  `at` or an unconfigured network; a network without `Dated` is an `error`
  event. A client that leaves mid-search does not stop it (at most
  ~log2(tip) reads).
