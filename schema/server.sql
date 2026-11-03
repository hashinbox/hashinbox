-- The payment server's own table, executed with schema/sdk.sql on the same
-- new file by the caller (`payment-server migrate`). No migrations.
--
-- Wallets are the SDK's `wallet` rows, their tags included; the server keeps
-- no wallet table of its own. It may read the SDK's tables; it writes only
-- its own, and a wallet's tags.

-- One row per network: its RPC endpoint documents in failover order, as one
-- JSON list. Seeded from the configuration file once, replaced by verified
-- edits.
CREATE TABLE rpc_endpoints (
    chain   TEXT NOT NULL,
    network TEXT NOT NULL,
    nodes   TEXT NOT NULL,
    PRIMARY KEY (chain, network)
) WITHOUT ROWID;


-- One trigger (pipes): its selector, filter, check and action as JSON, the
-- types of `apps/server/src/models/trigger.rs`. id: its slug, chosen at creation.
CREATE TABLE "trigger" (
    id         TEXT    NOT NULL PRIMARY KEY,
    chain      TEXT    NOT NULL,
    network    TEXT    NOT NULL,
    group_id   TEXT    NOT NULL, -- The batch it was created in: one UUIDv7 per create.
    selector   TEXT    NOT NULL CHECK (json_valid(selector)),
    filter     TEXT             CHECK (json_valid(filter)), -- NULL: no filter.
    "check"    TEXT    NOT NULL CHECK (json_valid("check")),
    action     TEXT    NOT NULL CHECK (json_valid(action)),
    enabled    INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at INTEGER NOT NULL, -- Unix ms.
    updated_at INTEGER NOT NULL, -- Unix ms.
    -- Its latest run, set in the write that records it; NULL: never run.
    last_run   TEXT    REFERENCES trigger_run (id) ON DELETE SET NULL,
    CHECK (length(id) BETWEEN 1 AND 64 AND id NOT GLOB '*[^a-z0-9-]*')
);
CREATE INDEX trigger_network ON "trigger" (chain, network);
CREATE INDEX trigger_group ON "trigger" (group_id);

-- One run of a trigger: started when its action is decided, with the jobs
-- it queued.
-- id: "{trigger}.{started_at}".
CREATE TABLE trigger_run (
    id         TEXT    NOT NULL PRIMARY KEY,
    "trigger"  TEXT    NOT NULL REFERENCES "trigger" (id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL, -- Unix ms.
    jobs       TEXT    NOT NULL CHECK (json_valid(jobs)), -- The send jobs' ids, a JSON list.
    CHECK (id = "trigger" || '.' || started_at)
);
CREATE INDEX trigger_run_latest ON trigger_run ("trigger", started_at);
