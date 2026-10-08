-- Proposed schema; not loaded by the current repository. Apply to an empty file.
-- IDs use canonical chain/network/native values with unambiguous escaping of
-- components; bare colon concatenation is insufficient when values contain ':'.
-- Constructors validate IDs, exact atomic amounts, JSON shapes and network
-- agreement across references. Enable foreign_keys on every SQLite connection.
-- Scanning never reads private_key. Addresses need not have a registered wallet.
-- Persist confirmed facts only; pending facts remain in memory.
--
-- Reorg recovery is explicit and offline for the affected network:
-- 1. Pause its writer and admissions, drain active work, discard pending facts,
--    and verify a retained common ancestor against the chain. Determine the
--    exact orphan block IDs from ancestry, not height alone (slots can be sparse
--    and sharded chains need branch-aware traversal). Keep other networks intact.
-- 2. BEGIN IMMEDIATE and verify the expected checkpoint has not changed.
-- 3. Delete outputs created in orphan blocks and creation-less tombstones whose
--    spent_block_id is orphaned. For retained creations spent in orphan blocks,
--    clear BOTH spent_block_id and spent_transaction_id. Never clear a retained
--    spend merely because its creation was discovered by a later scanning pass.
-- 4. Delete orphan ledger entries, then orphan transactions (their address
--    memberships cascade). Reset affected wallet progress to its last verified
--    retained inspection point, or NULL when none remains. Do not advance a
--    lagging wallet to the common ancestor: its own inspection must prove that.
--    Keep identities, birthdays, keys, archive state and application metadata.
-- 5. Move/delete the network checkpoint and remove references to orphan blocks
--    before deleting those blocks. COMMIT all repairs together. Before commit,
--    readers see old state; after commit, they see only the retained history.
-- 6. Reset the live indexer and replay replacement blocks from restored progress.
--    Replay is idempotent: matching facts are no-ops; conflicting facts fail.
--    If commit outcome is uncertain, reread checkpoint/state before retrying.
--
-- Example: output created at 100, spent at 120, recovery retains through 110.
-- Keep its creation, clear its spend, and it is unspent again. A creation at
-- 120 is deleted. A spend-first tombstone at 120 is deleted too. Creation replay
-- fills missing creation fields without overwriting a retained spend.
-- Balances fold retained ledger entries using checked exact arithmetic, not
-- SQLite SUM over text. Reads expose incomplete wallet inspection after repair.
-- No discarded-fork archive or automatic recovery implementation is provided.

CREATE TABLE wallet (
    id              TEXT    NOT NULL, -- <chain:network:address>
    chain           TEXT    NOT NULL,
    network         TEXT    NOT NULL,
    address         TEXT    NOT NULL,
    key_format      TEXT    NOT NULL,
    private_key     BLOB,
    config          JSON    NOT NULL DEFAULT '{}',
    metadata        JSON    NOT NULL DEFAULT '{}',
    birthday        INTEGER,
    progress        JSON, -- Last inspected block reference; NULL means no claim.
    archived_at     INTEGER,
    PRIMARY KEY (id),
    UNIQUE (chain, network, address)
) WITHOUT ROWID;

CREATE TABLE block (
    id              TEXT    NOT NULL, -- <chain:network:hash>
    chain           TEXT    NOT NULL,
    network         TEXT    NOT NULL,
    hash            BLOB    NOT NULL,
    position        INTEGER NOT NULL,
    height          INTEGER NOT NULL,
    timestamp       INTEGER, -- Unix seconds; NULL when unavailable.
    parents         JSON    NOT NULL DEFAULT '[]', -- Native references; genesis [].
    config          JSON    NOT NULL DEFAULT '{}', -- E.g. workchain/shard identity.
    PRIMARY KEY (id),
    UNIQUE (chain, network, hash)
) WITHOUT ROWID;
CREATE INDEX block_position ON block (chain, network, position);

-- One current canonical inclusion per native transaction identity.
CREATE TABLE "transaction" (
    id                  TEXT    NOT NULL, -- <chain:network:transaction_id>
    block_id            TEXT    NOT NULL,
    transaction_id      TEXT    NOT NULL,
    transaction_order   INTEGER NOT NULL,
    execution_status    INTEGER NOT NULL,
    failure_reason      TEXT,
    metadata            JSON    NOT NULL DEFAULT '{}',
    PRIMARY KEY (id),
    FOREIGN KEY (block_id) REFERENCES block (id),
    CHECK (execution_status IN (0, 1)),
    CHECK (transaction_order >= 0)
) WITHOUT ROWID;
CREATE INDEX transaction_block ON "transaction" (block_id, transaction_order, id);

CREATE TABLE transaction_address (
    transaction_id      TEXT NOT NULL, -- Internal transaction.id.
    address_id          TEXT NOT NULL, -- <chain:network:address>; no wallet FK.
    PRIMARY KEY (transaction_id, address_id),
    FOREIGN KEY (transaction_id) REFERENCES "transaction" (id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX transaction_address_history
    ON transaction_address (address_id, transaction_id);

-- Individual movements, not netted totals. Transactionless credits use NULL.
-- Preserve current tags: direction 0 credit, 1 debit; kind 0..4 movements,
-- 5 fee, 6 withdrawal. Fees are recorded here, without transaction fee summaries.
-- id deterministically includes block, transaction/credit namespace, movement,
-- address, asset and direction; it must also deduplicate transactionless retries.
CREATE TABLE ledger (
    id                  TEXT    NOT NULL,
    block_id            TEXT    NOT NULL,
    transaction_id      TEXT,
    address_id          TEXT    NOT NULL, -- No wallet FK.
    asset               TEXT    NOT NULL,
    movement_id         TEXT    NOT NULL,
    movement_order      INTEGER NOT NULL,
    direction           INTEGER NOT NULL,
    kind                INTEGER NOT NULL,
    amount              TEXT    NOT NULL, -- Exact nonnegative atomic integer.
    counterparty        TEXT,
    evidence            JSON    NOT NULL DEFAULT '{}',
    PRIMARY KEY (id),
    FOREIGN KEY (block_id) REFERENCES block (id),
    FOREIGN KEY (transaction_id) REFERENCES "transaction" (id),
    CHECK (direction IN (0, 1)),
    CHECK (kind BETWEEN 0 AND 6),
    CHECK (movement_order >= 0)
) WITHOUT ROWID;
CREATE INDEX ledger_address_asset
    ON ledger (address_id, asset, block_id, movement_order, id);
CREATE INDEX ledger_transaction ON ledger (transaction_id, movement_order, id);

-- Current canonical output state. Spend references make destructive rollback
-- reversible to a retained block; a boolean alone cannot say when to undo it.
-- A spend may arrive before creation. Its tombstone has only identity/address
-- and spend references. Later creation fills the whole creation without clearing
-- the spend. NULL spent_block_id means unspent, not an unknown tombstone.
CREATE TABLE output (
    id                      TEXT    NOT NULL, -- <chain:network:creating_tx:index>
    chain                   TEXT    NOT NULL,
    network                 TEXT    NOT NULL,
    transaction_id          TEXT    NOT NULL, -- Native creator ID; no required FK.
    output_index            INTEGER NOT NULL,
    address_id              TEXT, -- No wallet FK; some scripts have no address.
    asset                   TEXT,
    amount                  TEXT,
    evidence                BLOB,
    created_block_id        TEXT,
    coinbase                INTEGER,
    spent_block_id          TEXT,
    spent_transaction_id    TEXT, -- Internal transaction.id.
    PRIMARY KEY (id),
    UNIQUE (chain, network, transaction_id, output_index),
    FOREIGN KEY (created_block_id) REFERENCES block (id),
    FOREIGN KEY (spent_block_id) REFERENCES block (id),
    FOREIGN KEY (spent_transaction_id) REFERENCES "transaction" (id),
    CHECK (output_index >= 0),
    CHECK ((spent_block_id IS NULL) = (spent_transaction_id IS NULL)),
    CHECK (
        (asset IS NULL AND amount IS NULL AND evidence IS NULL
            AND created_block_id IS NULL AND coinbase IS NULL
            AND spent_block_id IS NOT NULL)
        OR
        (asset IS NOT NULL AND amount IS NOT NULL AND evidence IS NOT NULL
            AND created_block_id IS NOT NULL AND coinbase IS NOT NULL
            AND coinbase IN (0, 1))
    )
) WITHOUT ROWID;
CREATE INDEX output_unspent ON output (address_id, asset, id)
    WHERE spent_block_id IS NULL;
CREATE INDEX output_creation ON output (created_block_id);
CREATE INDEX output_spend ON output (spent_block_id);

CREATE TABLE network_checkpoint (
    chain           TEXT NOT NULL,
    network         TEXT NOT NULL,
    block_id        TEXT NOT NULL,
    PRIMARY KEY (chain, network),
    FOREIGN KEY (block_id) REFERENCES block (id)
) WITHOUT ROWID;

CREATE VIEW history_entry AS
    SELECT a.address_id, t.id, t.transaction_id, t.transaction_order,
           t.execution_status, t.failure_reason, b.chain, b.network,
           b.id AS block_id, b.position AS block_position,
           b.height AS block_height, b.hash AS block_hash,
           b.timestamp AS block_timestamp
    FROM transaction_address a
    JOIN "transaction" t ON t.id = a.transaction_id
    JOIN block b ON b.id = t.block_id;
