# Examples

Runnable programs that use only the public SDK API against local development
nodes. One directory is one scenario: a `main.rs` and the `config.toml` that
names its chains.

| Scenario | What it shows |
| --- | --- |
| [wallet](wallet/main.rs) | Create a wallet and import a key for each asset. No funds move. |
| [transfer](transfer/main.rs) | Fund a sender, send, then read balance and history. |
| [indexing](indexing/main.rs) | Run the sync loop and read each network's checkpoint. |
| [watch_only](watch_only/main.rs) | Observe an address that has no key behind it. |
| [all_chains](all_chains/main.rs) | The whole system in one file: one index, one wallet map, one send per chain. |
| [custom_store](custom_store/main.rs) | Supply your own repository and drive `sync` yourself. |

## Run one

Every scenario uses all three nodes. Start them, then run one:

```bash
bitcoind -regtest -txindex -rpcuser=bitcoin -rpcpassword=bitcoin -rpcport=18443
anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0
solana-test-validator --reset

cargo run -p payment-examples --example wallet
```

| Node | Version | Port |
| --- | --- | --- |
| Bitcoin Core, regtest, unpruned, `-txindex` | 31 | 18443 |
| Anvil | any | 8545 |
| Solana test validator | any | 8899 |

Index files are written to `.index-data/` in the working directory.

## Notes

- A send on a fresh Bitcoin regtest chain can fail with `FeeUnavailable`.
  Bitcoin Core estimates fees only after it has seen enough transactions, and
  the SDK has no fixed-rate override.
- The Solana `genesis_hash` in each `config.toml` is a placeholder.
  `solana-test-validator --reset` creates a new cluster each time, so each
  `main.rs` asks the node for the hash and substitutes it. A real deployment
  writes the hash it expects.
- Never point an example at a funded key or a public endpoint. Funding uses the
  local node's own RPC (`generatetoaddress`, Anvil's development key,
  `requestAirdrop`).
