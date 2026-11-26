<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hashinbox-logo-dark.svg">
    <img src="docs/assets/hashinbox-logo-light.svg" alt="hashinbox" width="360">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/hashinbox/hashinbox/actions/workflows/check.yml"><img alt="check" src="https://github.com/hashinbox/hashinbox/actions/workflows/check.yml/badge.svg?branch=main"></a>
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

Open <http://127.0.0.1:3000/ui/>. The first start creates `.data/hashinbox.db`, and the
dashboard asks for the first account.

To skip that step, preset the first account with `HASHINBOX_CONFIG`, as inline JSON or the path
of a JSON file:

```bash
HASHINBOX_CONFIG='{"root":{"email":"ops@shop.io","name":"Ops","password":"change-me"}}' \
  cargo run --locked --release -p hashinbox -- serve
```

More options: [apps/server](apps/server/README.md).
