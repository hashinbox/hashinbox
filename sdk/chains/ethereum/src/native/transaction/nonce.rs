use crate::native::{Error, ErrorKind};

/// The consecutive nonces one sender's transactions of a batch take.
///
/// A batch starts at the node's pending nonce, or at the `floor` above the
/// last nonce this process saw accepted when the node lags behind it, and
/// reserves one nonce per transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nonces {
    next: u64,
}

impl Nonces {
    /// Reserves `count` nonces from `max(pending, floor)`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] when the range overflows `u64`.
    pub fn reserve(pending: u64, floor: Option<u64>, count: usize) -> Result<Self, Error> {
        let start = floor.map_or(pending, |floor| floor.max(pending));
        let count = u64::try_from(count).map_err(|_| {
            Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum transaction count exceeds u64",
            )
        })?;
        start.checked_add(count).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum batch nonce range overflows u64",
            )
        })?;
        Ok(Self { next: start })
    }

    /// Takes the next nonce of the range.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] past `u64::MAX`.
    pub fn take(&mut self) -> Result<u64, Error> {
        let nonce = self.next;
        self.next = nonce.checked_add(1).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum transaction nonce overflows u64",
            )
        })?;
        Ok(nonce)
    }

    /// The floor once `nonce` is accepted: no later transaction may reuse it.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] when `nonce` is `u64::MAX`.
    pub fn floor_after(nonce: u64) -> Result<u64, Error> {
        nonce.checked_add(1).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidTransaction,
                "accepted Ethereum transaction exhausted the nonce range",
            )
        })
    }
}
