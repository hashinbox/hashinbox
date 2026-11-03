-- The SDK schema: what `base`'s stores keep (BlockStore, TransactionStore,
-- InputStore, WalletStore, WalletIndexStore), in the one SQLite file. Applied
-- once to a new file by the caller (`payment-server migrate`); no migrations.
-- Enable foreign_keys on every connection.
--
-- A network is (chain, network). Positions and heights are SQLite integers;
-- amounts are exact decimal TEXT, never summed in SQL.
--
-- Blocks are kept up to the produced tip. A block is live until the indexer
-- archives it (a reorg): `archived_at` is set, nothing is deleted, and every
-- read joins live blocks only, so archived facts leave balances, history and
-- unspent outputs. Confirmation is a read (position <= the chain's confirmed
-- tip); nothing about it is stored.
--
-- Mined facts (transactions and legs, rewards included) are kept only for
-- (address, asset) pairs with a wallet_index the block applies to
-- (BlockStore::insert).
-- A coin is a `received` leg with a coin; it is spent while a `spent` leg in
-- a live block names it, so archiving the spending block unspends it.

-- Keys: every table's `id` is TEXT, its natural key joined with dots (dots
-- are URL-safe), and every foreign key references such an id. No part holds
-- a dot: chain and network slugs, lowercase hex hashes, chain address text,
-- asset ids and txids; the server refuses a part with a dot (or an empty one)
-- when it builds an id (`models::key`).

-- id: "{chain}.{network}.{hash}". A block whose hash comes back after an
-- archive is revived (archived_at cleared), never inserted twice.
CREATE TABLE block (
    id              TEXT    NOT NULL PRIMARY KEY,
    chain           TEXT    NOT NULL,
    network         TEXT    NOT NULL,
    position        INTEGER NOT NULL, -- Native coordinate (height or slot).
    height          INTEGER NOT NULL, -- Produced-block count.
    hash            BLOB    NOT NULL,
    -- The parent block's id once it is stored: NULL for the first stored
    -- block of a run, set in the write that later stores the parent.
    parent          TEXT    REFERENCES block (id),
    -- The parent as reported; the position is kept because the header is
    -- read back whole while `parent` is still NULL.
    parent_position INTEGER,          -- NULL exactly for genesis.
    parent_hash     BLOB,
    timestamp       INTEGER,          -- Unix seconds, when reported.
    archived_at     INTEGER,          -- Unix ms; NULL while live.
    CHECK (id = chain || '.' || network || '.' || lower(hex(hash))),
    CHECK ((parent_position IS NULL) = (parent_hash IS NULL)),
    CHECK (parent IS NULL
        OR parent = chain || '.' || network || '.' || lower(hex(parent_hash)))
);
-- At most one live block per position.
CREATE UNIQUE INDEX block_live ON block (chain, network, position)
    WHERE archived_at IS NULL;
CREATE INDEX block_parent_hash ON block (chain, network, parent_hash)
    WHERE parent IS NULL;

-- A transaction, once per txid of a network: pending (block NULL, kept by
-- TransactionStore::insert, a no-op when the txid is kept) or mined. Mining
-- (BlockStore::insert) upserts it: block, order, state, reason and bytes
-- become the mined ones and its legs are replaced by the mined legs. One
-- whose block is archived and not re-mined is no longer shown: a reorg
-- cannot make funds appear.
-- id: "{chain}.{network}.{txid}".
CREATE TABLE "transaction" (
    id      TEXT    NOT NULL PRIMARY KEY,
    block   TEXT    REFERENCES block (id), -- NULL: pending, not mined.
    chain   TEXT    NOT NULL,
    network TEXT    NOT NULL,
    txid    TEXT    NOT NULL,          -- Chain-native id.
    "order" INTEGER,                   -- Position in the block.
    state   INTEGER NOT NULL,          -- 0 pending, 1 succeeded, 2 reverted.
    reason  TEXT,                      -- JSON Reason; reverted only.
    bytes   BLOB    NOT NULL,
    sent_at    INTEGER,                -- Unix ms recorded as pending; NULL once mined.
    expires_at INTEGER,                -- Unix ms it stops holding funds; NULL once mined.
    CHECK (id = chain || '.' || network || '.' || txid),
    CHECK (state IN (0, 1, 2)),
    CHECK (state = 2 OR reason IS NULL),
    CHECK (block IS NULL OR (sent_at IS NULL AND expires_at IS NULL))
);
CREATE INDEX transaction_block ON "transaction" (block);

-- One kept leg of a transaction, or a block reward (value a block moved
-- with no transaction; signed). A pending transaction's legs have no block;
-- only legs of a live block count (balances, unspent coins). Address and
-- asset are of the transaction's or block's network.
-- A mined leg is kept for the wallet index it applies to (and goes with
-- it), and carries its block's position, its `seq` and `total`: the
-- index's balance right after it, the previous live mined leg's total plus
-- its signed amount (design 072). `seq` is 1 + the highest the index has
-- ever kept (archived legs included, so never reused, and unique: a second
-- writer of the same step is refused); within a block it follows
-- transaction order, then leg index, rewards after the transactions. An
-- index's legs order by (position, seq); its balance at a position is the
-- total of its last live leg at or below it (none: 0). Written in that
-- order as blocks are kept, in one write transaction; a block kept again
-- (revived, or re-applied to an index) has its legs written again from the
-- live chain below it. A pending leg has none of these: it is read by
-- address until mined.
-- id: "{transaction}.{leg_index}", or "{block}.reward.{leg_index}".
CREATE TABLE leg (
    id            TEXT    NOT NULL PRIMARY KEY,
    block         TEXT    REFERENCES block (id), -- NULL: of a pending transaction.
    "transaction" TEXT    REFERENCES "transaction" (id), -- NULL: a reward.
    leg_index     INTEGER NOT NULL, -- In its transaction, or the block's rewards.
    kind          INTEGER NOT NULL, -- 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward.
    address       TEXT    NOT NULL,
    asset         TEXT    NOT NULL,
    amount        TEXT    NOT NULL, -- Exact atomic decimal.
    coin_txid     TEXT,             -- The UTXO created (received) or consumed (spent).
    coin_index    INTEGER,
    wallet_index_id TEXT NOT NULL REFERENCES wallet_index (id) ON DELETE CASCADE,
    position      INTEGER,          -- The block's; NULL while pending.
    seq           INTEGER,          -- Per index, never reused; NULL while pending.
    total         TEXT,             -- Exact decimal, signed; NULL while pending.
    CHECK (block IS NOT NULL OR "transaction" IS NOT NULL),
    CHECK (kind BETWEEN 0 AND 5),
    CHECK (("transaction" IS NULL) = (kind = 5)),
    CHECK (id = COALESCE("transaction", block || '.reward') || '.' || leg_index),
    CHECK ((coin_txid IS NULL) = (coin_index IS NULL)),
    CHECK (coin_txid IS NULL OR kind IN (0, 1)),
    CHECK ((block IS NULL) = (total IS NULL)),
    CHECK ((block IS NULL) = (position IS NULL)
        AND (block IS NULL) = (seq IS NULL))
);
CREATE INDEX leg_address ON leg (address, asset);
-- An index's last live leg at or below a position: one seek.
CREATE INDEX leg_wallet_index ON leg (wallet_index_id, position, seq);
CREATE UNIQUE INDEX leg_seq ON leg (wallet_index_id, seq);
CREATE INDEX leg_transaction ON leg ("transaction");
CREATE INDEX leg_coin ON leg (coin_txid, coin_index) WHERE coin_txid IS NOT NULL;

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
    CHECK (id = chain || '.' || network || '.' || address),
    CHECK ((curve IS NULL) = (secret IS NULL)),
    CHECK (curve IS NULL OR curve IN ('secp256k1', 'ed25519'))
);
CREATE INDEX wallet_network ON wallet (chain, network);
-- Tag reads (the tags in use, the tag selectors) scan this, not the rows.
CREATE INDEX wallet_tags ON wallet (tags);

-- One asset of one wallet being indexed; its chain, network and address are
-- the wallet's. Facts are kept for every live block from birthday through
-- indexed_at, with no gaps. NULL indexed_at: nothing scanned yet.
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
    indexed_at     TEXT    REFERENCES block (id),
    state          INTEGER NOT NULL,
    reason         TEXT,
    CHECK (id = wallet_id || '.' || asset),
    CHECK (state IN (0, 1, 2))
);
CREATE INDEX wallet_index_wallet ON wallet_index (wallet_id);
CREATE INDEX wallet_index_progress ON wallet_index (indexed_at);

