-- The SDK schema: indexing's tables (block, transaction, transaction_address,
-- ledger, output, network_checkpoint, wallet_index) and the wallets crate's
-- table (wallet), in the one SQLite file the embedder opens. Applied once, to
-- an empty file; there are no migrations: a schema change edits this file and
-- needs a fresh database. The application adds its own tables beside these
-- (schema/server.sql is the shipped example); it may read these, never write.
--
-- IDs are canonical "<chain>:<network>:<native>" strings with unambiguous
-- escaping of components. Constructors validate IDs, exact atomic amounts and
-- network agreement across references. Enable foreign_keys on every
-- connection. Persist confirmed facts only; pending blocks live in memory.
--
-- Ownership: `wallet` belongs to the wallets crate — identity (one address of
-- one network), key material and application metadata. `wallet_index` and
-- everything above it belongs to indexing — the observation of a wallet's
-- address per asset: its birthday and a progress cursor into `block`.
-- Scanning never reads key material; the join below reads identity only.
-- Facts are written only for what is watched: ledger rows for a watched
-- (address, asset) from its birthday, memberships and transactions for an
-- address watched in some asset — never for an unwatched counterparty.
-- Unwatch deletes, in one transaction, the cursor row and the watch's facts:
-- its ledger rows and outputs, and when no other asset of the address stays
-- watched its memberships, tombstones and every transaction no membership
-- names any more. A re-watch states its birthday again and is inspected from
-- there.
--
-- Finality is not a table. Each chain states its confirmed line at indexer
-- construction — Bitcoin a depth constant, Ethereum and Solana ask their
-- node — and network_checkpoint records where that line has been verified to.
--
-- Reorg recovery is explicit and offline for the affected network: pause it,
-- then delete the orphan blocks — CASCADE removes their transactions,
-- memberships, ledger rows and outputs. First clear the spend reference on
-- retained outputs (else the cascade takes them too)
-- whose spend was orphaned (the reference says when, not who; a retained
-- creation spent in an orphan block is unspent again). Restore the
-- affected cursors to the last retained inspected block, or delete the row
-- when none remains — never advance a lagging wallet to the common ancestor;
-- its own inspection must prove that. Identities, birthdays, keys, archive
-- state and application metadata stay. A rescan deletes every block of its
-- network: cascades take the facts, cursors fall back to NULL. Replay replacement blocks from the
-- restored cursors; replay is idempotent — matching facts are no-ops,
-- conflicting facts fail. Readers see the repair atomically
-- (BEGIN IMMEDIATE … COMMIT).
--
-- Balances are running totals in `ledger`: a holding's (address, asset)
-- balance is the `total` of its latest retained row in chain order
-- (block_position, movement_order, id); none means zero. Per-holding history
-- is forward-only: a row is only ever appended after the holding's tail.
-- When a holding would get a row at or before its tail (a backfill, a new
-- watch's birthday), its own rows from
-- that block position forward are deleted and written again in order; totals
-- are never rewritten in place. A watch whose inspected rows were deleted that
-- way has its cursor moved back below them, in the same transaction, so its
-- own inspection writes them again. Repair deletes a suffix, which leaves every
-- retained total correct.

CREATE TABLE block (
    id        TEXT    NOT NULL, -- <chain:network:hash>
    chain     TEXT    NOT NULL,
    network   TEXT    NOT NULL,
    hash      BLOB    NOT NULL,
    position  INTEGER NOT NULL,
    height    INTEGER NOT NULL,
    timestamp INTEGER,           -- Unix seconds; NULL when unavailable.
    parents   TEXT    NOT NULL DEFAULT '[]', -- Native parent refs; genesis '[]'.
    PRIMARY KEY (id),
    UNIQUE (chain, network, hash),
    UNIQUE (chain, network, position)
) WITHOUT ROWID;
CREATE INDEX block_height ON block (chain, network, height);

-- One current canonical inclusion per native transaction identity.
CREATE TABLE "transaction" (
    id                TEXT    NOT NULL, -- <chain:network:transaction_id>
    block_id          TEXT    NOT NULL REFERENCES block (id) ON DELETE CASCADE,
    transaction_id    TEXT    NOT NULL,
    transaction_order INTEGER NOT NULL,
    execution_status  INTEGER NOT NULL,
    failure_reason    TEXT,
    PRIMARY KEY (id),
    CHECK (execution_status IN (0, 1)),
    CHECK (transaction_order >= 0)
) WITHOUT ROWID;
CREATE INDEX transaction_block ON "transaction" (block_id, transaction_order, id);

-- One address's memberships; the history pages walk this key.
CREATE TABLE transaction_address (
    transaction_id TEXT NOT NULL REFERENCES "transaction" (id) ON DELETE CASCADE,
    address_id     TEXT NOT NULL, -- <chain:network:address>; no wallet FK.
    PRIMARY KEY (transaction_id, address_id)
) WITHOUT ROWID;
CREATE INDEX transaction_address_history
    ON transaction_address (address_id, transaction_id);

-- Individual movements, each with the running total after it. Transactionless
-- credits use a NULL transaction. The sign of diff is the direction (negative:
-- value left the address). movement_order is the in-block order: 2 x the
-- movement's index in its transaction, plus 1 for the debit side, so a
-- self-transfer's credit sorts before its debit; a fee is i64::MAX (last), a
-- withdrawal 0; ties break on id. kind 0..4 movement, 5 fee, 6 withdrawal.
-- A fee is a debit of the charged address in the charged asset,
-- naming the transaction responsible; history reads fees here. Its
-- movement_id is '' when the chain charges it beside the movements, or
-- 'embedded' when the movements already contain it (Bitcoin: inputs minus
-- outputs) — an embedded fee is shown, its diff the negative fee, and adds
-- nothing to total. The id deterministically includes block, transaction (or
-- credit namespace), movement, address, asset and side, and
-- deduplicates transactionless retries.
CREATE TABLE ledger (
    id             TEXT    NOT NULL,
    block_id       TEXT    NOT NULL REFERENCES block (id) ON DELETE CASCADE,
    transaction_id TEXT    REFERENCES "transaction" (id) ON DELETE CASCADE,
                                     -- NULL: a credit.
    address_id     TEXT    NOT NULL, -- No wallet FK.
    asset          TEXT    NOT NULL,
    movement_id    TEXT    NOT NULL,
    block_position INTEGER NOT NULL, -- The block's position, for chain order.
    movement_order INTEGER NOT NULL,
    kind           INTEGER NOT NULL,
    diff           TEXT    NOT NULL, -- Signed exact atomic change.
    total          TEXT    NOT NULL, -- Signed running total after this row.
    counterparty   TEXT,
    PRIMARY KEY (id),
    CHECK (kind BETWEEN 0 AND 6),
    CHECK (movement_order >= 0)
) WITHOUT ROWID;
CREATE INDEX ledger_address_asset
    ON ledger (address_id, asset, block_position, movement_order, id);
CREATE INDEX ledger_transaction ON ledger (transaction_id, movement_order, id);

-- Current canonical output state. The spend reference (to the block that
-- spent it, not the spender — the spender usually is not a stored
-- transaction) makes destructive rollback reversible. A spend may arrive
-- before its creation: the tombstone holds identity, address and the spend
-- reference, and a later creation fills the creation fields without clearing
-- the spend. NULL spent_block_id means unspent.
CREATE TABLE output (
    id                   TEXT    NOT NULL, -- <chain:network:creating_tx:index>
    chain                TEXT    NOT NULL,
    network              TEXT    NOT NULL,
    transaction_id       TEXT    NOT NULL,  -- Native creator ID.
    output_index         INTEGER NOT NULL,
    address_id           TEXT,              -- No wallet FK; some scripts have none.
    asset                TEXT,
    amount               TEXT,
    evidence             BLOB,
    created_block_id     TEXT    REFERENCES block (id) ON DELETE CASCADE,
    coinbase             INTEGER,
    spent_block_id       TEXT    REFERENCES block (id) ON DELETE CASCADE,
                                         -- when, not who.
    PRIMARY KEY (id),
    UNIQUE (chain, network, transaction_id, output_index),
    CHECK (output_index >= 0),
    CHECK (
        (asset IS NULL AND amount IS NULL AND evidence IS NULL
         AND created_block_id IS NULL AND coinbase IS NULL
         AND spent_block_id IS NOT NULL)   -- spend-first tombstone
        OR
        (asset IS NOT NULL AND amount IS NOT NULL AND evidence IS NOT NULL
         AND created_block_id IS NOT NULL AND coinbase IN (0, 1))  -- whole creation
    )
) WITHOUT ROWID;
CREATE INDEX output_unspent ON output (address_id, asset, id)
    WHERE spent_block_id IS NULL;
CREATE INDEX output_creation ON output (created_block_id);
CREATE INDEX output_spend ON output (spent_block_id);

-- The confirmed line, verified to this block on the last pass.
CREATE TABLE network_checkpoint (
    chain    TEXT NOT NULL,
    network  TEXT NOT NULL,
    block_id TEXT NOT NULL REFERENCES block (id) ON DELETE CASCADE,
    PRIMARY KEY (chain, network)
) WITHOUT ROWID;

-- The wallets crate's row: one address of one network, its key material and
-- application metadata. key_format names the exact bytes in private_key:
-- 'secp256k1-scalar' (a 32-byte scalar) or 'ed25519-seed' (a 32-byte seed);
-- import encodings are normalized before they get here. Scanning never reads
-- the key material.
CREATE TABLE wallet (
    id          TEXT    NOT NULL,
    chain       TEXT    NOT NULL,
    network     TEXT    NOT NULL,
    address     TEXT    NOT NULL,
    key_format  TEXT,                    -- NULL exactly when private_key is.
    private_key BLOB,                    -- NULL: watch-only.
    metadata    TEXT    NOT NULL DEFAULT '{}',
    archived_at INTEGER,                 -- The application's flag; keys kept.
    PRIMARY KEY (id),
    UNIQUE (chain, network, address),
    CHECK (key_format IN ('secp256k1-scalar', 'ed25519-seed')),
    CHECK ((key_format IS NULL) = (private_key IS NULL))
) WITHOUT ROWID;

-- One wallet's address observed in one asset, from its inclusive birthday
-- position: everything indexing reads, so indexing code never touches
-- `wallet`. The FK is the database's integrity rule: the wallets side files
-- the wallet row first. block_id is the progress cursor (NULL: not yet
-- inspected). The balance is the ledger's (see above). Unwatch deletes the
-- row and the watch's facts (see above). Archiving a wallet is the application's: it
-- unwatches every asset first.
CREATE TABLE wallet_index (
    wallet_id TEXT    NOT NULL REFERENCES wallet (id),
    chain     TEXT    NOT NULL,
    network   TEXT    NOT NULL,
    address   TEXT    NOT NULL,
    asset     TEXT    NOT NULL,
    birthday  INTEGER NOT NULL,          -- Inclusive start position.
    block_id  TEXT    REFERENCES block (id) ON DELETE SET NULL,
    PRIMARY KEY (wallet_id, asset),
    UNIQUE (chain, network, address, asset)
) WITHOUT ROWID;
CREATE INDEX wallet_index_block ON wallet_index (block_id);

-- History pages: memberships joined to their transactions and blocks.
CREATE VIEW history_entry AS
    SELECT a.address_id, t.id, t.transaction_id, t.transaction_order,
           t.execution_status, t.failure_reason,
           b.chain, b.network, b.id AS block_id, b.position AS block_position,
           b.height AS block_height, b.hash AS block_hash,
           b.timestamp AS block_timestamp, b.parents AS block_parents
    FROM transaction_address a
    JOIN "transaction" t ON t.id = a.transaction_id
    JOIN block b ON b.id = t.block_id;
