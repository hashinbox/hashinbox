//! Generate an Ethereum key and address locally, without a node or database.
//! The secret remains in memory and is dropped when the example exits.

use base::Addresser;
use chain_ethereum::KeyPair;
use wallets::AddressFormat;

fn main() -> Result<(), wallets::Error> {
    let key = KeyPair::generate()?;
    let address = key.address_text(&key.address())?;
    println!("Wallet address: {}", address.text);
    Ok(())
}
