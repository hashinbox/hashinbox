-- Design 085: brings a database made by the previous
-- `apps/server/init.sql` (`084-migrate-1.sql` applied) to the new one,
-- keeping every row. Run once, server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/085-migrate-1.sql
--
-- `transaction_index` is added: how far the blocks after each sent
-- transaction are read for it. The broadcast job reads only its payment's
-- index, so every pending transaction gets one here, not started, from its
-- network's last kept block (else its tip, else 0): the indexer reads on
-- from there and finds it once mined. A Solana payment's index has no
-- `expires_height` (it was not kept before): it is sent again until it is
-- mined, so let every unfinished Solana broadcast job finish before.
-- `network_index.watched` is dropped: nothing counts watches any more.

PRAGMA foreign_keys = OFF;

BEGIN;

CREATE TABLE transaction_index (
    id             TEXT    NOT NULL PRIMARY KEY,
    chain          TEXT    NOT NULL,
    network        TEXT    NOT NULL,
    txid           TEXT    NOT NULL,
    broadcasted_at INTEGER NOT NULL,
    expires_height INTEGER,
    indexed_at     TEXT    REFERENCES block (id),
    found          TEXT    REFERENCES block (id),
    state          INTEGER NOT NULL,
    reason         TEXT,
    CHECK (id = chain || '.' || network || '.' || txid),
    CHECK (state IN (0, 1, 2))
);
CREATE INDEX transaction_index_network ON transaction_index (chain, network);
CREATE INDEX transaction_index_progress ON transaction_index (indexed_at);

INSERT INTO transaction_index (id, chain, network, txid, broadcasted_at, state)
SELECT t.id, t.chain, t.network, t.txid, COALESCE(i.indexed, i.tip, 0), 0
FROM "transaction" t
LEFT JOIN network_index i ON i.chain = t.chain
    AND i.network = t.network
WHERE t.block IS NULL;

ALTER TABLE network_index DROP COLUMN watched;

PRAGMA foreign_key_check;

COMMIT;

PRAGMA foreign_keys = ON;
