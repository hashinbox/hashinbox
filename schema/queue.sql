-- Durable task queue (packages/tasks, docs/design/058). Executed by the
-- caller (the server: `migrate`) beside sdk.sql and server.sql; no migrations.

-- Live tasks: pending or running. A lane's head (lowest id) is the only one
-- claimable, so a lane runs one task at a time in order.
CREATE TABLE q (
    id           TEXT    NOT NULL, -- UUIDv7: time ordered, the lane order.
    lane         TEXT    NOT NULL,
    kind         TEXT    NOT NULL, -- Executor selector.
    payload      BLOB    NOT NULL,
    run_at       INTEGER NOT NULL, -- ms since epoch; not claimed before.
    deadline     INTEGER,          -- ms; due at or after it, the task expires.
    attempts     INTEGER NOT NULL, -- Claims so far.
    max_attempts INTEGER NOT NULL,
    backoff      INTEGER NOT NULL, -- ms per attempt before a retry.
    state        INTEGER NOT NULL, -- 0 pending, 1 running.
    owner        TEXT,             -- Lease holder while running.
    lease_until  INTEGER,          -- ms; an expired lease is reclaimable.
    error        TEXT,             -- Last retry reason.
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (id),
    CHECK (state IN (0, 1)),
    CHECK ((state = 0 AND owner IS NULL) OR (state = 1 AND owner IS NOT NULL))
) WITHOUT ROWID;
CREATE INDEX q_lane ON q (lane, id);

-- Terminal outcomes, written once on the move out of q.
CREATE TABLE q_result (
    task_id     TEXT    NOT NULL,
    outcome     INTEGER NOT NULL, -- 0 succeeded, 1 rejected, 2 failed,
                                  -- 3 ambiguous, 4 expired, 5 cancelled.
    output      BLOB,
    error       TEXT,
    attempts    INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    PRIMARY KEY (task_id),
    CHECK (outcome BETWEEN 0 AND 5)
) WITHOUT ROWID;
