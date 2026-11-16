-- Design 083, iteration 2: the indexes block pruning needs on a database
-- made by an earlier `apps/server/init.sql`. `block.parent` and `leg.block`
-- are foreign keys to `block`: deleting a block looks both up. Run once,
-- after a backup:
--
--   sqlite3 payments.db < docs/design/083-migrate-2.sql

CREATE INDEX IF NOT EXISTS block_parent ON block (parent);
CREATE INDEX IF NOT EXISTS leg_block ON leg (block);
