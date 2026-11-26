//! Bytes from the operating-system random source.

use crate::{Error, ErrorKind};

/// `N` random bytes from the OS.
///
/// # Errors
///
/// [`ErrorKind::Random`] when the OS gives none.
pub fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0; N];
    fill(&mut bytes)?;
    Ok(bytes)
}

/// Fills `bytes` from the OS.
///
/// # Errors
///
/// [`ErrorKind::Random`] when the OS gives none.
pub(crate) fn fill(bytes: &mut [u8]) -> Result<(), Error> {
    rand_core::RngCore::try_fill_bytes(&mut rand_core::OsRng, bytes).map_err(|_| {
        Error::new(
            ErrorKind::Random,
            "operating-system randomness is unavailable",
        )
    })
}
