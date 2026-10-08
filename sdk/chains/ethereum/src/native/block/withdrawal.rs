//! Consensus-layer withdrawals: ether a block credits with no transaction.
//!
//! Post-Shanghai (EIP-4895) a validator's partial withdrawals and full exits
//! credit an execution-layer account directly from the block body. There is
//! no transaction, no receipt and no trace entry, so a fold over transactions
//! alone understates such an account by every withdrawal it ever received,
//! silently and permanently.
//!
//! The bytes are already in hand: the `eth_getBlockByNumber` result this
//! crate fetches carries `withdrawals`, and
//! [`RawBlock::withdrawals`](super::RawBlock::withdrawals) reads them out of
//! the payload it already holds. There is **no extra RPC call**, and the
//! transaction path is untouched: this reads the one key it needs and
//! allocates nothing per transaction.

use alloy_eips::eip4895::Withdrawal as Credit;
use serde::Deserialize;

use crate::native::{Address, Error, Wei};

/// The one key of the block body this module reads.
#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    withdrawals: Option<Vec<Credit>>,
}

/// One consensus-layer credit: ether a block moves to an execution address
/// with no transaction to name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Withdrawal {
    /// The consensus layer's own identifier, unique and strictly increasing
    /// across the whole chain. It is what makes [`Withdrawal::id`] stable.
    pub index: u64,
    /// The validator the credit settles. Carried because it is the only
    /// thing that explains the credit to whoever reads it.
    pub validator_index: u64,
    /// The credited execution-layer account.
    pub address: Address,
    /// The credited amount, **in wei**.
    ///
    /// The block reports Gwei, as a `uint64`. The conversion happens here,
    /// once, because a missed factor of 10^9 does not look wrong in a
    /// number and every other ether amount in this crate is wei.
    pub amount: Wei,
}

/// Wei in one Gwei: the block's withdrawal amounts are the only place this
/// crate meets that unit.
const WEI_PER_GWEI: u128 = 1_000_000_000;

impl Withdrawal {
    /// The movement identity of this credit, stable for the life of the
    /// chain because the consensus layer never reuses an index.
    #[must_use]
    pub fn id(&self) -> String {
        format!("withdrawal:{}", self.index)
    }

    /// Every credit one `eth_getBlockByNumber` result carries, in block
    /// order.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] when the payload is not a
    /// block, or when [`Self::from_credits`] rejects the body.
    pub(super) fn parse_all(raw: &[u8]) -> Result<Vec<Self>, Error> {
        let body: Body = serde_json::from_slice(raw).map_err(|_| {
            Error::invalid_block("Ethereum block withdrawals do not match the RPC withdrawal shape")
        })?;
        Self::from_credits(body.withdrawals.as_deref())
    }

    /// Every credit a decoded block body carries, in block order.
    ///
    /// A pre-Shanghai block has no `withdrawals` key at all, which is an
    /// empty list rather than a defect. A zero-amount entry is dropped: it
    /// moves nothing, and a movement of nothing is not a movement.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] when the indices are not
    /// strictly increasing, which is what makes [`Withdrawal::id`] unique
    /// within the block.
    fn from_credits(credits: Option<&[Credit]>) -> Result<Vec<Self>, Error> {
        let mut withdrawals = Vec::new();
        let mut previous: Option<u64> = None;
        for credit in credits.unwrap_or_default() {
            if previous.is_some_and(|previous| previous >= credit.index) {
                return Err(Error::invalid_block(
                    "Ethereum block withdrawal indexes are not strictly increasing",
                ));
            }
            previous = Some(credit.index);
            // The widest reportable amount, `u64::MAX` Gwei, is about 1.8e28
            // wei: inside `u128` with 10 orders of magnitude to spare, so
            // this conversion has no failure mode to report.
            let amount = Wei::from_u128(u128::from(credit.amount) * WEI_PER_GWEI);
            if amount.is_zero() {
                continue;
            }
            withdrawals.push(Self {
                index: credit.index,
                validator_index: credit.validator_index,
                address: Address::from_bytes(credit.address.into_array()),
                amount,
            });
        }
        Ok(withdrawals)
    }
}
