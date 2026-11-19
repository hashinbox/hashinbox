-- Design 085: brings a database made by the previous
-- `apps/server/init.sql` (`085-migrate-2.sql` applied) to the new one,
-- keeping every row. Run once, server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/085-migrate-3.sql
--
-- The `transaction_index_with_blocks` view is added: every transaction
-- index read answers it, with whether its found transaction reverted and
-- its network's confirmed position joined in SQL.

BEGIN;

-- A transaction index as every read answers it: its row, whether its found
-- transaction is kept reverted, and its network's confirmed position.
CREATE VIEW transaction_index_with_blocks AS
SELECT
    ti.*,
    ti.found IS NOT NULL AND t.state IS 2 AS reverted,
    i.confirmed AS confirmed
FROM transaction_index ti
LEFT JOIN "transaction" t ON t.id = ti.id
LEFT JOIN network_index i ON i.chain = ti.chain
    AND i.network = ti.network;

COMMIT;
