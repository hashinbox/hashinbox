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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_starts_at_the_later_of_pending_and_floor() {
        for (pending, floor, first) in [(5, None, 5), (5, Some(7), 7), (9, Some(7), 9)] {
            let mut nonces = Nonces::reserve(pending, floor, 2).unwrap();
            assert_eq!(nonces.take(), Ok(first));
            assert_eq!(nonces.take(), Ok(first + 1));
        }
    }

    #[test]
    fn range_and_floor_overflow_are_invalid_transactions() {
        let error = Nonces::reserve(u64::MAX - 1, None, 2).unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (
                ErrorKind::InvalidTransaction,
                "Ethereum batch nonce range overflows u64"
            )
        );
        assert!(Nonces::reserve(u64::MAX - 1, None, 1).is_ok());
        assert_eq!(Nonces::floor_after(4), Ok(5));
        assert_eq!(
            Nonces::floor_after(u64::MAX).unwrap_err().kind,
            ErrorKind::InvalidTransaction
        );
    }
}
