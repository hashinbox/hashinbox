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

#[cfg(test)]
mod tests {
    use solana_keypair::{Keypair, Signer};

    use super::*;

    fn keypair_address() -> Address {
        let keypair = Keypair::new_from_array([7; Keypair::SECRET_KEY_LENGTH]);
        Address::from_bytes(*keypair.pubkey().as_array())
    }

    fn program_derived_address() -> Address {
        "2fnQrngrQT4SeLcdToJAD96phoEjNL2man2kfRLCASVk"
            .parse()
            .expect("the maintained-library PDA fixture must parse")
    }

    #[test]
    fn admits_a_known_keypair_public_key() {
        let address = keypair_address();
        let transfer = Transfer::new(address.clone(), Lamport::from_atomic(1))
            .expect("a keypair public key must be on-curve");
        assert_eq!(transfer.to, address);
    }

    #[test]
    fn rejects_a_known_program_derived_address_without_reclassifying_its_syntax() {
        let address = program_derived_address();
        assert_eq!(
            address
                .to_string()
                .parse::<Address>()
                .expect("the off-curve address remains valid protocol syntax"),
            address
        );
        let error = Transfer::new(address, Lamport::from_atomic(1))
            .expect_err("a program-derived address must fail the native-send gate");
        assert_eq!(
            error,
            Error::new(
                ErrorKind::Unsupported,
                "off-curve Solana addresses are unsupported native SOL destinations",
            )
        );
    }

    #[test]
    fn rejects_zero_lamports_after_the_curve_gate() {
        let error = Transfer::new(keypair_address(), Lamport::ZERO).unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (
                ErrorKind::InvalidTransaction,
                "Solana transfer amount must be positive"
            )
        );
        let error = Transfer::new(program_derived_address(), Lamport::ZERO).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
    }

    #[test]
    fn both_curve_classes_keep_canonical_address_round_trips() {
        for address in [keypair_address(), program_derived_address()] {
            let rendered = address.to_string();
            assert_eq!(
                rendered
                    .parse::<Address>()
                    .expect("curve class must not alter address parsing"),
                address
            );
        }
    }
}
