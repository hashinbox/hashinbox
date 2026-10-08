use bitcoin::{EcdsaSighashType, TapSighashType};

use crate::native::Error;

/// Which parts of a transaction an input's signature commits to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SighashType {
    /// Every input and output.
    All,
    /// Every input, no output.
    None,
    /// Every input and the output at the same index.
    Single,
    /// This input and every output.
    AllAnyoneCanPay,
    /// This input only.
    NoneAnyoneCanPay,
    /// This input and the output at the same index.
    SingleAnyoneCanPay,
    /// Taproot's implicit `All` (no sighash byte); invalid for P2WPKH.
    TaprootDefault,
}

impl SighashType {
    pub(super) fn taproot(self) -> TapSighashType {
        match self {
            Self::All => TapSighashType::All,
            Self::None => TapSighashType::None,
            Self::Single => TapSighashType::Single,
            Self::AllAnyoneCanPay => TapSighashType::AllPlusAnyoneCanPay,
            Self::NoneAnyoneCanPay => TapSighashType::NonePlusAnyoneCanPay,
            Self::SingleAnyoneCanPay => TapSighashType::SinglePlusAnyoneCanPay,
            Self::TaprootDefault => TapSighashType::Default,
        }
    }

    pub(super) fn ecdsa(self) -> Result<EcdsaSighashType, Error> {
        match self {
            Self::All => Ok(EcdsaSighashType::All),
            Self::None => Ok(EcdsaSighashType::None),
            Self::Single => Ok(EcdsaSighashType::Single),
            Self::AllAnyoneCanPay => Ok(EcdsaSighashType::AllPlusAnyoneCanPay),
            Self::NoneAnyoneCanPay => Ok(EcdsaSighashType::NonePlusAnyoneCanPay),
            Self::SingleAnyoneCanPay => Ok(EcdsaSighashType::SinglePlusAnyoneCanPay),
            Self::TaprootDefault => Err(Error::invalid_transaction(
                "Taproot default sighash cannot sign a P2WPKH input",
            )),
        }
    }
}
