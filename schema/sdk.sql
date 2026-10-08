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
-- Ownership: `wallet` belongs to the wallets crate — identity, keys,
-- application metadata, birthday. `wallet_index` and everything above it
-- belongs to indexing — the observation of a wallet's address per asset: a
-- progress cursor into `block`, the folded balance. Scanning never reads key
-- material; the join below reads identity and birthday only. Unwatch deletes
-- the cursor row (inspection stops, indexed facts stay); a re-watch resumes
-- from the wallet's birthday.
--
-- Finality is not a table. Each chain states its confirmed line at indexer
-- construction — Bitcoin a depth constant, Ethereum and Solana ask their
-- node — and network_checkpoint records where that line has been verified to.
--
-- Reorg recovery is explicit and offline for the affected network: pause it,
-- then delete the orphan blocks — CASCADE removes their transactions,
-- memberships and ledger rows. Clear BOTH spend references on retained
-- outputs whose spend was orphaned (a boolean could not say when to undo;
-- a retained creation spent in an orphan block is unspent again). Restore the
-- affected cursors to the last retained inspected block, or delete the row
-- when none remains — never advance a lagging wallet to the common ancestor;
-- its own inspection must prove that. Identities, birthdays, keys, archive
-- state and application metadata stay. Replay replacement blocks from the
-- restored cursors; replay is idempotent — matching facts are no-ops,
-- conflicting facts fail. Readers see the repair atomically
-- (BEGIN IMMEDIATE … COMMIT).

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
    block_id          TEXT    NOT NULL REFERENCES block (id),
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

-- Individual movements, not netted totals. Transactionless credits use a NULL
-- transaction. direction 0 credit, 1 debit; kind 0..4 movement, 5 fee,
-- 6 withdrawal. The id deterministically includes block, transaction (or
-- credit namespace), movement, address, asset and direction, and
-- deduplicates transactionless retries.
CREATE TABLE ledger (
    id             TEXT    NOT NULL,
    block_id       TEXT    NOT NULL REFERENCES block (id),
    transaction_id TEXT    REFERENCES "transaction" (id), -- NULL: a credit.
    address_id     TEXT    NOT NULL, -- No wallet FK.
    asset          TEXT    NOT NULL,
    movement_id    TEXT    NOT NULL,
    movement_order INTEGER NOT NULL,
    direction      INTEGER NOT NULL,
    kind           INTEGER NOT NULL,
    amount         TEXT    NOT NULL, -- Exact nonnegative atomic integer.
    counterparty   TEXT,
    PRIMARY KEY (id),
    CHECK (direction IN (0, 1)),
    CHECK (kind BETWEEN 0 AND 6),
    CHECK (movement_order >= 0)
) WITHOUT ROWID;
CREATE INDEX ledger_address_asset
    ON ledger (address_id, asset, block_id, movement_order, id);
CREATE INDEX ledger_transaction ON ledger (transaction_id, movement_order, id);

-- Current canonical output state. Spend references make destructive rollback
-- reversible to a retained block. A spend may arrive before its creation: the
-- tombstone holds identity, address and spend references, and a later
-- creation fills the creation fields without clearing the spend. NULL
-- spent_block_id means unspent.
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
    created_block_id     TEXT    REFERENCES block (id),
    coinbase             INTEGER,
    spent_block_id       TEXT    REFERENCES block (id),
    spent_transaction_id TEXT    REFERENCES "transaction" (id),
    PRIMARY KEY (id),
    UNIQUE (chain, network, transaction_id, output_index),
    CHECK (output_index >= 0),
    CHECK ((spent_block_id IS NULL) = (spent_transaction_id IS NULL)),
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
    block_id TEXT NOT NULL REFERENCES block (id),
    PRIMARY KEY (chain, network)
) WITHOUT ROWID;

-- The wallets crate's row: identity, key material, application metadata and
-- the birthday every observation of this address resumes from. Scanning
-- never reads the key material.
CREATE TABLE wallet (
    id          TEXT    NOT NULL,
    chain       TEXT    NOT NULL,
    network     TEXT    NOT NULL,
    address     TEXT    NOT NULL,
    key_format  TEXT    NOT NULL,
    private_key BLOB,                    -- NULL: a watch-only placeholder.
    metadata    TEXT    NOT NULL DEFAULT '{}',
    birthday    INTEGER NOT NULL,        -- Inclusive start position.
    archived_at INTEGER,                 -- The application's flag; keys kept.
    PRIMARY KEY (id),
    UNIQUE (chain, network, address)
) WITHOUT ROWID;

-- The lightweight join: a wallet's address observed per asset. block_id is
-- the progress cursor (NULL: not yet inspected); balance is the signed
-- absolute total, folded in the same transaction as the movements — never
-- repaired silently. Unwatch deletes the row; indexed facts stay.
CREATE TABLE wallet_index (
    wallet_id TEXT NOT NULL REFERENCES wallet (id),
    asset     TEXT NOT NULL,
    block_id  TEXT REFERENCES block (id),
    balance   TEXT NOT NULL DEFAULT '0', -- Signed exact atomic total.
    PRIMARY KEY (wallet_id, asset)
) WITHOUT ROWID;
CREATE INDEX wallet_index_block ON wallet_index (block_id);

-- History pages: memberships joined to their transactions and blocks.
CREATE VIEW history_entry AS
    SELECT a.address_id, t.id, t.transaction_id, t.transaction_order,
           t.execution_status, t.failure_reason,
           b.chain, b.network, b.id AS block_id, b.position AS block_position,
           b.height AS block_height, b.hash AS block_hash,
           b.timestamp AS block_timestamp
    FROM transaction_address a
    JOIN "transaction" t ON t.id = a.transaction_id
    JOIN block b ON b.id = t.block_id;
