-- Design 095: USD totals on wallet indexes. Brings a database made by the
-- previous `apps/server/init.sql` (`094-migrate-1.sql` applied) to the new
-- one. Run once, with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/095-migrate-1.sql
--
-- `wallet_index_with_balance` gains `balance_confirmed_received_usd` and
-- `balance_confirmed_sent_usd`: its confirmed leg's `total_received_usd`
-- and `total_sent_usd`.

BEGIN;

DROP VIEW wallet_index_with_balance;
CREATE VIEW wallet_index_with_balance AS
SELECT
    wi.*,
    w.chain,
    w.network,
    w.address,
    w.tags,
    w.secret IS NOT NULL AS keyed,
    wi.indexed_at AS indexed_through,
    MAX(i.tip - wi.indexed_at, 0) AS behind_tip,
    i.confirmed AS finality_at,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total, '0')) AS balance_confirmed,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total_received, '0')) AS balance_confirmed_received,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total_sent, '0')) AS balance_confirmed_sent,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total_received_usd, '0')) AS balance_confirmed_received_usd,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, COALESCE(c.total_sent_usd, '0')) AS balance_confirmed_sent_usd,
    IIF(i.confirmed IS NULL OR wi.indexed_at IS NULL, NULL, (
        SELECT SUM_DECIMAL(v)
        FROM (
            SELECT COALESCE(c.total, '0') AS v
            UNION ALL
            -- Same as `block IS NULL` (CHECK); lets `leg_wallet_index` seek
            -- straight to the pending legs.
            SELECT '-' || p.amount
            FROM leg p
            WHERE p.wallet_index_id = wi.id
              AND p.position IS NULL
              AND p.kind IN (1, 2, 4)
            UNION ALL
            SELECT '-' || m.amount
            FROM leg m
            WHERE m.wallet_index_id = wi.id
              AND m.position > i.confirmed
              AND m.kind IN (1, 2, 4)
        )
    )) AS balance_confirmed_spendable,
    IIF(wi.indexed_at IS NULL, NULL, (
        SELECT SUM_DECIMAL(v)
        FROM (
            SELECT COALESCE(n.total, '0') AS v
            UNION ALL
            SELECT IIF(p.kind IN (1, 2, 4), '-' || p.amount, p.amount)
            FROM leg p
            WHERE p.wallet_index_id = wi.id
              AND p.position IS NULL
        )
    )) AS balance_pending
FROM wallet_index wi
JOIN wallet w ON w.id = wi.wallet_id
LEFT JOIN network_index i ON i.chain = w.chain
    AND i.network = w.network
LEFT JOIN leg c ON c.id = (
    SELECT l.id
    FROM leg l
    WHERE l.wallet_index_id = wi.id
      AND l.position <= i.confirmed
    ORDER BY l.position DESC, l.seq DESC
    LIMIT 1
)
LEFT JOIN leg n ON n.id = (
    SELECT l.id
    FROM leg l
    WHERE l.wallet_index_id = wi.id
      AND l.position IS NOT NULL
    ORDER BY l.position DESC, l.seq DESC
    LIMIT 1
);

COMMIT;
