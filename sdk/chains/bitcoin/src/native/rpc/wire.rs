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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_hashes_use_display_byte_order_in_any_case() {
        let internal = BlockHash((0_u8..32).collect());
        let displayed = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";
        assert_eq!(format_block_hash(&internal).unwrap(), displayed);
        assert_eq!(parse_block_hash(displayed), Some(internal.clone()));
        assert_eq!(parse_block_hash(&displayed.to_uppercase()), Some(internal));
        for text in [
            String::new(),
            "00".repeat(31),
            "00".repeat(33),
            "gg".repeat(32),
            format!("0x{}", "00".repeat(32)),
            format!(" {}", "00".repeat(32)),
        ] {
            assert_eq!(parse_block_hash(&text), None);
        }
    }

    #[test]
    fn only_32_byte_hashes_format() {
        for length in [0, 1, 31, 33, 64] {
            let error = format_block_hash(&BlockHash(vec![0; length])).unwrap_err();
            assert_eq!(error.message, "Bitcoin block hash must be 32 bytes");
            assert!(!error.retryable);
        }
    }
}
