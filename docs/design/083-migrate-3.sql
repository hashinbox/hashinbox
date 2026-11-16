-- Design 083, iterations 3 and 5: brings a database made by the previous
-- `apps/server/init.sql` (iterations 1 and 2 applied) to the new one,
-- keeping every row. Run once, server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/083-migrate-3.sql
--
-- `leg` gains kind 6 `adjustment` (id
-- `{block}.adjustment.{wallet_index_id}.{leg_index}`) and `archived_at`
-- (a coin a jump wrote off), `wallet_index` state 3 `reconciling`: new
-- CHECKs, which SQLite cannot alter. Both tables are rebuilt (create
-- new, copy, drop, rename) with their indexes; the views naming them
-- (`transaction_with_legs`, `wallet_index_with_balance`, which now reads
-- NULL balances while reconciling) are dropped first and created again.
-- Foreign keys are off while the tables are swapped, and checked before
-- the commit: `foreign_key_check` must print nothing.

PRAGMA foreign_keys = OFF;

BEGIN;

DROP VIEW transaction_with_legs;
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
    CHECK (state IN (0, 1, 2, 3))
);
INSERT INTO wallet_index_new (id, wallet_id, asset, requested_from, birthday, indexed_at, state, reason)
SELECT id, wallet_id, asset, requested_from, birthday, indexed_at, state, reason
FROM wallet_index;
DROP TABLE wallet_index;
ALTER TABLE wallet_index_new RENAME TO wallet_index;
CREATE INDEX wallet_index_wallet ON wallet_index (wallet_id);
CREATE INDEX wallet_index_progress ON wallet_index (indexed_at);

CREATE TABLE leg_new (
    id            TEXT    NOT NULL PRIMARY KEY,
    block         TEXT    REFERENCES block (id), -- NULL: of a pending transaction.
    "transaction" TEXT    REFERENCES "transaction" (id), -- NULL: a reward or an adjustment.
    leg_index     INTEGER NOT NULL, -- In its transaction, or the block's rewards; an adjustment's 0, 1 or 2.
    kind          INTEGER NOT NULL, -- 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward, 6 adjustment.
    address       TEXT    NOT NULL,
    asset         TEXT    NOT NULL,
    amount        TEXT    NOT NULL, -- Exact atomic decimal.
    coin_txid     TEXT,             -- The UTXO created (received) or consumed (spent).
    coin_index    INTEGER,
    wallet_index_id TEXT NOT NULL REFERENCES wallet_index (id) ON DELETE CASCADE,
    position      INTEGER,          -- The block's; NULL while pending.
    seq           INTEGER,          -- Per index, never reused; NULL while pending.
    total         TEXT,             -- Exact decimal, signed; NULL while pending.
    total_received TEXT,            -- Exact decimal; NULL while pending.
    total_sent    TEXT,             -- Exact decimal; NULL while pending.
    archived_at   INTEGER,          -- A received coin: the jump line that wrote it off.
    CHECK (block IS NOT NULL OR "transaction" IS NOT NULL),
    CHECK (kind BETWEEN 0 AND 6),
    CHECK (("transaction" IS NULL) = (kind IN (5, 6))),
    CHECK (id = CASE kind
        WHEN 6 THEN block || '.adjustment.' || wallet_index_id
        ELSE COALESCE("transaction", block || '.reward')
    END || '.' || leg_index),
    CHECK ((coin_txid IS NULL) = (coin_index IS NULL)),
    CHECK (coin_txid IS NULL OR kind IN (0, 1)),
    CHECK (archived_at IS NULL OR (kind = 0 AND coin_txid IS NOT NULL)),
    CHECK ((block IS NULL) = (total IS NULL)
        AND (block IS NULL) = (total_received IS NULL)
        AND (block IS NULL) = (total_sent IS NULL)),
    CHECK ((block IS NULL) = (position IS NULL)
        AND (block IS NULL) = (seq IS NULL))
);
INSERT INTO leg_new (
    id, block, "transaction", leg_index, kind, address, asset, amount, coin_txid, coin_index,
    wallet_index_id, position, seq, total, total_received, total_sent
)
SELECT
    id, block, "transaction", leg_index, kind, address, asset, amount, coin_txid, coin_index,
    wallet_index_id, position, seq, total, total_received, total_sent
FROM leg;
DROP TABLE leg;
ALTER TABLE leg_new RENAME TO leg;
CREATE INDEX leg_address ON leg (address, asset);
CREATE INDEX leg_wallet_index ON leg (wallet_index_id, position, seq);
CREATE UNIQUE INDEX leg_seq ON leg (wallet_index_id, seq);
CREATE INDEX leg_transaction ON leg ("transaction");
CREATE INDEX leg_block ON leg (block);
CREATE INDEX leg_coin ON leg (coin_txid, coin_index) WHERE coin_txid IS NOT NULL;

CREATE VIEW transaction_with_legs AS
SELECT
    t.*,
    b.position AS position,
    COALESCE((
        SELECT json_group_array(
            json_object(
                'id', l.id,
                'block', l.block,
                'transaction', l."transaction",
                'leg_index', l.leg_index,
                'kind', l.kind,
                'address', l.address,
                'asset', l.asset,
                'amount', l.amount,
                'coin_txid', l.coin_txid,
                'coin_index', l.coin_index,
                'wallet_index_id', l.wallet_index_id,
                'position', l.position,
                'seq', l.seq,
                'total', l.total,
                'total_received', l.total_received,
                'total_sent', l.total_sent,
                'archived_at', l.archived_at
            )
            ORDER BY l.leg_index
        )
        FROM leg l
        WHERE l."transaction" = t.id
    ), '[]') AS legs
FROM "transaction" t
LEFT JOIN block b ON b.id = t.block;

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
    IIF(i.confirmed IS NULL OR wi.state = 3, NULL, COALESCE(c.total, '0')) AS balance_confirmed,
    IIF(i.confirmed IS NULL OR wi.state = 3, NULL, COALESCE(c.total_received, '0')) AS balance_confirmed_received,
    IIF(i.confirmed IS NULL OR wi.state = 3, NULL, COALESCE(c.total_sent, '0')) AS balance_confirmed_sent,
    IIF(i.confirmed IS NULL OR wi.state = 3, NULL, (
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
    IIF(wi.state = 3, NULL, (
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
