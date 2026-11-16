-- Design 083, the alt rewrite: brings a database made by the previous
-- `apps/server/init.sql` (`083-migrate-3.sql` applied) to the new one,
-- keeping every row. Run once, server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/083-migrate-4.sql
--
-- `wallet_index` loses state 3 `reconciling`: a reconciling index becomes
-- indexing (0), and a chain that reads balances settles it again by itself
-- when its next block is at or below the chain's final block. Its CHECK
-- goes back to `state IN (0, 1, 2)`, which SQLite cannot alter: the table
-- is rebuilt (create new, copy, drop, rename) with its indexes; the view
-- naming it (`wallet_index_with_balance`, which now reads NULL balances
-- before an index's first block instead of while reconciling) is dropped
-- first and created again. Ethereum and Solana lose `max_depth` (NULL):
-- their line is the chain's final block. Foreign keys are off while the
-- table is swapped, and checked before the commit: `foreign_key_check`
-- must print nothing.

PRAGMA foreign_keys = OFF;

BEGIN;

DROP VIEW wallet_index_with_balance;

CREATE TABLE wallet_index_new (
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
INSERT INTO wallet_index_new (id, wallet_id, asset, requested_from, birthday, indexed_at, state, reason)
SELECT id, wallet_id, asset, requested_from, birthday, indexed_at, IIF(state = 3, 0, state), reason
FROM wallet_index;
DROP TABLE wallet_index;
ALTER TABLE wallet_index_new RENAME TO wallet_index;
CREATE INDEX wallet_index_wallet ON wallet_index (wallet_id);
CREATE INDEX wallet_index_progress ON wallet_index (indexed_at);

UPDATE network SET max_depth = NULL WHERE chain IN ('ethereum', 'solana');

CREATE VIEW wallet_index_with_balance AS
SELECT
    wi.*,
    w.chain,
    w.network,
    w.address,
    w.tags,
    w.secret IS NOT NULL AS keyed,
    -- A reorged block indexes nothing.
    IIF(b.reorged_at IS NULL, b.position, NULL) AS indexed_through,
    MAX(i.tip - IIF(b.reorged_at IS NULL, b.position, NULL), 0) AS behind_tip,
    i.confirmed AS finality_at,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total, '0')) AS balance_confirmed,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total_received, '0')) AS balance_confirmed_received,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total_sent, '0')) AS balance_confirmed_sent,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, (
        SELECT SUM_DECIMAL(v)
        FROM (
            SELECT COALESCE(c.total, '0') AS v
            UNION ALL
            -- Same as `block IS NULL` (CHECK); lets `leg_wallet_index` seek
            -- straight to the pending legs.
            SELECT '-' || p.amount
            FROM leg p
            WHERE p.wallet_index_id = wi.id
              AND p.position IS NULL
              AND p.kind IN (1, 2, 4)
            UNION ALL
            SELECT '-' || m.amount
            FROM leg m
            JOIN block mb ON mb.id = m.block
            WHERE m.wallet_index_id = wi.id
              AND m.position > i.confirmed
              AND mb.reorged_at IS NULL
              AND m.kind IN (1, 2, 4)
        )
    )) AS balance_confirmed_spendable,
    IIF(wi.indexed_at IS NULL, NULL, (
        SELECT SUM_DECIMAL(v)
        FROM (
            SELECT COALESCE(n.total, '0') AS v
            UNION ALL
            SELECT IIF(p.kind IN (1, 2, 4), '-' || p.amount, p.amount)
            FROM leg p
            WHERE p.wallet_index_id = wi.id
              AND p.position IS NULL
        )
    )) AS balance_pending
FROM wallet_index wi
JOIN wallet w ON w.id = wi.wallet_id
LEFT JOIN block b ON b.id = wi.indexed_at
LEFT JOIN network_index i ON i.chain = w.chain
    AND i.network = w.network
LEFT JOIN leg c ON c.id = (
    SELECT l.id
    FROM leg l
    JOIN block lb ON lb.id = l.block
    WHERE l.wallet_index_id = wi.id
      AND l.position <= i.confirmed
      AND lb.reorged_at IS NULL
    ORDER BY l.position DESC, l.seq DESC
    LIMIT 1
)
LEFT JOIN leg n ON n.id = (
    SELECT l.id
    FROM leg l
    JOIN block lb ON lb.id = l.block
    WHERE l.wallet_index_id = wi.id
      AND lb.reorged_at IS NULL
    ORDER BY l.position DESC, l.seq DESC
    LIMIT 1
);

PRAGMA foreign_key_check;

COMMIT;

PRAGMA foreign_keys = ON;
