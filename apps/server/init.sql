-- The hashinbox server's one SQLite file, created by `hashinbox serve`
-- on a new file (`Db::open_or_create`, embedded from here). No migrations: a schema
-- change means a fresh file.

-- The SDK schema: what `base`'s stores keep (BlockStore, TransactionStore,
-- InputStore, WalletStore, WalletIndexStore), in the one SQLite file. Applied
-- once to a new file by the caller (`hashinbox serve`); no migrations.
-- Enable foreign_keys on every connection.
--
-- A network is (chain, network). Positions and heights are SQLite integers;
-- amounts are exact decimal TEXT, never summed in SQL.
--
-- Blocks are kept up to the produced tip. A reorg deletes the blocks the
-- chain took back in one write (design 087): their facts go by cascade, this
-- server's own sent transactions go back to pending, and every index at or
-- above is capped below. Confirmation is a read (position <= the network's
-- `network_index` confirmed position). Below a network's line (on a chain
-- that reads balances its final block, else its tip less its `max_depth`),
-- the prune job deletes blocks that hold no facts (`block::db::prune`).
-- Nothing else names a block: an index's progress and an adjustment are
-- positions (design 087).
--
-- Mined facts (transactions and legs, rewards included) are kept only for
-- (address, asset) pairs with a wallet_index the block applies to
-- (BlockStore::insert).
-- A coin is a `received` leg with a coin; it is spent while a `spent` leg in
-- a block names it, so reorging the spending block unspends it.

-- Keys: every table's `id` is TEXT, its natural key joined with dots (dots
-- are URL-safe), and every foreign key references such an id. No part holds
-- a dot: chain and network slugs, lowercase hex hashes, chain address text,
-- asset ids and txids; the server refuses a part with a dot (or an empty one)
-- when it builds an id (`models::key`).

-- id: "{chain}.{network}.{hash}". Kept again, it is not inserted twice.
CREATE TABLE block (
    id              TEXT    NOT NULL PRIMARY KEY,
    chain           TEXT    NOT NULL,
    network         TEXT    NOT NULL,
    position        INTEGER NOT NULL, -- Native coordinate (height or slot).
    height          INTEGER NOT NULL, -- Produced-block count.
    hash            BLOB    NOT NULL,
    -- The parent block's id once it is stored: NULL for the first stored
    -- block of a run, set in the write that later stores the parent; NULL
    -- again once the parent is pruned.
    parent          TEXT    REFERENCES block (id) ON DELETE SET NULL,
    -- The parent as reported; the position is kept because the header is
    -- read back whole while `parent` is still NULL.
    parent_position INTEGER,          -- NULL exactly for genesis.
    parent_hash     BLOB,
    timestamp       INTEGER,          -- Unix seconds, when reported.
    CHECK (id = chain || '.' || network || '.' || lower(hex(hash))),
    CHECK ((parent_position IS NULL) = (parent_hash IS NULL)),
    CHECK (parent IS NULL
        OR parent = chain || '.' || network || '.' || lower(hex(parent_hash)))
);
-- At most one block per position.
CREATE UNIQUE INDEX block_position ON block (chain, network, position);
CREATE INDEX block_parent_hash ON block (chain, network, parent_hash)
    WHERE parent IS NULL;
-- A block's kept children: what deleting it unlinks.
CREATE INDEX block_parent ON block (parent);

-- A transaction, once per txid of a network: pending (block NULL, kept by
-- TransactionStore::insert, a no-op when the txid is kept) or mined. Mining
-- (BlockStore::insert) upserts it: block, order, state, reason and bytes
-- become the mined ones and its legs are replaced by the mined legs;
-- `broadcasted_at` stays. This server's own sends are exactly the rows with
-- `broadcasted_at`. A reorg (design 087) deletes the others mined at or
-- above the position it takes back, their legs by cascade
-- (`transaction::db::delete_unbroadcast_from`), and sets ours there pending,
-- their legs too (`leg::db::set_pending_from`,
-- `transaction::db::set_pending_from`), before the blocks go: a reorg cannot
-- make funds appear, nor free what a send holds; its broadcast job owns it
-- until final, expired or forgotten. Nothing is left in a deleted block, so
-- its cascade never reaches a transaction.
-- id: "{chain}.{network}.{txid}".
CREATE TABLE "transaction" (
    id      TEXT    NOT NULL PRIMARY KEY,
    block   TEXT    REFERENCES block (id) ON DELETE CASCADE, -- NULL: pending, not mined.
    chain   TEXT    NOT NULL,
    network TEXT    NOT NULL,
    txid    TEXT    NOT NULL,          -- Chain-native id.
    "order" INTEGER,                   -- Position in the block.
    state   INTEGER NOT NULL,          -- 0 pending, 1 succeeded, 2 reverted.
    reason  TEXT,                      -- JSON Reason; reverted only.
    bytes   BLOB    NOT NULL,
    broadcasted_at INTEGER,            -- Unix ms this server recorded it, before its first broadcast; NULL: not ours.
    position INTEGER,                  -- Its block's; NULL while pending.
    CHECK (id = chain || '.' || network || '.' || txid),
    CHECK (state IN (0, 1, 2)),
    CHECK (state = 2 OR reason IS NULL)
);
CREATE INDEX transaction_block ON "transaction" (block);

-- One kept leg of a transaction, a block reward (value a block moved
-- with no transaction; signed), or an adjustment (design 083, signed, no
-- transaction and no block, only its position (design 087): leg 0, what
-- the chain's balance read says an index holds at a final position less its
-- total there (settle); leg 1, the coins a jump archived (write-off)); a
-- reorg's write deletes the adjustments at or above its position.
-- A coin a jump wrote off is archived (`archived_at`): never spendable
-- again. A pending transaction's legs have no block;
-- only legs of a block, and adjustments, count (balances, unspent
-- coins). Address and
-- asset are of the transaction's or block's network.
-- A mined leg is kept for the wallet index it applies to (and goes with
-- it), and carries its block's position, its `seq` and `total`: the
-- index's balance right after it, the previous mined leg's total plus
-- its signed amount (design 072), and `total_received` / `total_sent`:
-- what the chain showed come in (received, minted, reward) and go out
-- (spent, burned, fee) so far. An adjustment moves only `total`, so
-- `total = total_received - total_sent + the adjustments so far`. A spend
-- of a coin the index archived or never saw subtracts all the same: a
-- negative total means history is missing. `seq` is 1 + the highest the index has
-- ever kept (so never reused while kept, and unique: a second
-- writer of the same step is refused); within a block it follows
-- transaction order, then leg index, rewards after the transactions. An
-- index's legs order by (position, seq); its balance at a position is the
-- total of its last leg at or below it (none: 0). Written in that
-- order as blocks are kept, in one write transaction; a block kept again
-- (re-applied to an index) has its legs written again from the chain below
-- it. A pending leg has none of these: it is read by
-- address until mined. Deleting its block or its transaction deletes it.
-- id (built by `models/leg.rs`, not checked here): "{transaction}.{leg_index}",
-- "{block}.reward.{leg_index}", or
-- "{wallet_index_id}.adjustment.{position}.{leg_index}".
CREATE TABLE leg (
    id            TEXT    NOT NULL PRIMARY KEY,
    block         TEXT    REFERENCES block (id) ON DELETE CASCADE, -- NULL: of a pending transaction, or an adjustment.
    "transaction" TEXT    REFERENCES "transaction" (id) ON DELETE CASCADE, -- NULL: a reward or an adjustment.
    leg_index     INTEGER NOT NULL, -- In its transaction, or the block's rewards; an adjustment's 0 or 1.
    kind          INTEGER NOT NULL, -- 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward, 6 adjustment.
    address       TEXT    NOT NULL,
    asset         TEXT    NOT NULL,
    amount        TEXT    NOT NULL, -- Exact atomic decimal.
    coin_txid     TEXT,             -- The UTXO created (received) or consumed (spent).
    coin_index    INTEGER,
    wallet_index_id TEXT NOT NULL REFERENCES wallet_index (id) ON DELETE CASCADE,
    position      INTEGER,          -- The block's, or an adjustment's own; NULL while pending.
    seq           INTEGER,          -- Per index, never reused; NULL while pending.
    total         TEXT,             -- Exact decimal, signed; NULL while pending.
    total_received TEXT,            -- Exact decimal; NULL while pending.
    total_sent    TEXT,             -- Exact decimal; NULL while pending.
    archived_at   INTEGER,          -- A received coin: the jump line that wrote it off.
    -- Exact decimal USD, signed as the leg adds to its balance (negative
    -- for a spend, burn or fee): amount (in display units) times
    -- its asset's latest USD price when it was kept: the median close of
    -- the markets' newest hour of the asset (the market job's, in
    -- memory). NULL when there was none, and for a pending leg or an
    -- adjustment; never filled later.
    amount_usd    TEXT,
    -- As total_received and total_sent, in USD: each leg's amount_usd (none:
    -- 0) added. NULL while pending.
    total_received_usd TEXT,
    total_sent_usd TEXT,
    CHECK (kind BETWEEN 0 AND 6),
    CHECK (("transaction" IS NULL) = (kind IN (5, 6))),
    CHECK (kind <> 6 OR leg_index IN (0, 1)),
    CHECK ((coin_txid IS NULL) = (coin_index IS NULL)),
    CHECK (coin_txid IS NULL OR kind IN (0, 1)),
    CHECK (archived_at IS NULL OR (kind = 0 AND coin_txid IS NOT NULL)),
    CHECK (kind <> 6 OR block IS NULL),
    CHECK ((position IS NULL) = (total IS NULL)) -- Pending legs: neither.
);
CREATE INDEX leg_address ON leg (address, asset);
-- An index's last leg at or below a position: one seek.
CREATE INDEX leg_wallet_index ON leg (wallet_index_id, position, seq);
CREATE UNIQUE INDEX leg_seq ON leg (wallet_index_id, seq);
CREATE INDEX leg_transaction ON leg ("transaction");
-- A block's legs: what deleting it deletes.
CREATE INDEX leg_block ON leg (block);
CREATE INDEX leg_coin ON leg (coin_txid, coin_index) WHERE coin_txid IS NOT NULL;

-- A transaction with its legs (`legs`, JSON as the `Leg` model serializes them, by
-- leg_index): what every transaction read answers.
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

-- Every address the server knows on a network, keyed or watch-only: its
-- key's curve and bytes (both NULL: watch-only, no key kept), and its tags
-- (pipes' labels), a JSON list kept sorted, each once, each [a-z0-9-]{1,64}.
-- id: "{chain}.{network}.{address}".
CREATE TABLE wallet (
    id      TEXT NOT NULL PRIMARY KEY,
    chain   TEXT NOT NULL,
    network TEXT NOT NULL,
    address TEXT NOT NULL,
    curve   TEXT,            -- NULL: watch-only (no key kept)
    secret  BLOB,            -- NULL: watch-only
    tags    TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
    -- The caller's own reference (e.g. their customer id); not unique.
    external_id TEXT CHECK (external_id IS NULL OR length(external_id) BETWEEN 1 AND 128),
    CHECK (id = chain || '.' || network || '.' || address),
    CHECK ((curve IS NULL) = (secret IS NULL)),
    CHECK (curve IS NULL OR curve IN ('secp256k1', 'ed25519'))
);
CREATE INDEX wallet_network ON wallet (chain, network);
-- Tag reads (the tags in use, the tag selectors) scan this, not the rows.
CREATE INDEX wallet_tags ON wallet (tags);

-- One asset of one wallet being indexed; its chain, network and address are
-- the wallet's. Facts are kept for every block from birthday through
-- indexed_at, the position read through, with no gaps. NULL indexed_at:
-- nothing scanned yet.
-- requested_from: the start asked for; birthday moves past it, before the
-- first scan, to the first block the node serves.
-- state: 0 indexing, 1 paused, 2 no_data (the node no longer serves a block
-- it needs; reason says which). Only indexing indexes are scanned.
-- id: "{wallet_id}.{asset}".
CREATE TABLE wallet_index (
    id             TEXT    NOT NULL PRIMARY KEY,
    wallet_id      TEXT    NOT NULL REFERENCES wallet (id) ON DELETE CASCADE,
    asset          TEXT    NOT NULL,
    requested_from INTEGER NOT NULL,
    birthday       INTEGER NOT NULL,
    indexed_at     INTEGER,
    state          INTEGER NOT NULL,
    reason         TEXT,
    CHECK (id = wallet_id || '.' || asset),
    CHECK (state IN (0, 1, 2))
);
CREATE INDEX wallet_index_wallet ON wallet_index (wallet_id);
CREATE INDEX wallet_index_progress ON wallet_index (indexed_at);

-- One sent transaction being looked for (design 085): the blocks from
-- `broadcasted_at` (the tip it was broadcast at) through `indexed_at` (a
-- position) are read for it, moved by every block kept as a wallet index
-- is, with no gaps; NULL indexed_at: nothing read yet. `found_at`: the
-- position it is mined in (NULL until mined), no longer read once set; a
-- reorg that takes it back clears it. Whether it reverted is its
-- transaction row's own state. `expires_height`: the last block height it
-- can land at (Solana's blockhash); NULL: never. state, only what the
-- index itself does: 0 indexing, 1 paused, 2 no_data (the node no longer
-- serves a block it needs, or it was given up: `reason`). The indexer
-- never judges expiry: one past its
-- `expires_height` reads on until its broadcast job drops it. Its
-- broadcast job deletes it once done.
-- id: the transaction's, "{chain}.{network}.{txid}".
CREATE TABLE transaction_index (
    id             TEXT    NOT NULL PRIMARY KEY,
    chain          TEXT    NOT NULL,
    network        TEXT    NOT NULL,
    txid           TEXT    NOT NULL,
    broadcasted_at INTEGER NOT NULL,
    expires_height INTEGER,
    indexed_at     INTEGER,
    found_at       INTEGER,
    state          INTEGER NOT NULL,
    reason         TEXT,
    CHECK (id = chain || '.' || network || '.' || txid),
    CHECK (state IN (0, 1, 2))
);
CREATE INDEX transaction_index_network ON transaction_index (chain, network);
CREATE INDEX transaction_index_progress ON transaction_index (indexed_at);

-- A wallet index with its wallet's network, address and tags, whether its
-- key is kept, the position it is indexed through and how far that is
-- behind its network's indexer tip, and the indexer's confirmed position
-- (`finality_at`). Its balances are NULL before its first block (NULL
-- `indexed_at`: a chain that reads balances has not settled it yet).
-- Its confirmed balances, NULL while that is unknown
-- (no `network_index` confirmed position): `balance_confirmed`,
-- `balance_confirmed_received`, `balance_confirmed_sent`, the totals of its
-- last mined leg at or below `finality_at` (`c`; '0': none), and
-- `balance_confirmed_received_usd`, `balance_confirmed_sent_usd`, that
-- leg's `total_received_usd` and `total_sent_usd`;
-- `balance_confirmed_spendable`, `balance_confirmed` less its outgoing legs
-- (spent, burned, fee) not yet in it: pending (no block), or mined in a
-- block above `finality_at`. `balance_pending`: what it will hold once
-- every pending transaction lands: the total of its newest mined leg,
-- any block (`n`; '0': none), with its pending legs (what this server sent,
-- not mined yet), in and out. Each
-- leg is one seek of `leg_wallet_index`. `SUM_DECIMAL` is the server's:
-- read only through its connections.
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


-- The hashinbox server's own tables.
--
-- Wallets are the SDK's `wallet` rows, their tags included; the server keeps
-- no wallet table of its own. It may read the SDK's tables; it writes only
-- its own, and a wallet's tags.

-- One row per network the server knows: what does not change while it
-- runs. `chain_id`, the chain's own id of it (Ethereum chain id, Solana
-- genesis hash; NULL where the name is enough). `block_time_ms`, the chain's
-- block (slot) time. `confirmations`, positions below the tip before a block
-- counts as confirmed. `max_depth`, how many positions below the tip a
-- wallet's start may be (NULL: no limit); at least `confirmations`; NULL on
-- a chain that reads balances (its line is its final block).
-- `explorer_address` / `explorer_transaction`, page URLs with `{address}` /
-- `{tx}` filled in (NULL: none). `settings`, the chain's own knobs as a JSON
-- object (fee limits, poll interval, ...); an absent key is the chain's
-- default. Seeded by `seed.sql`.
CREATE TABLE network (
    chain                TEXT    NOT NULL,
    network              TEXT    NOT NULL,
    chain_id             TEXT,
    testnet              INTEGER NOT NULL,
    enabled              INTEGER NOT NULL,
    block_time_ms        INTEGER NOT NULL,
    confirmations        INTEGER NOT NULL,
    max_depth         INTEGER,
    explorer_address     TEXT,
    explorer_transaction TEXT,
    settings             TEXT    NOT NULL DEFAULT '{}',
    PRIMARY KEY (chain, network)
);

-- Every asset of a network: its native currency (`kind` 'native', `asset`
-- 'native', no contract) and its tokens (`kind` 'contract', `asset` the
-- contract or mint). `display_decimals`: fractional digits a view shows
-- (NULL: all).
CREATE TABLE network_asset (
    chain            TEXT    NOT NULL,
    network          TEXT    NOT NULL,
    asset            TEXT    NOT NULL,
    kind             TEXT    NOT NULL,
    contract         TEXT,
    ticker           TEXT    NOT NULL,
    decimals         INTEGER NOT NULL,
    display_decimals INTEGER,
    PRIMARY KEY (chain, network, asset),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network)
);

-- A source of market data, and later of swaps: one row per implementation
-- (`usecase::market`: binance, kraken, coinbase, bitstamp, coingecko),
-- seeded (`seed.sql`). feed: the market job asks it for quotes
-- (market_candle_1h); the user can turn one off. updated_at: Unix ms it was
-- last changed.
CREATE TABLE market (
    name       TEXT    PRIMARY KEY,
    feed       INTEGER NOT NULL DEFAULT 1 CHECK (feed IN (0, 1)),
    updated_at INTEGER NOT NULL
);

-- One market's hour of one asset's USD price (the market job, asking
-- every 10 s): asset, a `network_asset` ticker (`BTC`); start, the hour,
-- Unix seconds, hour-aligned; open, high, low, close, exact decimal USD:
-- the hour's first quote, its extremes and its latest; updated_at, Unix ms
-- its last quote answered. What an asset was worth in an hour is the
-- median close of its candles. A USDT quote counts as USD 1:1 for a
-- non-stable asset; a stablecoin is quoted in USD only.
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
-- Every market's candles of an hour (and asset): one seek.
CREATE INDEX market_candle_1h_start ON market_candle_1h (start, asset);

-- What each network's indexer last wrote, written the first time its tip is
-- known (no row: no tip yet; the server indexes every `enabled` network):
-- `tip`, the node's newest block; `confirmed`, `tip - confirmations`
-- (saturating), written with it; `indexed`, the last kept block (NULL: none
-- yet); `checked_at`, Unix ms of the last step whose tip moved. `blocks`
-- (kept), `reorgs` and `failures` count since the row was made. Why a step
-- failed is the index job's `q.reason`.
CREATE TABLE network_index (
    chain      TEXT    NOT NULL,
    network    TEXT    NOT NULL,
    tip        INTEGER NOT NULL,
    confirmed  INTEGER NOT NULL,
    indexed    INTEGER,
    checked_at INTEGER,
    blocks     INTEGER NOT NULL DEFAULT 0,
    reorgs     INTEGER NOT NULL DEFAULT 0,
    failures   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (chain, network),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network)
);

-- One RPC node of a network: its URL (keys, headers and `rpc.` settings
-- written in it), failing over by `position`, lowest first. Seeded by
-- `seed.sql`, changed by verified edits. `id` is an integer, unlike the
-- natural TEXT keys above: a URL holds dots and secrets.
CREATE TABLE network_endpoint (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    chain    TEXT    NOT NULL,
    network  TEXT    NOT NULL,
    url      TEXT    NOT NULL,
    position INTEGER NOT NULL,
    UNIQUE (chain, network, url),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network)
);

-- `rpc::Router` of a network: each method with rows here is spread round
-- robin over those endpoints; every other method goes to the cluster of all
-- the network's endpoints.
CREATE TABLE network_endpoint_route (
    endpoint_id INTEGER NOT NULL REFERENCES network_endpoint (id) ON DELETE CASCADE,
    method      TEXT    NOT NULL,
    PRIMARY KEY (endpoint_id, method)
) WITHOUT ROWID;

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
    -- JSON: what its last round saw (`trigger::LastRound`), acted or not;
    -- NULL: no round yet.
    last_round TEXT             CHECK (json_valid(last_round)),
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
    stats      TEXT    NOT NULL CHECK (json_valid(stats)), -- JSON: what its round saw (`trigger::Stats`).
    CHECK (id = "trigger" || '.' || started_at)
);
CREATE INDEX trigger_run_latest ON trigger_run ("trigger", started_at);

-- Durable job queue (packages/tasks `Store`, the server's usecase/tasks.rs).

-- One row per job for its whole life; finished rows are the record. Queue
-- fields are the queue's; `input` and `state` are the job's worker's.
CREATE TABLE q (
    id         INTEGER PRIMARY KEY AUTOINCREMENT, -- Identity and place in line.
    topic      TEXT    NOT NULL, -- What it locks: `a.b.c`; a prefix conflicts.
    kind       TEXT    NOT NULL, -- Worker selector.
    status     INTEGER NOT NULL, -- 0 pending, 1 running, 2 done, 3 failed, 4 skipped, 5 idle.
    run_at     INTEGER NOT NULL, -- ms since epoch; not started before; running: lease end.
    deadline   INTEGER,          -- ms since epoch; not started after.
    attempts   INTEGER NOT NULL, -- Executions started since it was last fresh.
    reason     TEXT,             -- Why it failed, or why it last retried.
    input      TEXT    NOT NULL, -- JSON: what to do; never changed.
    state      TEXT,             -- JSON: what the worker has done so far.
    created_at INTEGER NOT NULL, -- ms.
    updated_at INTEGER NOT NULL, -- ms; last written.
    CHECK (status IN (0, 1, 2, 3, 4, 5)),
    CHECK (attempts >= 0),
    CHECK (json_valid(input)),
    CHECK (state IS NULL OR json_valid(state))
);
-- What a claim weighs a job against: the unfinished jobs, by topic.
CREATE INDEX q_unfinished ON q (topic) WHERE status IN (0, 1);
-- A kind's one unfinished job per topic.
CREATE INDEX q_kind ON q (kind, topic) WHERE status IN (0, 1);
-- A kind's due jobs.
CREATE INDEX q_due ON q (kind, run_at) WHERE status IN (0, 1);

-- Each trigger run, with what its round saw (`stats`), and each send job it
-- queued, as the job is now: one row per job in the order queued
-- (`position`), its `job`, `status` and `reason` NULL when the job is no
-- longer kept; a run that queued none has one row of NULLs.
CREATE VIEW trigger_run_with_jobs AS
SELECT
    run.id AS run,
    run."trigger",
    run.started_at,
    run.stats,
    queued.key AS position,
    job.id AS job,
    job.status,
    job.reason
FROM trigger_run run
LEFT JOIN json_each(run.jobs) queued
LEFT JOIN q job ON job.id = CAST(queued.value AS INTEGER);

-- A person or team owner (design 096): the widest scopes anything they
-- issue may have. Times are Unix ms; a session's `iat` is seconds, and one
-- issued in the second of `invalidated_at` or before is refused.
CREATE TABLE "user" (
    id             TEXT    PRIMARY KEY,         -- uuid v7; its client id is `user/{id}`.
    email          TEXT    NOT NULL UNIQUE,
    name           TEXT    NOT NULL,
    password_hash  TEXT,                        -- argon2id (PHC text); NULL: no dashboard login.
    scopes         TEXT    NOT NULL CHECK (json_valid(scopes)), -- JSON, `Scopes::pack` form.
    session_secret BLOB    NOT NULL,            -- 32 random bytes: signs the user's sessions (HS256).
    created_at     INTEGER NOT NULL,
    invalidated_at INTEGER,                     -- Every session issued at or before it: 401.
    disabled_at    INTEGER                      -- Disabled: every key and session refused.
);

-- A key a user issued: its public client id, its secret (`his/…`, kept
-- readable: it checks the HS256 sessions the key signs) and the secret's
-- hash (bearer lookup).
CREATE TABLE user_key (
    id             TEXT    PRIMARY KEY,         -- `hip/…`: the key's public client id.
    user_id        TEXT    NOT NULL REFERENCES "user" (id) ON DELETE CASCADE,
    name           TEXT    NOT NULL,            -- "shop backend".
    secret_hash    BLOB    NOT NULL UNIQUE,     -- SHA-256 of `his/…`: bearer lookup.
    secret         TEXT    NOT NULL,            -- `his/…`: the HS256 key of the key's sessions.
    scopes         TEXT    NOT NULL CHECK (json_valid(scopes)), -- JSON, packed; covered by the user's.
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER,
    invalidated_at INTEGER,                     -- Every session it issued at or before it: 401.
    revoked_at     INTEGER,                     -- The key itself refused, its sessions too.
    last_used_at   INTEGER
);
CREATE INDEX user_key_user ON user_key (user_id);

-- A user's TOTP (RFC 6238), on once confirmed.
CREATE TABLE user_2fa (
    user_id      TEXT    PRIMARY KEY REFERENCES "user" (id) ON DELETE CASCADE,
    secret       BLOB    NOT NULL,              -- The TOTP secret, kept plain.
    confirmed_at INTEGER,                       -- NULL until the first valid code.
    last_step    INTEGER,                       -- Step of the last code used: no replay.
    created_at   INTEGER NOT NULL
);

-- The instance's settings (`models/setting.rs` declares every key): one
-- row per key written; a key with no row is its default.
CREATE TABLE setting (
    key        TEXT    PRIMARY KEY,
    value      TEXT    NOT NULL,                -- JSON of the key's type.
    public     INTEGER NOT NULL,                -- 1: may be sent in any response, to anyone.
    updated_at INTEGER NOT NULL                 -- Unix ms.
);
