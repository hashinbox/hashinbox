-- Design 085: brings a database made by the previous
-- `apps/server/init.sql` (`085-migrate-4.sql` applied) to the new one,
-- keeping every row. Run once, server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/085-migrate-5.sql
--
-- A transaction index settled through the chain's Confirmer keeps whether
-- its transaction reverted (`transaction_index.found_reverted`): its found
-- block is kept without transactions, so no mined row tells. The
-- `transaction_index_with_blocks` view reads it into `reverted`; it is
-- dropped and created again. Indexes settled before stay not reverted.

BEGIN;

DROP VIEW transaction_index_with_blocks;

ALTER TABLE transaction_index
    ADD COLUMN found_reverted INTEGER NOT NULL DEFAULT 0 CHECK (found_reverted IN (0, 1));

-- A transaction index as every read answers it: its row, whether its found
-- transaction reverted (kept mined reverted, or as the Confirmer told), and
-- its network's confirmed position.
CREATE VIEW transaction_index_with_blocks AS
SELECT
    ti.*,
    ti.found IS NOT NULL AND (t.state IS 2 OR ti.found_reverted) AS reverted,
    i.confirmed AS confirmed
FROM transaction_index ti
LEFT JOIN "transaction" t ON t.id = ti.id
LEFT JOIN network_index i ON i.chain = ti.chain
    AND i.network = ti.network;

COMMIT;
