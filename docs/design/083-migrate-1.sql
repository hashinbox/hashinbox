-- Design 083, iteration 1: brings a database made by the previous
-- `apps/server/init.sql` to the new one, keeping every row. `init.sql` only
-- creates; it never alters a kept file. Run once, server stopped, after a
-- backup:
--
--   sqlite3 payments.db < docs/design/083-migrate-1.sql
--
-- `block.archived_at` becomes `reorged_at`: SQLite rewrites the `block_live`
-- index and the `wallet_index_with_balance` view with it.
-- `network.max_history_days` becomes `max_depth`, from days to positions
-- (`days * 86400000 / block_time_ms`); NULL stays NULL (no limit). Ethereum
-- takes at most 100 (design 083 §4): its config refuses more than 128 less
-- its `batch` (§11). Solana takes at most 150: an index further behind
-- jumps (§8).

BEGIN;

ALTER TABLE block RENAME COLUMN archived_at TO reorged_at;

ALTER TABLE network RENAME COLUMN max_history_days TO max_depth;
UPDATE network
SET max_depth = max_depth * 86400000 / block_time_ms
WHERE max_depth IS NOT NULL;
UPDATE network
SET max_depth = 100
WHERE chain = 'ethereum'
  AND max_depth > 100;
UPDATE network
SET max_depth = 150
WHERE chain = 'solana'
  AND max_depth > 150;

COMMIT;
