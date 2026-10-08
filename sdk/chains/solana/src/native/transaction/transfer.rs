use crate::native::{Address, Error, ErrorKind, Lamport};

/// One requested native SOL payment: who receives how many lamports. The
/// sender is whoever signs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    /// The credited account; always on the Ed25519 curve.
    pub to: Address,
    /// Lamports sent; never zero.
    pub amount: Lamport,
}

impl Transfer {
    /// A payment of `amount` to `to`.
    ///
    /// Off-curve (program-derived) addresses are valid identities, but this
    /// integration cannot establish how their owner would spend native SOL,
    /// so it does not pay them.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for an off-curve `to` and
    /// [`ErrorKind::InvalidTransaction`] for zero lamports.
    pub fn new(to: Address, amount: Lamport) -> Result<Self, Error> {
        if !to.is_on_curve() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "off-curve Solana addresses are unsupported native SOL destinations",
            ));
        }
        if amount == Lamport::ZERO {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "Solana transfer amount must be positive",
            ));
        }
        Ok(Self { to, amount })
    }
}
