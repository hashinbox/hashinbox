-- Design 085: brings a database made by the previous
-- `apps/server/init.sql` (`085-migrate-3.sql` applied) to the new one,
-- keeping every row but the stale jobs below. Run once, server stopped,
-- after a backup:
--
--   sqlite3 payments.db < docs/design/085-migrate-4.sql
--
-- The `expire` jobs are deleted: their worker was removed in 084 (no clock
-- frees a pending transaction any more) without a cleanup, so they stay
-- pending for ever. No schema change.

BEGIN;

DELETE FROM q WHERE kind = 'expire';

COMMIT;
