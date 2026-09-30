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
pub(super) const MIGRATIONS: [&str; 3] = [REPOSITORY, WATCHED_ADDRESS, BALANCES];

/// The schema this build creates, whole, in one step.
///
/// **There is no shape to bring forward, so none is created.**
/// `docs/SYSTEM_REQUIREMENTS.md` revoked the durable-format exception in
/// writing: there is one storage backend, there is no reader for any earlier
/// format, and no deployment exists, so the exception protected nothing. A step
/// that built the five-table shape `docs/design/030` left behind and then
/// copied it into this one would be migrating a file nothing ever wrote —
/// executable proof of an equivalence between a live shape and a shape this
/// build never produces, which proves nothing about either. The file this
/// build creates carries the number of steps in [`MIGRATIONS`] as its
/// `PRAGMA user_version`, and one whose version it cannot account for is
/// refused rather than interpreted (`schema.rs`).
///
/// Everything below is framed by `(chain, network)` as its leading key columns,
/// which is what makes `purge_network` one `DELETE ... WHERE` per table, and is
/// `WITHOUT ROWID` throughout: every table is addressed by its whole primary key
/// and never by a row id, and those keys are the orders the reads page in, so an
/// ascending keyset page is an index walk rather than a sort.
///
/// What each table is for, and what it cost to shape it this way:
///
/// - `watch` is one row per observation: what to observe and how far it has been
///   inspected. Progress used to be a second table joined on every read, which
///   bought a foreign key, a cascade and one extra read per recorded claim;
///   folded in, `WatchStore::all`'s "one consistent snapshot" is a single
///   statement with nothing to pair. `id` keeps a file-wide unique index rather
///   than a per-network one, because `payment-server`'s identity map is global.
/// - `network_checkpoint` is the network's confirmed line, a table of its own
///   because individual watches may still be catching up to it — which is
///   `WatchProgress::caught_up`.
/// - `transaction_fact` holds a transaction's block reference, status and fee
///   **once**. Storing them per participating address, as the shape this
///   replaces did, cost eleven columns twice for a two-party transfer and twelve
///   times for a Bitcoin transaction naming a dozen addresses; it also made
///   `verify` a comparison per address rather than one row against one truth.
/// - `transaction_address` is who took part, and it is required rather than
///   optional: a failed transaction carries no movement at all
///   (`ObservationDraft::validate_movements`) and a successful one may touch its
///   fee payer nowhere but the fee, so both would vanish from every history if
///   membership were derived from `balance_movement` alone.
/// - `balance_movement` is one balance change, for one address, in one asset,
///   with `direction` separate from `kind`. A transactionless credit is the same
///   row with no transaction to name it.
/// - `output` is one outpoint's whole life. Creation and spending are two halves
///   of it, and **knowing the second before the first is a real state** — a
///   spend above an address's anchor whose creating block is still below the
///   checkpoint. The two-table shape could only express it by having one table
///   answer for the other; here the columns say it, and the `CHECK` forbids the
///   one combination that means nothing.
///
/// Absolute balances are added and seeded by the third transactional step.
/// Signed atomic text preserves deficits while historical credits are outstanding.
///
/// **No persisted attestation without a writer for its evidence.** The
/// column held an *attested* basis — `1` when an independent read of the same
/// subject at the same block agreed with the fold — and nothing in the
/// persistence layer could ever write it. The only thing in this SDK that
/// produces an attestation is `chain_ethereum::BalanceWitness`, which reads a
/// node and reports to its caller whether the two agreed; persisting what it
/// learned means giving it a store to write to, and that is an ownership change
/// (`docs/design/034` steps 9-12) rather than a schema one. The evidence columns
/// `034` section 3.1 paired with it were never created for the same reason:
/// columns nothing can write are dead schema, and they cost nothing to add
/// beside the writer that needs them.
///
/// The other claim a caller can receive — that every change the address ever
/// made is indexed — is not stored either. It is a conjunction of `birthday` and
/// the recorded progress, both columns of this same row and both changed by a
/// legal write, so storing it as well would guarantee the two drifted apart.
///
/// **`transaction_order` is a column, and it is written rather than computed.**
/// `docs/design/034` section 5.6 asked for the transaction's native place in its
/// block, on the grounds that it is recoverable now and not later. It is not
/// recoverable *here*: a store is handed the transactions that touched a
/// selected address, never the block's whole list, so any ordinal this file
/// computed would be a dense rank within whichever selection reached the block
/// first. Two transactions of one block indexed under different selections would
/// both sit at rank zero, and a later, wider watch would insert rows whose ranks
/// fall **below** a cursor a caller is already holding — which `docs/API.md`
/// promises in as many words cannot happen.
///
/// So the number arrives instead: `ObservationDraft::transaction_order` is the
/// position in the *complete* block, filled by each interpreter out of the
/// block-ordered list it holds while it filters (`base::Transactions`), and
/// `transaction_fact` stores it beside the rest of the transaction's facts —
/// where insert-if-absent, verify-if-present compares it like every other
/// column, so one transaction cannot be stored at two places. A page is ordered
/// by `(block_height, transaction_order, transaction_id)`, and the cursor is
/// that triple: all three are functions of the block alone, so the page stays
/// append-only under any future selection and now runs in native execution
/// order. `transaction_address` repeats the ordinal inside its own primary key,
/// for the reason it already repeats `block_height` — the page walks that table
/// alone, and an ordering column outside the key it walks would turn the keyset
/// page into a sort.
///
/// `balance_movement` does **not** repeat it. Its rows are read grouped by the
/// transaction they belong to, whose order among transactions the history page
/// has already decided; what those rows order is the movements *inside* one
/// transaction, which is `movement_order`.
///
/// **Two divergences from `034` section 3, both forced by what the write path
/// can produce.** A row no transaction names carries `transaction_id = ''`
/// rather than `NULL`, because SQLite makes every primary-key column of a
/// `WITHOUT ROWID` table implicitly `NOT NULL` and the identity has to be *in*
/// the key for a re-presented block to collide rather than double-count; the
/// sentinel is unforgeable because `ObservationDraft::validate` refuses an empty
/// transaction identifier. And a spend is recorded as a flag rather than a
/// height, because the height of the block that spent an output is not among the
/// facts `OutputChanges` carries — a `spent_height` column would have had to be
/// invented rather than observed.
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
