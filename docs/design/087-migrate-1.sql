-- Design 087: brings a database made by the previous
-- `apps/server/init.sql` (`085-migrate-5.sql` applied) to the new one,
-- keeping every row but the ones below. Run once, server stopped, after a
-- backup:
--
--   sqlite3 payments.db < docs/design/087-migrate-1.sql
--
-- `transaction_forgotten` goes. Progress pointers become positions: `wallet_index.indexed_at`,
-- `transaction_index.indexed_at` and `transaction_index.found` (now
-- `found_at`) hold the position of the block they named. A transaction
-- index keeps only what it does as its state (0 indexing, 1 paused, 2
-- no_data): a found one is state 0 with `found_at` kept; whether it
-- reverted is its transaction row's own state, as it is; `found_reverted`
-- goes. Expiry is
-- the broadcast job's, by the height of a kept block at or below the
-- confirmed position: no state for it. `network_index` keeps only `tip`
-- and `confirmed`, both NOT NULL: a row without a tip is dropped (the
-- indexer's next step writes it again). `transaction.sent_at` becomes
-- `broadcasted_at`, set exactly on this server's sends (a transaction a
-- transaction index looks for, or a pending one): `sent_at`, else when its
-- broadcast job recorded it (the job's `created_at`), else now; every other
-- row NULL. Adjustments
-- (leg kind 6) lose their block, keep their position, and take the id
-- `{wallet_index_id}.adjustment.{position}.{leg_index}`. A reorg is a hard
-- delete now: first the reorged blocks go with their facts, as a reorg's
-- write does today, by hand (foreign keys are off, nothing cascades): a
-- transaction a transaction index looks for (this server's send) mined in
-- one goes back to pending with its legs, every other one goes with its
-- legs, then the blocks' rewards and adjustments, then the blocks (a kept
-- child's `parent` unset). SQLite cannot drop a column with a foreign key:
-- the tables are rebuilt (create new, copy, drop, rename) with their
-- indexes: `wallet_index`, `transaction_index`, `leg`, then
-- `block` without `reorged_at` and with a plain unique position, and
-- `transaction` with its block's `position`. This server's sends (a
-- transaction a transaction index looks for, pending or mined): a reorg
-- sets them, and their legs, pending again. The views
-- naming them are dropped first and created again last
-- (`transaction_index_with_blocks` is not; `transaction_with_legs` reads
-- `position` from its row). Before the
-- last two rebuilds, the blocks below each network's confirmed position
-- that hold nothing (no leg, no transaction: the settles' empty blocks) are
-- deleted, but each network's newest one, as the prune job does. Foreign
-- keys are off while the tables are swapped, and checked before the
-- commit: `foreign_key_check` must print nothing.

PRAGMA foreign_keys = OFF;

BEGIN;

DROP VIEW transaction_with_legs;
DROP VIEW wallet_index_with_balance;
DROP VIEW transaction_index_with_blocks;

-- The broadcast job's state is the record of a payment that never landed
-- (design 084, Resolution): the audit table goes.
DROP TABLE transaction_forgotten;

-- Only the tip and the confirmed position, written the first time the tip
-- is known: a row without one goes.
CREATE TABLE network_index_new (
    chain      TEXT    NOT NULL,
    network    TEXT    NOT NULL,
    tip        INTEGER NOT NULL,
    confirmed  INTEGER NOT NULL,
    indexed    INTEGER,
    checked_at INTEGER,
    blocks     INTEGER NOT NULL DEFAULT 0,
    reorgs     INTEGER NOT NULL DEFAULT 0,
    failures   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (chain, network),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network)
);
INSERT INTO network_index_new (
    chain, network, tip, confirmed, indexed, checked_at, blocks, reorgs, failures
)
SELECT chain, network, tip, confirmed, indexed, checked_at, blocks, reorgs, failures
FROM network_index
WHERE tip IS NOT NULL
  AND confirmed IS NOT NULL;
DROP TABLE network_index;
ALTER TABLE network_index_new RENAME TO network_index;

-- First the reorged blocks, with their facts; ours back to pending.
CREATE TEMP TABLE reorged AS
SELECT id FROM block WHERE reorged_at IS NOT NULL;
CREATE TEMP TABLE ours AS
SELECT t.id
FROM "transaction" t
WHERE t.block IN (SELECT id FROM reorged)
  AND t.id IN (SELECT id FROM transaction_index);
UPDATE leg
SET block = NULL, position = NULL, seq = NULL,
    total = NULL, total_received = NULL, total_sent = NULL
WHERE "transaction" IN (SELECT id FROM ours);
UPDATE "transaction"
SET block = NULL, "order" = NULL, state = 0, reason = NULL
WHERE id IN (SELECT id FROM ours);
DELETE FROM leg
WHERE "transaction" IN (
    SELECT id FROM "transaction" WHERE block IN (SELECT id FROM reorged)
);
DELETE FROM "transaction" WHERE block IN (SELECT id FROM reorged);
DELETE FROM leg WHERE block IN (SELECT id FROM reorged);
UPDATE block SET parent = NULL WHERE parent IN (SELECT id FROM reorged);
DELETE FROM block WHERE id IN (SELECT id FROM reorged);
DROP TABLE ours;
DROP TABLE reorged;

-- 1. The indexes, their pointers as positions.
CREATE TABLE wallet_index_new (
    id             TEXT    NOT NULL PRIMARY KEY,
    wallet_id      TEXT    NOT NULL REFERENCES wallet (id) ON DELETE CASCADE,
    asset          TEXT    NOT NULL,
    requested_from INTEGER NOT NULL,
    birthday       INTEGER NOT NULL,
    indexed_at     INTEGER,
    state          INTEGER NOT NULL,
    reason         TEXT,
    CHECK (id = wallet_id || '.' || asset),
    CHECK (state IN (0, 1, 2))
);
INSERT INTO wallet_index_new (
    id, wallet_id, asset, requested_from, birthday, indexed_at, state, reason
)
SELECT
    wi.id, wi.wallet_id, wi.asset, wi.requested_from, wi.birthday,
    (SELECT b.position FROM block b WHERE b.id = wi.indexed_at),
    wi.state, wi.reason
FROM wallet_index wi;
DROP TABLE wallet_index;
ALTER TABLE wallet_index_new RENAME TO wallet_index;
CREATE INDEX wallet_index_wallet ON wallet_index (wallet_id);
CREATE INDEX wallet_index_progress ON wallet_index (indexed_at);

-- 1. Found: `found_at`; its state only the index's own.
CREATE TABLE transaction_index_new (
    id             TEXT    NOT NULL PRIMARY KEY,
    chain          TEXT    NOT NULL,
    network        TEXT    NOT NULL,
    txid           TEXT    NOT NULL,
    broadcasted_at INTEGER NOT NULL,
    expires_height INTEGER,
    indexed_at     INTEGER,
    found_at       INTEGER,
    state          INTEGER NOT NULL,
    reason         TEXT,
    CHECK (id = chain || '.' || network || '.' || txid),
    CHECK (state IN (0, 1, 2))
);
INSERT INTO transaction_index_new (
    id, chain, network, txid, broadcasted_at, expires_height, indexed_at, found_at, state,
    reason
)
SELECT
    ti.id, ti.chain, ti.network, ti.txid, ti.broadcasted_at, ti.expires_height,
    ib.position,
    fb.position,
    IIF(ti.state IN (0, 1, 2), ti.state, 0),
    ti.reason
FROM transaction_index ti
LEFT JOIN block ib ON ib.id = ti.indexed_at
LEFT JOIN block fb ON fb.id = ti.found;
DROP TABLE transaction_index;
ALTER TABLE transaction_index_new RENAME TO transaction_index;
CREATE INDEX transaction_index_network ON transaction_index (chain, network);
CREATE INDEX transaction_index_progress ON transaction_index (indexed_at);

-- 3. Legs: an adjustment with no block, its position kept, its id by it.
CREATE TABLE leg_new (
    id            TEXT    NOT NULL PRIMARY KEY,
    block         TEXT    REFERENCES block (id) ON DELETE CASCADE, -- NULL: of a pending transaction, or an adjustment.
    "transaction" TEXT    REFERENCES "transaction" (id) ON DELETE CASCADE, -- NULL: a reward or an adjustment.
    leg_index     INTEGER NOT NULL, -- In its transaction, or the block's rewards; an adjustment's 0 or 1.
    kind          INTEGER NOT NULL, -- 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward, 6 adjustment.
    address       TEXT    NOT NULL,
    asset         TEXT    NOT NULL,
    amount        TEXT    NOT NULL, -- Exact atomic decimal.
    coin_txid     TEXT,             -- The UTXO created (received) or consumed (spent).
    coin_index    INTEGER,
    wallet_index_id TEXT NOT NULL REFERENCES wallet_index (id) ON DELETE CASCADE,
    position      INTEGER,          -- The block's, or an adjustment's own; NULL while pending.
    seq           INTEGER,          -- Per index, never reused; NULL while pending.
    total         TEXT,             -- Exact decimal, signed; NULL while pending.
    total_received TEXT,            -- Exact decimal; NULL while pending.
    total_sent    TEXT,             -- Exact decimal; NULL while pending.
    archived_at   INTEGER,          -- A received coin: the jump line that wrote it off.
    CHECK (kind BETWEEN 0 AND 6),
    CHECK (("transaction" IS NULL) = (kind IN (5, 6))),
    CHECK (kind <> 6 OR leg_index IN (0, 1)),
    CHECK ((coin_txid IS NULL) = (coin_index IS NULL)),
    CHECK (coin_txid IS NULL OR kind IN (0, 1)),
    CHECK (archived_at IS NULL OR (kind = 0 AND coin_txid IS NOT NULL)),
    CHECK (kind <> 6 OR block IS NULL),
    CHECK ((position IS NULL) = (total IS NULL)) -- Pending legs: neither.
);
INSERT INTO leg_new (
    id, block, "transaction", leg_index, kind, address, asset, amount, coin_txid, coin_index,
    wallet_index_id, position, seq, total, total_received, total_sent, archived_at
)
SELECT
    IIF(l.kind = 6, l.wallet_index_id || '.adjustment.' || l.position || '.' || l.leg_index, l.id),
    IIF(l.kind = 6, NULL, l.block),
    l."transaction", l.leg_index, l.kind, l.address, l.asset, l.amount, l.coin_txid,
    l.coin_index, l.wallet_index_id, l.position, l.seq, l.total, l.total_received,
    l.total_sent, l.archived_at
FROM leg l;
DROP TABLE leg;
ALTER TABLE leg_new RENAME TO leg;
CREATE INDEX leg_address ON leg (address, asset);
-- An index's last leg at or below a position: one seek.
CREATE INDEX leg_wallet_index ON leg (wallet_index_id, position, seq);
CREATE UNIQUE INDEX leg_seq ON leg (wallet_index_id, seq);
CREATE INDEX leg_transaction ON leg ("transaction");
-- A block's legs: what deleting it deletes.
CREATE INDEX leg_block ON leg (block);
CREATE INDEX leg_coin ON leg (coin_txid, coin_index) WHERE coin_txid IS NOT NULL;

-- 5. The blocks nothing holds any more, below each network's confirmed
-- position, but its newest one; a kept child's `parent` is unset by hand
-- (foreign keys are off).
CREATE TEMP TABLE emptied AS
SELECT b.id
FROM block b
JOIN network_index i ON i.chain = b.chain
    AND i.network = b.network
WHERE b.position < i.confirmed
  AND b.position < (
      SELECT MAX(newest.position)
      FROM block newest
      WHERE newest.chain = b.chain
        AND newest.network = b.network
  )
  AND NOT EXISTS (SELECT 1 FROM leg l WHERE l.block = b.id)
  AND NOT EXISTS (SELECT 1 FROM "transaction" t WHERE t.block = b.id);
UPDATE block SET parent = NULL WHERE parent IN (SELECT id FROM emptied);
DELETE FROM block WHERE id IN (SELECT id FROM emptied);
DROP TABLE emptied;

-- 6. `block` without `reorged_at`, one block per position; `transaction`
-- with its block's `position`.
CREATE TABLE block_new (
    id              TEXT    NOT NULL PRIMARY KEY,
    chain           TEXT    NOT NULL,
    network         TEXT    NOT NULL,
    position        INTEGER NOT NULL, -- Native coordinate (height or slot).
    height          INTEGER NOT NULL, -- Produced-block count.
    hash            BLOB    NOT NULL,
    -- The parent block's id once it is stored: NULL for the first stored
    -- block of a run, set in the write that later stores the parent; NULL
    -- again once the parent is pruned.
    parent          TEXT    REFERENCES block (id) ON DELETE SET NULL,
    -- The parent as reported; the position is kept because the header is
    -- read back whole while `parent` is still NULL.
    parent_position INTEGER,          -- NULL exactly for genesis.
    parent_hash     BLOB,
    timestamp       INTEGER,          -- Unix seconds, when reported.
    CHECK (id = chain || '.' || network || '.' || lower(hex(hash))),
    CHECK ((parent_position IS NULL) = (parent_hash IS NULL)),
    CHECK (parent IS NULL
        OR parent = chain || '.' || network || '.' || lower(hex(parent_hash)))
);
INSERT INTO block_new (
    id, chain, network, position, height, hash, parent, parent_position, parent_hash, timestamp
)
SELECT
    id, chain, network, position, height, hash, parent, parent_position, parent_hash, timestamp
FROM block;
DROP TABLE block;
ALTER TABLE block_new RENAME TO block;
-- At most one block per position.
CREATE UNIQUE INDEX block_position ON block (chain, network, position);
CREATE INDEX block_parent_hash ON block (chain, network, parent_hash)
    WHERE parent IS NULL;
-- A block's kept children: what deleting it unlinks.
CREATE INDEX block_parent ON block (parent);


CREATE TABLE transaction_new (
    id      TEXT    NOT NULL PRIMARY KEY,
    block   TEXT    REFERENCES block (id) ON DELETE CASCADE, -- NULL: pending, not mined.
    chain   TEXT    NOT NULL,
    network TEXT    NOT NULL,
    txid    TEXT    NOT NULL,          -- Chain-native id.
    "order" INTEGER,                   -- Position in the block.
    state   INTEGER NOT NULL,          -- 0 pending, 1 succeeded, 2 reverted.
    reason  TEXT,                      -- JSON Reason; reverted only.
    bytes   BLOB    NOT NULL,
    broadcasted_at INTEGER,            -- Unix ms this server recorded it, before its first broadcast; NULL: not ours.
    position INTEGER,                  -- Its block's; NULL while pending.
    CHECK (id = chain || '.' || network || '.' || txid),
    CHECK (state IN (0, 1, 2)),
    CHECK (state = 2 OR reason IS NULL)
);
-- When each broadcast job started, by every txid it recorded (current or
-- replaced).
CREATE TEMP TABLE recorded AS
SELECT v.value AS txid, MIN(q.created_at) AS at
FROM q, json_each(json_array(
    json_extract(q.state, '$.txid'),
    json_extract(q.state, '$.replaced')
)) AS r, json_each(IIF(r.type = 'array', r.value, json_array(r.value))) AS v
WHERE q.kind = 'broadcast'
  AND q.state IS NOT NULL
  AND v.value IS NOT NULL
GROUP BY v.value;
INSERT INTO transaction_new (
    id, block, chain, network, txid, "order", state, reason, bytes, broadcasted_at, position
)
SELECT
    t.id, t.block, t.chain, t.network, t.txid, t."order", t.state, t.reason, t.bytes,
    IIF(
        t.block IS NULL OR t.id IN (SELECT id FROM transaction_index),
        COALESCE(
            t.sent_at,
            (SELECT r.at FROM recorded r WHERE r.txid = t.txid),
            CAST(unixepoch('subsec') * 1000 AS INTEGER)
        ),
        NULL
    ),
    (SELECT b.position FROM block b WHERE b.id = t.block)
FROM "transaction" t;
DROP TABLE recorded;
DROP TABLE "transaction";
ALTER TABLE transaction_new RENAME TO "transaction";
CREATE INDEX transaction_block ON "transaction" (block);

-- 4. The views, as `apps/server/init.sql` has them (last: after every
-- rebuild).

-- A transaction with its legs (`legs`, JSON as the `Leg` model serializes them, by
-- leg_index): what every transaction read answers.
CREATE VIEW transaction_with_legs AS
SELECT
    t.*,
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
FROM "transaction" t;

-- A wallet index with its wallet's network, address and tags, whether its
-- key is kept, the position it is indexed through and how far that is
-- behind its network's indexer tip, and the indexer's confirmed position
-- (`finality_at`). Its balances are NULL before its first block (NULL
-- `indexed_at`: a chain that reads balances has not settled it yet).
-- Its confirmed balances, NULL while that is unknown
-- (no `network_index` confirmed position): `balance_confirmed`,
-- `balance_confirmed_received`, `balance_confirmed_sent`, the totals of its
-- last mined leg at or below `finality_at` (`c`; '0': none);
-- `balance_confirmed_spendable`, `balance_confirmed` less its outgoing legs
-- (spent, burned, fee) not yet in it: pending (no block), or mined in a
-- block above `finality_at`. `balance_pending`: what it will hold once
-- every pending transaction lands: the total of its newest mined leg,
-- any block (`n`; '0': none), with its pending legs (what this server sent,
-- not mined yet), in and out. Each
-- leg is one seek of `leg_wallet_index`. `SUM_DECIMAL` is the server's:
-- read only through its connections.
CREATE VIEW wallet_index_with_balance AS
SELECT
    wi.*,
    w.chain,
    w.network,
    w.address,
    w.tags,
    w.secret IS NOT NULL AS keyed,
    wi.indexed_at AS indexed_through,
    MAX(i.tip - wi.indexed_at, 0) AS behind_tip,
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
            WHERE m.wallet_index_id = wi.id
              AND m.position > i.confirmed
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
LEFT JOIN network_index i ON i.chain = w.chain
    AND i.network = w.network
LEFT JOIN leg c ON c.id = (
    SELECT l.id
    FROM leg l
    WHERE l.wallet_index_id = wi.id
      AND l.position <= i.confirmed
    ORDER BY l.position DESC, l.seq DESC
    LIMIT 1
)
LEFT JOIN leg n ON n.id = (
    SELECT l.id
    FROM leg l
    WHERE l.wallet_index_id = wi.id
      AND l.position IS NOT NULL
    ORDER BY l.position DESC, l.seq DESC
    LIMIT 1
);


PRAGMA foreign_key_check;

COMMIT;

PRAGMA foreign_keys = ON;
