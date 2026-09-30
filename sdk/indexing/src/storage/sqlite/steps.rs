//! The schema, as ordered steps.
//!
//! Separate from `schema.rs` because that file is the mechanism — what the file
//! is, which application owns it, how many steps it is behind — and this is the
//! content it applies. A step may alter what an earlier step wrote
//! (`docs/SYSTEM_REQUIREMENTS.md`), so the array is append-only and the text of
//! an applied step is never edited.

/// A file's `PRAGMA user_version` is the number of steps applied to it, so step
/// `n` is applied exactly once, in one transaction with the bump that records
/// it.
pub(super) const MIGRATIONS: [&str; 4] = [REPOSITORY, WATCHED_ADDRESS, BALANCES, OPENING_EVIDENCE];

/// Initial repository schema. Later changes append transactional migration steps;
/// an existing file retains its prior rows while advancing its version.
const REPOSITORY: &str = "
    CREATE TABLE watch (
        chain                   TEXT    NOT NULL,
        network                 TEXT    NOT NULL,
        asset                   TEXT    NOT NULL,
        address                 TEXT    NOT NULL,
        id                      TEXT    NOT NULL,
        birthday                INTEGER NOT NULL,
        indexed_position        INTEGER,
        indexed_height          INTEGER,
        indexed_hash            BLOB,
        indexed_parent_position INTEGER,
        indexed_parent_hash     BLOB,
        indexed_timestamp       INTEGER,
        PRIMARY KEY (chain, network, asset, address)
    ) WITHOUT ROWID;
    CREATE UNIQUE INDEX watch_id ON watch (id);

    CREATE TABLE network_checkpoint (
        chain           TEXT    NOT NULL,
        network         TEXT    NOT NULL,
        position        INTEGER NOT NULL,
        height          INTEGER NOT NULL,
        hash            BLOB    NOT NULL,
        parent_position INTEGER,
        parent_hash     BLOB,
        timestamp       INTEGER,
        PRIMARY KEY (chain, network)
    ) WITHOUT ROWID;

    CREATE TABLE transaction_fact (
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
        PRIMARY KEY (chain, network, transaction_id)
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
            REFERENCES transaction_fact (chain, network, transaction_id)
            ON DELETE CASCADE
    ) WITHOUT ROWID;

    CREATE TABLE balance_movement (
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
                     transaction_id, movement_id, direction)
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
        CHECK (asset IS NOT NULL OR spent <> 0)
    ) WITHOUT ROWID;
    CREATE INDEX output_unspent
        ON output (chain, network, address, transaction_id, output_index)
        WHERE spent = 0;
";

/// Every observation of one address, as a lookup.
///
/// `watch` is keyed `(chain, network, asset, address)`, which serves the read
/// that names an asset and no other. A balance, a history page and a spendable
/// set each ask how far *the address* has been inspected — whatever asset each
/// observation of it is in — inside the snapshot they answer from, and without
/// this that question walks the network's whole watch list on every read.
///
/// A step of its own rather than an edit to the first: the array is
/// append-only, and a file the first step already created is brought forward
/// by this one.
const WATCHED_ADDRESS: &str = "
    CREATE INDEX watch_address ON watch (chain, network, address);
";

/// Watched holdings keep their signed net; Rust seeds exact values in this step.
const BALANCES: &str = "ALTER TABLE watch ADD COLUMN balance TEXT NOT NULL DEFAULT '0';";

/// A verified snapshot belongs to its watched holding and is removed with it.
const OPENING_EVIDENCE: &str = "
    ALTER TABLE watch ADD COLUMN evidence_position INTEGER;
    ALTER TABLE watch ADD COLUMN evidence_height INTEGER;
    ALTER TABLE watch ADD COLUMN evidence_hash BLOB;
    ALTER TABLE watch ADD COLUMN evidence_parent_position INTEGER;
    ALTER TABLE watch ADD COLUMN evidence_parent_hash BLOB;
    ALTER TABLE watch ADD COLUMN evidence_timestamp INTEGER;
    ALTER TABLE watch ADD COLUMN evidence_amount TEXT;
";
