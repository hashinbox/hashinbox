//! Bitcoin Core's text forms of block hashes: reversed hexadecimal.

use base::BlockHash;
use bitcoin::{BlockHash as NativeBlockHash, hashes::Hash};

use crate::native::Error;

/// The block hash `text` names in Bitcoin Core's display order; `None` for
/// anything but 64 hexadecimal digits.
pub(crate) fn parse_block_hash(text: &str) -> Option<BlockHash> {
    text.parse::<NativeBlockHash>()
        .ok()
        .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
}

/// `hash` in Bitcoin Core's display order.
pub(crate) fn format_block_hash(hash: &BlockHash) -> Result<String, Error> {
    let bytes: [u8; 32] = hash
        .0
        .as_slice()
        .try_into()
        .map_err(|_| Error::invalid_block("Bitcoin block hash must be 32 bytes"))?;
    Ok(NativeBlockHash::from_byte_array(bytes).to_string())
}
