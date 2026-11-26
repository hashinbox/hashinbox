-- Design 099: BNB Smart Chain. Brings a database made by the previous
-- `apps/server/seed.sql` (`098-migrate-1.sql` applied) to the new one. Run
-- once, with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/099-migrate-1.sql
--
-- Adds the `bnb` networks (mainnet, chain 56; testnet, chain 97), enabled,
-- their assets (BNB; USDT and USDC at 18 decimals) and their public nodes,
-- as `seed.sql` now seeds them. No table changes.

BEGIN;

INSERT INTO network (chain, network, chain_id, testnet, enabled, block_time_ms,
    confirmations, max_depth, explorer_address, explorer_transaction, settings)
VALUES
    ('bnb', 'testnet', '97', 1, 1, 450, 15, NULL,
        'https://testnet.bscscan.com/address/{address}',
        'https://testnet.bscscan.com/tx/{tx}', '{"batch": 10}'),
    ('bnb', 'mainnet', '56', 0, 1, 450, 15, NULL,
        'https://bscscan.com/address/{address}',
        'https://bscscan.com/tx/{tx}', '{"batch": 10}');

INSERT INTO network_asset (chain, network, asset, kind, contract, ticker, decimals,
    display_decimals)
VALUES
    ('bnb', 'testnet', 'native', 'native', NULL, 'BNB', 18, 8),
    ('bnb', 'testnet', '0x337610d27c682e347c9cd60bd4b3b107c9d34ddd', 'contract',
        '0x337610d27c682e347c9cd60bd4b3b107c9d34ddd', 'USDT', 18, 2),
    ('bnb', 'testnet', '0x64544969ed7ebf5f083679233325356ebe738930', 'contract',
        '0x64544969ed7ebf5f083679233325356ebe738930', 'USDC', 18, 2),
    ('bnb', 'mainnet', 'native', 'native', NULL, 'BNB', 18, 8),
    ('bnb', 'mainnet', '0x55d398326f99059ff775485246999027b3197955', 'contract',
        '0x55d398326f99059ff775485246999027b3197955', 'USDT', 18, 2),
    ('bnb', 'mainnet', '0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d', 'contract',
        '0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d', 'USDC', 18, 2);

INSERT INTO network_endpoint (chain, network, url, position)
VALUES
    ('bnb', 'testnet', 'https://bsc-testnet-rpc.publicnode.com', 0),
    ('bnb', 'mainnet', 'https://bsc-rpc.publicnode.com', 0);

COMMIT;
