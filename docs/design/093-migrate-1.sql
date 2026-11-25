-- Design 093: brings a database made by the previous
-- `apps/server/init.sql` (`091-migrate-1.sql` applied) to the new one. Run
-- with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/093-migrate-1.sql
--
-- `log` gains `wallet_id`, `transaction_id`, `trigger_id` and `market`,
-- their indexes and one by time (`at`). It is dropped (if there) and made again
-- in its new shape: every kept log entry is discarded, on purpose (they are
-- diagnostics, pruned anyway). Safe to run again on a database it already
-- brought here.

BEGIN;

DROP TABLE IF EXISTS log;

-- One event the server kept (`usecase::logs::DbLayer`, `--log-db`): at: Unix
-- ms; level: error, warn, info, debug or trace; target: the event's code
-- (`rpc.http.failed`, `index.step`, ...); message: its own text, NULL for
-- its code's template (`apps/server/logs.json`); fields: its and its
-- spans' key=vals as a JSON object of text; chain, network: its network
-- (a `network` field), both NULL for one of no network; job_id: the job
-- it ran in (a `job_id` field), NULL outside a job, its logs deleted with
-- it; wallet_id,
-- transaction_id: the wallet (`{chain}.{network}.{address}`) and the
-- transaction (`{chain}.{network}.{txid}`) it concerns (a `wallet_id` or
-- `wallet` field, a `transaction_id` or `txid` field), NULL for none;
-- trigger_id: the trigger it concerns (a `trigger_id` or `trigger` field),
-- NULL for none; market: the market it concerns (a `market` field), NULL
-- for none. Written in batches; the prune job keeps debug and trace
-- 24 hours, the rest 30 days, and at most the newest 10 000 of each job,
-- and of each network's (and no network's) entries outside a job.
CREATE TABLE log (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    at             INTEGER NOT NULL,
    level          TEXT    NOT NULL,
    target         TEXT    NOT NULL,
    message        TEXT,
    fields         TEXT    NOT NULL CHECK (json_valid(fields)),
    chain          TEXT,
    network        TEXT,
    job_id         INTEGER REFERENCES q (id) ON DELETE CASCADE,
    wallet_id      TEXT,
    transaction_id TEXT,
    trigger_id     TEXT,
    market         TEXT,
    CHECK ((chain IS NULL) = (network IS NULL)),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network) ON DELETE CASCADE
);
-- A network's entries, newest first: one seek.
CREATE INDEX log_network ON log (chain, network, id);
-- One event's entries, newest first.
CREATE INDEX log_target ON log (target, id);
-- A job's entries, newest first.
CREATE INDEX log_job ON log (job_id, id);
-- A wallet's entries, newest first.
CREATE INDEX log_wallet ON log (wallet_id, id);
-- A transaction's entries, newest first.
CREATE INDEX log_transaction ON log (transaction_id, id);
-- A trigger's entries, newest first.
CREATE INDEX log_trigger ON log (trigger_id, id);
-- A market's entries, newest first.
CREATE INDEX log_market ON log (market, id);
-- Entries by time: a jump to a moment, and the prune.
CREATE INDEX log_at ON log (at);

COMMIT;
