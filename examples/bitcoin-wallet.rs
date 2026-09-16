//! Generate a Bitcoin key and address locally, without a node or database.
//! The secret remains in memory and is dropped when the example exits.

use base::Addresser;
use chain_bitcoin::{AddressType, KeyPair, Network};
use wallets::AddressFormat;

fn main() -> Result<(), wallets::Error> {
    let key = KeyPair::generate(Network::Regtest, AddressType::SegwitV0)?;
    let address = key.address_text(&key.address())?;
    println!("Wallet address: {}", address.text);
    Ok(())
}
