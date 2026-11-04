# Network asset icons

`svg/color/<ticker>.svg` of [cryptocurrency-icons](https://github.com/spothq/cryptocurrency-icons)
0.18.1 (CC0-1.0, public domain), copied unchanged but for the trailing newline.
One file per network asset: `<chain>.svg` for a native coin,
`<chain>_<asset>.svg` for a token; the chain's short name and the asset's
ticker, in lowercase. A token shows its own logo on every chain (the set has
no chain badges). The server embeds them and serves them under
`/assets/crypto/`. To add one, copy its file from the same package and variant
and list it in `apps/server/src/ui.rs`.

- `btc.svg`, `eth.svg`, `sol.svg`
- `eth_usdc.svg`, `sol_usdc.svg` (usdc)
- `eth_usdt.svg`, `sol_usdt.svg` (usdt)
