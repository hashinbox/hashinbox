<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hashinbox-logo-dark.svg">
    <img src="docs/assets/hashinbox-logo-light.svg" alt="hashinbox" width="360">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/hashinbox/hashinbox/actions/workflows/ci.yml"><img alt="ci" src="https://github.com/hashinbox/hashinbox/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <img alt="Rust 1.91+" src="https://img.shields.io/badge/rust-1.91%2B-000000?logo=rust">
  <img alt="Bitcoin" src="https://img.shields.io/badge/Bitcoin-000000?logo=bitcoin&amp;logoColor=white">
  <img alt="Ethereum" src="https://img.shields.io/badge/Ethereum-000000?logo=ethereum&amp;logoColor=white">
  <img alt="Solana" src="https://img.shields.io/badge/Solana-000000?logo=solana&amp;logoColor=white">
</p>

## Install

Needs Rust 1.91 or newer.

```bash
git clone git@github.com:hashinbox/hashinbox.git
cd hashinbox
cargo run --locked --release -p hashinbox -- serve --bind 127.0.0.1:3000
```

Open <http://127.0.0.1:3000/>. The first start creates `.data/hashinbox.db`, and the
dashboard asks for the first account.

To skip that step, preset the first account with `HASHINBOX_CONFIG`, as inline JSON or the path
of a JSON file:

```bash
HASHINBOX_CONFIG='{"root":{"email":"ops@shop.io","name":"Ops","password":"change-me"}}' \
  cargo run --locked --release -p hashinbox -- serve
```

## Docker

```bash
docker run -p 8080:8080 -v hashinbox:/data ghcr.io/hashinbox/hashinbox:latest
```

All data lives in the database `/data/hashinbox.db`, inside the volume `hashinbox`. Keep the
`-v hashinbox:/data`: without it, removing the container deletes the database, wallets and keys
included. Back up the volume, not the container.

## Options

Each flag of `serve` has an env fallback:

- `--db` (`HASHINBOX_DB`): the SQLite file, default `.data/hashinbox.db`; image: `/data/hashinbox.db`.
- `--bind` (`HASHINBOX_BIND`): default `127.0.0.1:8080`; image: `0.0.0.0:8080`, port 8080.
- `--tls-terminated-upstream` (`HASHINBOX_TLS_TERMINATED_UPSTREAM`): allows a non-loopback bind;
  the image runs with it.
- `--config` (`HASHINBOX_CONFIG`): the first-account preset; `--log-level` (`HASHINBOX_LOG_LEVEL`).
- Image: volume `/data`, uid 10001; health check `GET /health`.

## Develop

```bash
cargo run -p hashinbox -- serve       # the API on 127.0.0.1:8080
cd apps/server/ui && npx vite         # the dashboard, proxying the API
```

Open <http://localhost:5173/>.
