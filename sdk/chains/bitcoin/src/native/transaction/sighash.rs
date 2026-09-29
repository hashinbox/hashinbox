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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ErrorKind;

    #[test]
    fn modes_preserve_their_consensus_flags() {
        for (mode, flag) in [
            (SighashType::All, 0x01),
            (SighashType::None, 0x02),
            (SighashType::Single, 0x03),
            (SighashType::AllAnyoneCanPay, 0x81),
            (SighashType::NoneAnyoneCanPay, 0x82),
            (SighashType::SingleAnyoneCanPay, 0x83),
        ] {
            assert_eq!(mode.taproot() as u8, flag);
            assert_eq!(mode.ecdsa().unwrap().to_u32(), u32::from(flag));
        }
        assert_eq!(SighashType::TaprootDefault.taproot() as u8, 0x00);
    }

    #[test]
    fn ecdsa_rejects_taproot_default() {
        let error = SighashType::TaprootDefault.ecdsa().unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidTransaction);
        assert_eq!(
            error.message,
            "Taproot default sighash cannot sign a P2WPKH input"
        );
    }
}
