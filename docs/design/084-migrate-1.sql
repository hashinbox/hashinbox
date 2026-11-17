-- Design 084, iterations 1 and 2: brings a database made by the previous
-- `apps/server/init.sql` (`083-migrate-4.sql` applied) to the new one,
-- keeping every row. Run once, server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/084-migrate-1.sql
--
-- `"transaction"` loses `expires_at`: no clock frees a pending
-- transaction any more, the pending job forgets it once the chain proves it
-- dead. Its CHECK becomes `block IS NULL OR sent_at IS NULL`, which SQLite
-- cannot alter: the table is rebuilt (create new, copy, drop, rename) with
-- its index; the view naming it (`transaction_with_legs`) is dropped first
-- and created again as it was. The `transaction_forgotten` table is added: every
-- pending transaction forgotten from now on is kept there. Foreign keys are
-- off while the table is swapped, and checked before the commit:
-- `foreign_key_check` must print nothing.
--
-- `sent_at` now means a node accepted it (NULL while recorded, not yet
-- sent); a pending row keeps the time it was recorded under the old rule,
-- so the pending job keeps sending it again. A broadcast job is now one
-- payment (`to`, `amount`, state `{txid, sent}`): let every unfinished
-- broadcast job finish before, since one of the old shape (`transfers`,
-- state `{ids, sent}`) fails `invalid-input` once claimed.

PRAGMA foreign_keys = OFF;

BEGIN;

DROP VIEW transaction_with_legs;

CREATE TABLE transaction_new (
    id      TEXT    NOT NULL PRIMARY KEY,
    block   TEXT    REFERENCES block (id), -- NULL: pending, not mined.
    chain   TEXT    NOT NULL,
    network TEXT    NOT NULL,
    txid    TEXT    NOT NULL,          -- Chain-native id.
    "order" INTEGER,                   -- Position in the block.
    state   INTEGER NOT NULL,          -- 0 pending, 1 succeeded, 2 reverted.
    reason  TEXT,                      -- JSON Reason; reverted only.
    bytes   BLOB    NOT NULL,
    sent_at INTEGER,                   -- Unix ms a node accepted it; NULL until then and once mined.
    CHECK (id = chain || '.' || network || '.' || txid),
    CHECK (state IN (0, 1, 2)),
    CHECK (state = 2 OR reason IS NULL),
    CHECK (block IS NULL OR sent_at IS NULL)
);
INSERT INTO transaction_new (id, block, chain, network, txid, "order", state, reason, bytes, sent_at)
SELECT id, block, chain, network, txid, "order", state, reason, bytes, sent_at
FROM "transaction";
DROP TABLE "transaction";
ALTER TABLE transaction_new RENAME TO "transaction";
CREATE INDEX transaction_block ON "transaction" (block);

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

CREATE TABLE transaction_forgotten (
    id           TEXT    NOT NULL PRIMARY KEY,
    chain        TEXT    NOT NULL,
    network      TEXT    NOT NULL,
    txid         TEXT    NOT NULL,
    reason       TEXT    NOT NULL,
    forgotten_at INTEGER NOT NULL, -- Unix ms.
    bytes        BLOB    NOT NULL,
    legs         TEXT    NOT NULL CHECK (json_valid(legs)),
    CHECK (id = chain || '.' || network || '.' || txid)
);

PRAGMA foreign_key_check;

COMMIT;

PRAGMA foreign_keys = ON;
