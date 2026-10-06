-- The indexing repository's tables, in the one SQLite file it shares with
-- its application. Applied once, to an empty file, which is then stamped with
-- this text's fingerprint (`schema.rs`); there are no migrations: a schema
-- change edits this file and needs a fresh database.
--
-- `wallet` is every wallet the store holds, observed or archived. An observed
-- row (`archived_at` NULL) has a birthday and a `balance`, its holding's signed
-- net kept exact inside every commit, and the progress commits record. An
-- archived row is scanned by nothing and claims nothing: no progress and no
-- balance, so its reads fold the ledger. It keeps its birthday, or has none when
-- nothing ever observed it. `label` and `secret` belong to the application
-- (`WalletRegistry`); scanning never reads them. `wallet_address` indexes an
-- address's rows across assets.
--
-- Every table has one model in `models/`, which owns the statements that read
-- and write it; the one view, `history_entry`, is a join and has its own.

CREATE TABLE wallet (
    chain                   TEXT    NOT NULL,
    network                 TEXT    NOT NULL,
    asset                   TEXT    NOT NULL,
    address                 TEXT    NOT NULL,
    id                      TEXT    NOT NULL,
    birthday                INTEGER,
    block_position        INTEGER,
    block_height          INTEGER,
    block_hash            BLOB,
    block_parent_position INTEGER,
    block_parent_hash     BLOB,
    block_timestamp       INTEGER,
    balance                 TEXT,
    label                   TEXT,
    secret                  BLOB,
    archived_at             INTEGER,
    PRIMARY KEY (chain, network, asset, address),
    -- Observed: scanned from a birthday, with a total kept.
    CHECK (archived_at IS NOT NULL OR (birthday IS NOT NULL AND balance IS NOT NULL)),
    -- Archived: nothing claimed, nothing kept.
    CHECK (archived_at IS NULL OR (block_position IS NULL AND balance IS NULL)),
    -- Progress is one block reference, written whole or not at all.
    CHECK ((block_position IS NULL) = (block_height IS NULL)
       AND (block_position IS NULL) = (block_hash IS NULL)),
    CHECK ((block_parent_position IS NULL) = (block_parent_hash IS NULL)),
    CHECK (block_position IS NOT NULL
        OR (block_parent_position IS NULL AND block_timestamp IS NULL))
) WITHOUT ROWID;
CREATE UNIQUE INDEX wallet_id ON wallet (id);
CREATE INDEX wallet_address ON wallet (chain, network, address);

CREATE TABLE network_checkpoint (
    chain           TEXT    NOT NULL,
    network         TEXT    NOT NULL,
    block_position  INTEGER NOT NULL,
    block_height    INTEGER NOT NULL,
    block_hash      BLOB    NOT NULL,
    block_parent_position INTEGER,
    block_parent_hash BLOB,
    block_timestamp INTEGER,
    PRIMARY KEY (chain, network),
    CHECK ((block_parent_position IS NULL) = (block_parent_hash IS NULL))
) WITHOUT ROWID;

CREATE TABLE "transaction" (
    chain                 TEXT    NOT NULL,
    network               TEXT    NOT NULL,
    transaction_id        TEXT    NOT NULL,
    block_position        INTEGER NOT NULL,
    block_height          INTEGER NOT NULL,
    block_hash            BLOB    NOT NULL,
    block_parent_position INTEGER,
    block_parent_hash     BLOB,
    block_timestamp       INTEGER,
    transaction_order     INTEGER NOT NULL,
    execution_status      INTEGER NOT NULL,
    failure_reason        TEXT,
    fee_asset             TEXT,
    fee_amount            TEXT,
    fee_payer             TEXT,
    PRIMARY KEY (chain, network, transaction_id),
    CHECK (execution_status IN (0, 1)),
    CHECK ((fee_asset IS NULL) = (fee_amount IS NULL)),
    CHECK ((block_parent_position IS NULL) = (block_parent_hash IS NULL))
) WITHOUT ROWID;

CREATE TABLE transaction_address (
    chain             TEXT    NOT NULL,
    network           TEXT    NOT NULL,
    address           TEXT    NOT NULL,
    block_height      INTEGER NOT NULL,
    transaction_order INTEGER NOT NULL,
    transaction_id    TEXT    NOT NULL,
    PRIMARY KEY (chain, network, address, block_height, transaction_order,
                 transaction_id),
    FOREIGN KEY (chain, network, transaction_id)
        REFERENCES "transaction" (chain, network, transaction_id)
        ON DELETE CASCADE
) WITHOUT ROWID;

CREATE TABLE ledger (
    chain          TEXT    NOT NULL,
    network        TEXT    NOT NULL,
    address        TEXT    NOT NULL,
    asset          TEXT    NOT NULL,
    block_height   INTEGER NOT NULL,
    transaction_id TEXT    NOT NULL,
    movement_id    TEXT    NOT NULL,
    direction      INTEGER NOT NULL,
    block_position INTEGER NOT NULL,
    movement_order INTEGER NOT NULL,
    kind           INTEGER NOT NULL,
    amount         TEXT    NOT NULL,
    counterparty   TEXT,
    PRIMARY KEY (chain, network, address, asset, block_height,
                 transaction_id, movement_id, direction),
    CHECK (direction IN (0, 1)),
    CHECK (kind BETWEEN 0 AND 6)
) WITHOUT ROWID;

CREATE TABLE output (
    chain          TEXT    NOT NULL,
    network        TEXT    NOT NULL,
    address        TEXT    NOT NULL,
    transaction_id TEXT    NOT NULL,
    output_index   INTEGER NOT NULL,
    asset          TEXT,
    amount         TEXT,
    evidence       BLOB,
    created_height INTEGER,
    coinbase       INTEGER,
    spent          INTEGER NOT NULL,
    PRIMARY KEY (chain, network, address, transaction_id, output_index),
    CHECK (spent IN (0, 1)),
    CHECK (asset IS NOT NULL OR spent <> 0),
    -- A creation is written whole, or not yet (a spend that arrived first).
    CHECK ((asset IS NULL AND amount IS NULL AND evidence IS NULL
            AND created_height IS NULL AND coinbase IS NULL)
        OR (asset IS NOT NULL AND amount IS NOT NULL AND evidence IS NOT NULL
            AND created_height IS NOT NULL AND coinbase IN (0, 1)))
) WITHOUT ROWID;
CREATE INDEX output_unspent
    ON output (chain, network, address, transaction_id, output_index)
    WHERE spent = 0;

-- One address's memberships beside the transactions they name. The place
-- columns are the membership's, so paging walks `transaction_address`'s key.
CREATE VIEW history_entry AS
    SELECT a.chain, a.network, a.address, a.block_height, a.transaction_order,
           a.transaction_id, t.block_position, t.block_hash,
           t.block_parent_position, t.block_parent_hash, t.block_timestamp,
           t.execution_status, t.failure_reason, t.fee_asset, t.fee_amount,
           t.fee_payer
    FROM transaction_address a
    JOIN "transaction" t
      ON t.chain = a.chain AND t.network = a.network
     AND t.transaction_id = a.transaction_id;
