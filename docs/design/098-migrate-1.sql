-- Design 098: the instance's settings. Brings a database made by the
-- previous `apps/server/init.sql` (`096-migrate-1.sql` applied) to the new
-- one. Run once, with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/098-migrate-1.sql
--
-- Adds the `setting` table, empty: every key is its default
-- (`apps/server/src/models/setting.rs`) until written by
-- `PUT /v1/settings/{key}`. The `--brand` flag (`HASHINBOX_UI_BRAND`) is
-- gone: the dashboard's name is the `instance` setting.

BEGIN;

CREATE TABLE setting (
    key        TEXT    PRIMARY KEY,
    value      TEXT    NOT NULL,
    public     INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

COMMIT;
