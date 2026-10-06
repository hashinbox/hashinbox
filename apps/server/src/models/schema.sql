-- The payment server's own table, beside indexing's in the one SQLite file.
-- Applied once, with indexing's, when the server creates its database; there
-- are no migrations: the file's identity covers this text, so a schema change
-- edits this file and needs a fresh database.
-- Wallets are indexing's `wallet` rows (`indexing::WalletRegistry`); the
-- server keeps no wallet table of its own.

-- One row per network: its RPC endpoint documents in failover order, as one
-- JSON list. Seeded from the configuration file once, replaced by verified
-- edits.
CREATE TABLE rpc_endpoints (
    chain TEXT NOT NULL,
    network TEXT NOT NULL,
    nodes TEXT NOT NULL,
    PRIMARY KEY (chain, network)
) WITHOUT ROWID;
