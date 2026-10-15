-- Durable job queue (packages/tasks `Store`, the server's storage/tasks.rs).
-- Executed by the caller (the server: `migrate`) beside sdk.sql and
-- server.sql; no migrations.

-- One row per job for its whole life; finished rows are the record. The store
-- keeps jobs whole; every rule (lanes, attempts, deadlines, recovery) is the
-- runner's, and its bookkeeping lives in `payload`.
CREATE TABLE q (
    id         TEXT    NOT NULL, -- UUIDv7: time ordered, the lane order.
    lane       TEXT    NOT NULL,
    kind       TEXT    NOT NULL, -- Executor selector.
    state      INTEGER NOT NULL, -- 0 pending, 1 running, 2 done, 3 failed.
    payload    TEXT    NOT NULL, -- JSON: input plus what the worker saves.
    run_at     INTEGER NOT NULL, -- ms since epoch; not run before.
    created_at INTEGER NOT NULL, -- ms.
    updated_at INTEGER NOT NULL, -- ms; last set.
    PRIMARY KEY (id),
    CHECK (state IN (0, 1, 2, 3)),
    CHECK (json_valid(payload))
) WITHOUT ROWID;
CREATE INDEX q_unfinished ON q (id) WHERE state IN (0, 1);
