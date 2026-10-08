//! The configured asset tags every route, configuration section and registry
//! row selects an asset by, and the wallet factories keyed by them.

use std::{error, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// The wallet factories the routes serve, keyed by configured asset tags. The
/// composition root owns this type; the HTTP layer only borrows it.
pub(crate) type Wallets = wallets::Wallets<AssetTag>;

/// The user-facing key of one registered asset: `btc`, `eth`, `usdc`,
/// `eth-sepolia`.
///
/// One tag names one wallet factory (one asset in one network) at the
/// composition root; everything after composition selects the asset by its
/// tag, never by a chain enum. Text is `[a-z0-9][a-z0-9-]{0,31}`.
// linter:disable rust/redundant-module-prefix -- Config and the registry say `AssetTag`, not `Tag`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct AssetTag(String);

impl AssetTag {
    /// Longest accepted tag, in bytes.
    const MAX_LEN: usize = 32;

    /// Validates owned tag text.
    ///
    /// # Errors
    ///
    /// [`TagError`] when `tag` is not `[a-z0-9][a-z0-9-]{0,31}`.
    fn new(tag: impl Into<String>) -> Result<Self, TagError> {
        let tag = tag.into();
        if Self::accepts(tag.as_bytes()) {
            Ok(Self(tag))
        } else {
            Err(TagError)
        }
    }

    const fn accepts(bytes: &[u8]) -> bool {
        if bytes.is_empty() || bytes.len() > Self::MAX_LEN || bytes[0] == b'-' {
            return false;
        }
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-') {
                return false;
            }
            index += 1;
        }
        true
    }
}

impl fmt::Display for AssetTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AssetTag {
    type Err = TagError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl TryFrom<String> for AssetTag {
    type Error = TagError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<AssetTag> for String {
    fn from(tag: AssetTag) -> Self {
        tag.0
    }
}

/// Text that is not an [`AssetTag`].
// linter:disable rust/empty-struct -- The failure has one cause; its message is the whole value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TagError;

impl fmt::Display for TagError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset tag must match [a-z0-9][a-z0-9-]{0,31}")
    }
}

impl error::Error for TagError {}
