-- Durable job queue (packages/tasks `Store`, the server's storage/tasks.rs).
-- Executed by the caller (the server: `migrate`) beside sdk.sql and
-- server.sql; no migrations.

-- One row per job for its whole life; finished rows are the record. Queue
-- fields are the queue's; `input` and `state` are the job's worker's.
CREATE TABLE q (
    id         TEXT    NOT NULL, -- UUIDv7: time ordered, the lane order.
    topic      TEXT    NOT NULL, -- What it locks: `a.b.c`; a prefix conflicts.
    kind       TEXT    NOT NULL, -- Worker selector.
    status     INTEGER NOT NULL, -- 0 pending, 1 running, 2 done, 3 failed.
    run_at     INTEGER NOT NULL, -- ms since epoch; not started before.
    deadline   INTEGER,          -- ms since epoch; not started after.
    attempts   INTEGER NOT NULL, -- Executions started.
    reason     TEXT,             -- Why it failed, or why it last retried.
    input      TEXT    NOT NULL, -- JSON: what to do; never changed.
    state      TEXT,             -- JSON: what the worker has done so far.
    created_at INTEGER NOT NULL, -- ms.
    updated_at INTEGER NOT NULL, -- ms; last written.
    PRIMARY KEY (id),
    CHECK (status IN (0, 1, 2, 3)),
    CHECK (attempts >= 0),
    CHECK (json_valid(input)),
    CHECK (state IS NULL OR json_valid(state))
) WITHOUT ROWID;
-- What `next` compares against: the unfinished jobs.
CREATE INDEX q_unfinished ON q (id, topic) WHERE status IN (0, 1);
