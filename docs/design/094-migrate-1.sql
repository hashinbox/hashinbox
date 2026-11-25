-- Design 094: USD prices. Brings a database made by the previous
-- `apps/server/init.sql` (`093-migrate-1.sql` applied) to the new one. Run
-- once, with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/094-migrate-1.sql
--
-- `leg` gains `amount_usd`, `total_received_usd` and `total_sent_usd`
-- (NULL for every leg kept before: set when a leg is mined, never filled
-- later); `transaction_with_legs` shows them; `market` (its rows, as
-- `seed.sql`) and `market_candle_1h` are new (empty: the market job fills
-- it; nothing before).

BEGIN;

ALTER TABLE leg ADD COLUMN amount_usd TEXT;
ALTER TABLE leg ADD COLUMN total_received_usd TEXT;
ALTER TABLE leg ADD COLUMN total_sent_usd TEXT;
DROP INDEX IF EXISTS leg_unpriced;

DROP VIEW transaction_with_legs;
CREATE VIEW transaction_with_legs AS
SELECT
    t.*,
    COALESCE((
        SELECT json_group_array(
            json_object(
                'id', l.id,
                'block', l.block,
                'transaction', l."transaction",
                'leg_index', l.leg_index,
                'kind', l.kind,
                'address', l.address,
                'asset', l.asset,
                'amount', l.amount,
                'coin_txid', l.coin_txid,
                'coin_index', l.coin_index,
                'wallet_index_id', l.wallet_index_id,
                'position', l.position,
                'seq', l.seq,
                'total', l.total,
                'total_received', l.total_received,
                'total_sent', l.total_sent,
                'archived_at', l.archived_at,
                'amount_usd', l.amount_usd,
                'total_received_usd', l.total_received_usd,
                'total_sent_usd', l.total_sent_usd
            )
            ORDER BY l.leg_index
        )
        FROM leg l
        WHERE l."transaction" = t.id
    ), '[]') AS legs
FROM "transaction" t;

-- As `init.sql`: the markets, and one market's hour of an asset's USD
-- price.
CREATE TABLE market (
    name       TEXT    PRIMARY KEY,
    feed       INTEGER NOT NULL DEFAULT 1 CHECK (feed IN (0, 1)),
    updated_at INTEGER NOT NULL
);
INSERT INTO market (name, feed, updated_at)
VALUES
    ('binance', 1, 0),
    ('bitstamp', 1, 0),
    ('coinbase', 1, 0),
    ('coingecko', 1, 0),
    ('kraken', 1, 0);

CREATE TABLE market_candle_1h (
    market     TEXT    NOT NULL REFERENCES market (name),
    asset      TEXT    NOT NULL,
    start      INTEGER NOT NULL,
    open       TEXT    NOT NULL,
    high       TEXT    NOT NULL,
    low        TEXT    NOT NULL,
    close      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (market, asset, start),
    CHECK (start % 3600 = 0)
);
CREATE INDEX market_candle_1h_start ON market_candle_1h (start, asset);

COMMIT;
