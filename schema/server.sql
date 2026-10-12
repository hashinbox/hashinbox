-- The payment server's own table, executed with schema/sdk.sql on the same
-- new file by the caller (`payment-server migrate`). No migrations.
--
-- Wallets are the SDK's `wallet` rows; the server keeps no wallet table of
-- its own. It may read the SDK's tables; it writes only this one.

-- One row per network: its RPC endpoint documents in failover order, as one
-- JSON list. Seeded from the configuration file once, replaced by verified
-- edits.
CREATE TABLE rpc_endpoints (
    chain   TEXT NOT NULL,
    network TEXT NOT NULL,
    nodes   TEXT NOT NULL,
    PRIMARY KEY (chain, network)
) WITHOUT ROWID;
