-- Design 100: Tron. Brings a database made by the previous
-- `apps/server/init.sql` and `seed.sql` (`098-migrate-1.sql` applied) to
-- the new seed. Run once, with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/100-migrate-1.sql
--
-- Adds Tron's networks (mainnet, Nile), their assets (TRX, USDT; no USDC)
-- and public nodes, as `seed.sql` seeds a new file. No schema changes.

BEGIN;

INSERT INTO network (chain, network, chain_id, testnet, enabled, block_time_ms,
    confirmations, max_depth, explorer_address, explorer_transaction, settings)
VALUES
    ('tron', 'nile', '0000000000000000d698d4192c56cb6be724a558448e2684802de4d6cd8690dc',
        1, 1, 3000, 19, NULL,
        'https://nile.tronscan.org/#/address/{address}',
        'https://nile.tronscan.org/#/transaction/{tx}', '{"batch": 10}'),
    ('tron', 'mainnet', '00000000000000001ebf88508a03865c71d452e25f4d51194196a1d22b6653dc',
        0, 1, 3000, 19, NULL,
        'https://tronscan.org/#/address/{address}',
        'https://tronscan.org/#/transaction/{tx}', '{"batch": 10}');

INSERT INTO network_asset (chain, network, asset, kind, contract, ticker, decimals,
    display_decimals)
VALUES
    ('tron', 'nile', 'native', 'native', NULL, 'TRX', 6, 4),
    ('tron', 'nile', 'TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf', 'contract',
        'TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf', 'USDT', 6, 2),
    ('tron', 'mainnet', 'native', 'native', NULL, 'TRX', 6, 4),
    ('tron', 'mainnet', 'TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t', 'contract',
        'TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t', 'USDT', 6, 2);

INSERT INTO network_endpoint (chain, network, url, position)
VALUES
    ('tron', 'nile', 'https://nile.trongrid.io', 0),
    ('tron', 'mainnet', 'https://tron-rpc.publicnode.com', 0),
    ('tron', 'mainnet', 'https://api.trongrid.io', 1);

COMMIT;
