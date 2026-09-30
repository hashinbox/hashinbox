# base

The chain-neutral vocabulary every other SDK crate shares: networks, addresses,
assets, exact amounts, blocks and transaction identities. It also declares the
small traits a chain implements to sign, build and broadcast a transaction.

## Usage

```rust
use base::{
    Address, AssetId, ChainName, Decimal, Digest, Network, PublicKeyFormat, SignRequest,
    SignablePayload, SignatureEncoding, SignatureScheme, Signer,
};
use crypto::KeyPair;

const CHAIN: ChainName = ChainName::from_static("example");

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let network = Network::new(CHAIN, "testnet");
let address = Address::new(network.clone(), "addr1");
assert!(address.belongs_to(&network));
assert!(AssetId::native(CHAIN).is_native());

// Amounts are exact decimals, converted to atomic units without rounding.
let amount: Decimal = "0.015".parse()?;
assert_eq!(amount.to_atomic_u64(8)?, 1_500_000);

// A signer may be a local key, an HSM or another process.
let key = KeyPair::secp256k1([1_u8; 32])?;
let signed = futures_executor::block_on(key.sign(SignRequest {
    payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
    scheme: SignatureScheme::EcdsaSecp256k1,
    encoding: SignatureEncoding::Recoverable,
    public_key_format: PublicKeyFormat::Compressed,
    key_tweak: None,
}))?;
assert_eq!(signed.signature.bytes.len(), 65);
# Ok(())
# }
```

## Notes

- `Address::new` stores the text it is given and does not validate or normalize
  it. Addresses are compared as strings, so parse user input with the chain's
  wallet factory; a differently spelled address is a different address and its
  history will not be found.

`Units` resolves display precision by both network and asset identity: the same
token address on different networks may declare different decimals.
