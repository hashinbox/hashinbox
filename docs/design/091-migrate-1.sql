-- Design 091: brings a database made by the previous
-- `apps/server/init.sql` (`087-migrate-1.sql` applied) to the new one,
-- keeping every row. Run once, server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/091-migrate-1.sql
--
-- `network_endpoint_stats` is new: one row per node, every stored node's
-- counts at zero, and a trigger making the row of each new node.
-- `network_endpoint_failure` is new: what disabled or enabled a node, and
-- why; no row: every stored node is enabled. `network_state` is new: a
-- network with its index and its nodes, the API's networks and nodes.
-- `log` is new: one row per kept event, pruned to 7 days and at most
-- 100 000 rows per network.

BEGIN;

-- One RPC node's counts, one row per `network_endpoint` row, made with it
-- (`network_endpoint_stats_made`): calls, failed_calls: its HTTP requests
-- since it was stored, and the failed ones, each written as it finishes;
-- updated_at: Unix ms of the last.
CREATE TABLE network_endpoint_stats (
    endpoint_id  INTEGER PRIMARY KEY REFERENCES network_endpoint (id) ON DELETE CASCADE,
    calls        INTEGER NOT NULL,
    failed_calls INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

-- Every stored node: its counts at zero.
INSERT INTO network_endpoint_stats (endpoint_id, calls, failed_calls, updated_at)
SELECT id, 0, 0, CAST(strftime('%s', 'now') AS INTEGER) * 1000
FROM network_endpoint;

-- Every stored node starts with its counts at zero.
CREATE TRIGGER network_endpoint_stats_made AFTER INSERT ON network_endpoint
BEGIN
    INSERT OR IGNORE INTO network_endpoint_stats (endpoint_id, calls, failed_calls, updated_at)
    VALUES (NEW.id, 0, 0, CAST(strftime('%s', 'now') AS INTEGER) * 1000);
END;

-- What disabled or enabled one RPC node, and why; its newest row is its
-- state: none or kind `enabled`, enabled; any other, disabled, `message`
-- why. kind: `http_401`, `http_403`, `http_404`, `rpc_401`, `rpc_403`,
-- `rpc_404` (a failure it cannot recover from), `wrong_chain` (it serves
-- another chain), `manual` (disabled by hand), `enabled` (enabled by hand).
-- at: Unix ms; message: why, never a URL. A failure it may recover from (a
-- rate limit, a timeout) is not here: it is in `log` (`rpc.http`).
CREATE TABLE network_endpoint_failure (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    endpoint_id INTEGER NOT NULL REFERENCES network_endpoint (id) ON DELETE CASCADE,
    at          INTEGER NOT NULL,
    kind        TEXT    NOT NULL,
    message     TEXT    NOT NULL
);
-- A node's records, newest first: one seek.
CREATE INDEX network_endpoint_failure_endpoint ON network_endpoint_failure (endpoint_id, id);

-- One network: its `network` row, and as JSON: `assets`, its
-- `network_asset` rows by asset; `routes`, its routed methods by method,
-- each one's endpoint ids in failover order; `index`, its `network_index`
-- row and `job_reason`, the reason of its newest index job (`q`, kind
-- `index`, topic `index:{chain}:{network}`), NULL when it has neither; and
-- `endpoints`, its nodes by position, each with its counts
-- (`network_endpoint_stats`) and `disabled`: its newest
-- `network_endpoint_failure` row unless that is an `enabled` one (NULL:
-- enabled). An empty list is `[]`.
CREATE VIEW network_state AS
WITH
newest_failure AS (
    SELECT * FROM network_endpoint_failure
    WHERE id IN (SELECT MAX(id) FROM network_endpoint_failure GROUP BY endpoint_id)
),
endpoint AS (
    SELECT
        e.chain,
        e.network,
        e.id,
        e.url,
        e.position,
        s.calls,
        s.failed_calls,
        s.updated_at,
        CASE WHEN f.kind <> 'enabled' THEN json_object(
            'id', f.id,
            'endpoint_id', f.endpoint_id,
            'at', f.at,
            'kind', f.kind,
            'message', f.message
        ) END AS disabled_by
    FROM network_endpoint e
    JOIN network_endpoint_stats s ON s.endpoint_id = e.id
    LEFT JOIN newest_failure f ON f.endpoint_id = e.id
),
endpoints AS (
    SELECT
        chain,
        network,
        json_group_array(json_object(
            'id', id,
            'url', url,
            'position', position,
            'calls', calls,
            'failed_calls', failed_calls,
            'updated_at', updated_at,
            'disabled', json(disabled_by)
        ) ORDER BY position, id) AS endpoints
    FROM endpoint
    GROUP BY chain, network
),
assets AS (
    SELECT
        chain,
        network,
        json_group_array(json_object(
            'asset', asset,
            'kind', kind,
            'contract', contract,
            'ticker', ticker,
            'decimals', decimals,
            'display_decimals', display_decimals
        ) ORDER BY asset) AS assets
    FROM network_asset
    GROUP BY chain, network
),
route AS (
    SELECT
        e.chain,
        e.network,
        r.method,
        json_group_array(e.id ORDER BY e.position, e.id) AS endpoint_ids
    FROM network_endpoint_route r
    JOIN network_endpoint e ON e.id = r.endpoint_id
    GROUP BY e.chain, e.network, r.method
),
routes AS (
    SELECT
        chain,
        network,
        json_group_array(json_object(
            'method', method,
            'endpoint_ids', json(endpoint_ids)
        ) ORDER BY method) AS routes
    FROM route
    GROUP BY chain, network
),
index_job AS (
    SELECT topic, reason FROM q
    WHERE id IN (SELECT MAX(id) FROM q WHERE kind = 'index' GROUP BY topic)
)
SELECT
    n.chain,
    n.network,
    n.chain_id,
    n.testnet,
    n.enabled,
    n.block_time_ms,
    n.confirmations,
    n.max_depth,
    n.explorer_address,
    n.explorer_transaction,
    n.settings,
    COALESCE(a.assets, '[]') AS assets,
    COALESCE(r.routes, '[]') AS routes,
    CASE WHEN i.chain IS NOT NULL OR j.reason IS NOT NULL THEN json_object(
        'tip', i.tip,
        'confirmed', i.confirmed,
        'indexed', i.indexed,
        'checked_at', i.checked_at,
        'blocks', COALESCE(i.blocks, 0),
        'reorgs', COALESCE(i.reorgs, 0),
        'failures', COALESCE(i.failures, 0),
        'job_reason', j.reason
    ) END AS "index",
    COALESCE(e.endpoints, '[]') AS endpoints
FROM network n
LEFT JOIN network_index i ON i.chain = n.chain AND i.network = n.network
LEFT JOIN endpoints e ON e.chain = n.chain AND e.network = n.network
LEFT JOIN assets a ON a.chain = n.chain AND a.network = n.network
LEFT JOIN routes r ON r.chain = n.chain AND r.network = n.network
LEFT JOIN index_job j ON j.topic = 'index:' || n.chain || ':' || n.network;


-- One event the server kept (`usecase::logs::DbLayer`, `--log-db`): at: Unix
-- ms; level: error, warn, info, debug or trace; target: the event
-- (`rpc.http`, `index.step`, ...); fields: its and its spans' key=vals as a
-- JSON object of text; chain, network: its network (a `network` field),
-- both NULL for one of no network; job_id: the job it ran in (a `job_id`
-- field), NULL outside a job, its logs deleted with it. Written in
-- batches; the prune job keeps 7 days, at most 100 000 rows per network
-- and as many of no network.
CREATE TABLE log (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at      INTEGER NOT NULL,
    level   TEXT    NOT NULL,
    target  TEXT    NOT NULL,
    message TEXT    NOT NULL,
    fields  TEXT    NOT NULL CHECK (json_valid(fields)),
    chain   TEXT,
    network TEXT,
    job_id  INTEGER REFERENCES q (id) ON DELETE CASCADE,
    CHECK ((chain IS NULL) = (network IS NULL)),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network) ON DELETE CASCADE
);
-- A network's entries, newest first: one seek.
CREATE INDEX log_network ON log (chain, network, id);
-- One event's entries, newest first.
CREATE INDEX log_target ON log (target, id);
-- A job's entries, newest first.
CREATE INDEX log_job ON log (job_id, id);

COMMIT;
