-- The networks a new file starts with (`Db::migrate` runs it after
-- `init.sql`, in the same write): their facts, assets and public endpoints.
-- Edit the rows to add, tune or disable a network; `settings` holds a
-- chain's own knobs (empty: every default). Ethereum fetches 10 blocks a
-- step (`batch`), which public nodes serve without rate-limiting. Ethereum
-- and Solana read balances: no `max_depth`; Ethereum's line is the block
-- after its confirmed one (64 confirmations: about two epochs, where
-- `finalized` sits), Solana's its tip less `read_depth` (a setting, default
-- 1000; at least `confirmations`).

INSERT INTO network (chain, network, chain_id, testnet, enabled, block_time_ms,
    confirmations, max_depth, explorer_address, explorer_transaction, settings)
VALUES
    ('ethereum', 'sepolia', '11155111', 1, 1, 12000, 64, NULL,
        'https://sepolia.etherscan.io/address/{address}',
        'https://sepolia.etherscan.io/tx/{tx}', '{"batch": 10}'),
    ('ethereum', 'mainnet', '1', 0, 1, 12000, 64, NULL,
        'https://etherscan.io/address/{address}',
        'https://etherscan.io/tx/{tx}', '{"batch": 10}'),
    ('solana', 'devnet', 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG', 1, 1, 400, 32, NULL,
        'https://explorer.solana.com/address/{address}?cluster=devnet',
        'https://explorer.solana.com/tx/{tx}?cluster=devnet', '{"read_depth": 150}'),
    ('solana', 'mainnet', '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d', 0, 1, 400, 32, NULL,
        'https://explorer.solana.com/address/{address}',
        'https://explorer.solana.com/tx/{tx}', '{}');

-- Circle's USDC (test USDC on the test networks), Tether's USDT.
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
    ('solana', 'devnet', 'native', 'native', NULL, 'SOL', 9, 4),
    ('solana', 'devnet', '4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU', 'contract',
        '4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU', 'USDC', 6, 2),
    ('solana', 'mainnet', 'native', 'native', NULL, 'SOL', 9, 4),
    ('solana', 'mainnet', 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', 'contract',
        'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', 'USDC', 6, 2),
    ('solana', 'mainnet', 'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB', 'contract',
        'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB', 'USDT', 6, 2);

INSERT INTO network_endpoint (chain, network, url, position)
VALUES
    ('ethereum', 'sepolia', 'https://ethereum-sepolia-rpc.publicnode.com', 0),
    ('ethereum', 'mainnet', 'https://ethereum-rpc.publicnode.com', 0),
    ('solana', 'devnet', 'https://api.devnet.solana.com', 0),
    ('solana', 'mainnet', 'https://api.mainnet-beta.solana.com', 0);
