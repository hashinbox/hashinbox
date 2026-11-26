-- The networks a new file starts with (`Db::migrate` runs it after
-- `init.sql`, in the same write): their facts, assets and public endpoints.
-- Edit the rows to add, tune or disable a network; `settings` holds a
-- chain's own knobs (empty: every default). Ethereum fetches 10 blocks a
-- step (`batch`), which public nodes serve without rate-limiting. Ethereum
-- and Solana read balances: no `max_depth`; Ethereum's line is the block
-- after its confirmed one (64 confirmations: about two epochs, where
-- `finalized` sits), Solana's its tip less `read_depth` (a setting, default
-- 1000; at least `confirmations`). BNB Smart Chain (`bnb`) is Ethereum's
-- EVM under its own name: 0.45 s blocks, 15 confirmations (fast finality
-- finalizes within a few blocks), BEP-20 stablecoins at 18 decimals. Tron
-- reads no balance: it reads every block from a watch's start (no
-- `max_depth`: no limit), confirmed 19 below the tip (about its solidified
-- block), 10 blocks a step (two requests each).

INSERT INTO network (chain, network, chain_id, testnet, enabled, block_time_ms,
    confirmations, max_depth, explorer_address, explorer_transaction, settings)
VALUES
    ('ethereum', 'sepolia', '11155111', 1, 1, 12000, 64, NULL,
        'https://sepolia.etherscan.io/address/{address}',
        'https://sepolia.etherscan.io/tx/{tx}', '{"batch": 10}'),
    ('ethereum', 'mainnet', '1', 0, 1, 12000, 64, NULL,
        'https://etherscan.io/address/{address}',
        'https://etherscan.io/tx/{tx}', '{"batch": 10}'),
    ('bnb', 'testnet', '97', 1, 1, 450, 15, NULL,
        'https://testnet.bscscan.com/address/{address}',
        'https://testnet.bscscan.com/tx/{tx}', '{"batch": 10}'),
    ('bnb', 'mainnet', '56', 0, 1, 450, 15, NULL,
        'https://bscscan.com/address/{address}',
        'https://bscscan.com/tx/{tx}', '{"batch": 10}'),
    ('solana', 'devnet', 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG', 1, 1, 400, 32, NULL,
        'https://explorer.solana.com/address/{address}?cluster=devnet',
        'https://explorer.solana.com/tx/{tx}?cluster=devnet', '{"read_depth": 150}'),
    ('solana', 'mainnet', '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d', 0, 1, 400, 32, NULL,
        'https://explorer.solana.com/address/{address}',
        'https://explorer.solana.com/tx/{tx}', '{}'),
    ('tron', 'nile', '0000000000000000d698d4192c56cb6be724a558448e2684802de4d6cd8690dc',
        1, 1, 3000, 19, NULL,
        'https://nile.tronscan.org/#/address/{address}',
        'https://nile.tronscan.org/#/transaction/{tx}', '{"batch": 10}'),
    ('tron', 'mainnet', '00000000000000001ebf88508a03865c71d452e25f4d51194196a1d22b6653dc',
        0, 1, 3000, 19, NULL,
        'https://tronscan.org/#/address/{address}',
        'https://tronscan.org/#/transaction/{tx}', '{"batch": 10}');

-- Circle's USDC (test USDC on the test networks), Tether's USDT; on BNB
-- testnet, the BNB Chain faucet's USDT and USDC; on Nile, its test USDT. No
-- USDC on Tron: Circle stopped minting it there in 2024 and ended its support.
INSERT INTO network_asset (chain, network, asset, kind, contract, ticker, decimals,
    display_decimals)
VALUES
    ('ethereum', 'sepolia', 'native', 'native', NULL, 'ETH', 18, 12),
    ('ethereum', 'sepolia', '0x1c7d4b196cb0c7b01d743fbc6116a902379c7238', 'contract',
        '0x1c7d4b196cb0c7b01d743fbc6116a902379c7238', 'USDC', 6, 2),
    ('ethereum', 'mainnet', 'native', 'native', NULL, 'ETH', 18, 12),
    ('ethereum', 'mainnet', '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'contract',
        '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'USDC', 6, 2),
    ('ethereum', 'mainnet', '0xdac17f958d2ee523a2206206994597c13d831ec7', 'contract',
        '0xdac17f958d2ee523a2206206994597c13d831ec7', 'USDT', 6, 2),
    ('bnb', 'testnet', 'native', 'native', NULL, 'BNB', 18, 8),
    ('bnb', 'testnet', '0x337610d27c682e347c9cd60bd4b3b107c9d34ddd', 'contract',
        '0x337610d27c682e347c9cd60bd4b3b107c9d34ddd', 'USDT', 18, 2),
    ('bnb', 'testnet', '0x64544969ed7ebf5f083679233325356ebe738930', 'contract',
        '0x64544969ed7ebf5f083679233325356ebe738930', 'USDC', 18, 2),
    ('bnb', 'mainnet', 'native', 'native', NULL, 'BNB', 18, 8),
    ('bnb', 'mainnet', '0x55d398326f99059ff775485246999027b3197955', 'contract',
        '0x55d398326f99059ff775485246999027b3197955', 'USDT', 18, 2),
    ('bnb', 'mainnet', '0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d', 'contract',
        '0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d', 'USDC', 18, 2),
    ('solana', 'devnet', 'native', 'native', NULL, 'SOL', 9, 4),
    ('solana', 'devnet', '4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU', 'contract',
        '4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU', 'USDC', 6, 2),
    ('solana', 'mainnet', 'native', 'native', NULL, 'SOL', 9, 4),
    ('solana', 'mainnet', 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', 'contract',
        'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', 'USDC', 6, 2),
    ('solana', 'mainnet', 'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB', 'contract',
        'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB', 'USDT', 6, 2),
    ('tron', 'nile', 'native', 'native', NULL, 'TRX', 6, 4),
    ('tron', 'nile', 'TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf', 'contract',
        'TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf', 'USDT', 6, 2),
    ('tron', 'mainnet', 'native', 'native', NULL, 'TRX', 6, 4),
    ('tron', 'mainnet', 'TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t', 'contract',
        'TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t', 'USDT', 6, 2);

INSERT INTO network_endpoint (chain, network, url, position)
VALUES
    ('ethereum', 'sepolia', 'https://ethereum-sepolia-rpc.publicnode.com', 0),
    ('ethereum', 'mainnet', 'https://ethereum-rpc.publicnode.com', 0),
    ('bnb', 'testnet', 'https://bsc-testnet-rpc.publicnode.com', 0),
    ('bnb', 'mainnet', 'https://bsc-rpc.publicnode.com', 0),
    ('solana', 'devnet', 'https://api.devnet.solana.com', 0),
    ('solana', 'mainnet', 'https://api.mainnet-beta.solana.com', 0),
    -- Tron's HTTP API. TronGrid without a key is rate-limited (HTTP 429
    -- past a few heavy calls a second): on mainnet it cannot keep up alone;
    -- add `?header.TRON-PRO-API-KEY=<key>` to its URL for real use.
    ('tron', 'nile', 'https://nile.trongrid.io', 0),
    ('tron', 'mainnet', 'https://tron-rpc.publicnode.com', 0),
    ('tron', 'mainnet', 'https://api.trongrid.io', 1);

-- Every market implementation, its price feed on.
INSERT INTO market (name, feed, updated_at)
VALUES
    ('binance', 1, 0),
    ('bitstamp', 1, 0),
    ('coinbase', 1, 0),
    ('coingecko', 1, 0),
    ('kraken', 1, 0);
