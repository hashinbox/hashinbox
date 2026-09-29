# ADR-0028: One chain-neutral address

## Status

Accepted. Supersedes the byte-address and encoding-tag parts of ADR-0001;
ADR-0001's rule that Solana text is canonical plain Base58 stands.

## Date

2026-09-22

## Context

An address had four representations (a retired design note (011)): the typed native
address of each chain, a byte `base::Address` whose bytes meant something
different per chain (20 raw bytes, 32 raw bytes, or UTF-8 text),
`CanonicalAddress { network, value }` that indexing stored, and
`wallets::AddressText { encoding, text }` at the wallet boundary. Wallet
registry rows came from `AddressText.text` and index rows from `Display`;
they matched only by convention. The encoding tag carried nothing the text
and network did not already imply, and it made a correct address fail when
the caller named the wrong tag.

## Decision

- One chain-neutral address, `base::Address { network, value }`, used
  everywhere outside chain crates: index, registry, filters, wallets, sends,
  and the API. `value` is always the chain's canonical text, so string
  equality is address equality. Ethereum's canonical text is lowercase `0x`
  hex (unchanged); EIP-55 is an input format only.
- Inside a chain crate only the typed native `Address` is used. The two
  convert only in the chain's `identity.rs`: `Address::canonical(&network)`
  and a strict `TryFrom<&base::Address>` (this chain, a known network, exact
  canonical text).
- User text is parsed only by the wallet's provider,
  `wallets::AddressParser::parse_address(&str) -> base::Address` (a `Provider`
  supertrait), reached through
  `Wallets::parse_address`: Ethereum any case with the EIP-55 checksum checked
  on mixed case, Bitcoin any case of the provider's network, Solana canonical
  Base58.
- `wallets::Addresser::address()` returns the wallet's `&base::Address`,
  computed once when the wallet is built. `WalletInfo` carries only that
  address; its network is the scope.
- Deleted: the byte `base::Address`, `AddressValidator`, `AddressError`,
  `AddressErrorKind`, `CanonicalAddress`, `AddressText`, `AddressEncoding`,
  `AddressFormat`, and the address parameter of `base::KeyPair`.
  `scripts/check-chains.sh` rejects the retired names in code.
- HTTP: `destination` is a plain string. The legacy object
  `{encoding, text}` stays accepted and deprecated until the next major
  version; its `encoding` is ignored. Responses are unchanged.

## Consequences

- No stored string changes, so there is no migration. docs/INDEXING.md lists
  a one-off query proving no stored Ethereum address has uppercase.
- A mixed-case Ethereum address with a wrong checksum is now rejected instead
  of silently used.
- A legacy API request with a mismatched `encoding` for a valid address now
  succeeds instead of failing with `422`.
- Bitcoin testnets that share prefixes are told apart by the address's
  network, which the old encoding tag never carried.
