-- The payment server's own table, executed with schema/sdk.sql on the same
-- new file by the caller (`payment-server migrate`). No migrations.
--
-- Wallets are the SDK's `wallet` rows; the server keeps no wallet table of
-- its own. It may read the SDK's tables; it writes only its own.

-- One row per network: its RPC endpoint documents in failover order, as one
-- JSON list. Seeded from the configuration file once, replaced by verified
-- edits.
CREATE TABLE rpc_endpoints (
    chain   TEXT NOT NULL,
    network TEXT NOT NULL,
    nodes   TEXT NOT NULL,
    PRIMARY KEY (chain, network)
) WITHOUT ROWID;


-- One tag on an address of a network (keyed or watch-only alike): a label
-- that only selects the address's wallet indexes (pipes).
-- id: "{chain}.{network}.{address}.{tag}"; a tag is [a-z0-9-]{1,64}.
CREATE TABLE tag (
    id      TEXT NOT NULL PRIMARY KEY,
    chain   TEXT NOT NULL,
    network TEXT NOT NULL,
    address TEXT NOT NULL,
    tag     TEXT NOT NULL,
    CHECK (id = chain || '.' || network || '.' || address || '.' || tag),
    CHECK (length(tag) BETWEEN 1 AND 64 AND tag NOT GLOB '*[^a-z0-9-]*')
);
CREATE INDEX tag_address ON tag (chain, network, address);
CREATE INDEX tag_tag ON tag (chain, network, tag);


-- One trigger (pipes): its selector, filter, check and action as JSON, the
-- types of `apps/server/src/pipe`. id: its slug, chosen at creation.
CREATE TABLE "trigger" (
    id         TEXT    NOT NULL PRIMARY KEY,
    chain      TEXT    NOT NULL,
    network    TEXT    NOT NULL,
    selector   TEXT    NOT NULL CHECK (json_valid(selector)),
    filter     TEXT             CHECK (json_valid(filter)), -- NULL: no filter.
    "check"    TEXT    NOT NULL CHECK (json_valid("check")),
    action     TEXT    NOT NULL CHECK (json_valid(action)),
    enabled    INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at INTEGER NOT NULL, -- Unix ms.
    updated_at INTEGER NOT NULL, -- Unix ms.
    CHECK (length(id) BETWEEN 1 AND 64 AND id NOT GLOB '*[^a-z0-9-]*')
);
CREATE INDEX trigger_network ON "trigger" (chain, network);

-- One run of a trigger: started when its action is decided, settled when
-- every send of it is confirmed or definitively failed.
-- id: "{trigger}.{started_at}".
CREATE TABLE trigger_run (
    id         TEXT    NOT NULL PRIMARY KEY,
    "trigger"  TEXT    NOT NULL REFERENCES "trigger" (id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL, -- Unix ms.
    settled_at INTEGER,          -- Unix ms; NULL: a send is in flight.
    CHECK (id = "trigger" || '.' || started_at)
);
CREATE INDEX trigger_run_latest ON trigger_run ("trigger", started_at);
