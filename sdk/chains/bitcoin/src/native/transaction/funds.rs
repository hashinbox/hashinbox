use super::Transfer;
use crate::native::{Address, Error, block::Utxo};

/// One source's funds in a grouped transaction: the outputs it spends, the
/// transfers it pays, and where its change goes. No source pays for another.
///
/// A pure rule and nothing else, as on every chain (`docs/CHAIN_LAYOUT.md`
/// section 3): it answers what one source must afford, and the transaction
/// that pays a group of them is built by [`super::Builder::grouped`]. On a
/// UTXO chain the answer names the outputs, because there is no balance to
/// name instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funds {
    /// Every output of this source; all of them are spent.
    pub available: Vec<Utxo>,
    /// The transfers this source pays.
    pub transfers: Vec<Transfer>,
    /// Receives this source's change.
    pub change: Address,
    /// Receives the entire residual after fixed transfers and this source's fee
    /// share. Unlike change, a dust residual is rejected rather than discarded.
    pub sweep: Option<Address>,
}

impl Funds {
    /// Inputs minus transfers, sorting the inputs into display order.
    pub(super) fn surplus(&mut self) -> Result<u64, Error> {
        if self.available.is_empty() && self.sweep.is_some() {
            return Err(Error::insufficient("Bitcoin sweep has no available UTXOs"));
        }
        if self.available.is_empty() || (self.transfers.is_empty() && self.sweep.is_none()) {
            return Err(Error::invalid_transaction(
                "each Bitcoin grouped source needs inputs and recipients",
            ));
        }
        self.available.sort_by(Utxo::display_order);
        let input = Utxo::total(&self.available)?;
        let output = Transfer::total(&self.transfers)?;
        input.checked_sub(output).ok_or_else(|| {
            Error::insufficient("a Bitcoin grouped source cannot fund its requested outputs")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{
        ErrorKind, Satoshi,
        transaction::builder::tests::{p2wpkh, utxo},
    };

    /// The whole of what this type answers: inputs minus transfers for one
    /// source, in display order, and a refusal when the source cannot cover
    /// its own transfers. Building the transaction that pays a group of them
    /// is `Builder::grouped`, and is tested there.
    #[test]
    fn a_source_surplus_is_its_inputs_minus_its_own_transfers() {
        let (address, script) = p2wpkh(7);
        let mut funds = Funds {
            available: vec![
                utxo([2; 32], 0, 30_000, &script),
                utxo([1; 32], 0, 70_000, &script),
            ],
            transfers: vec![Transfer::new(address.clone(), Satoshi::from_atomic(25_000))],
            change: address.clone(),
            sweep: None,
        };
        assert_eq!(funds.surplus().unwrap(), 75_000);
        assert_eq!(
            funds
                .available
                .iter()
                .map(|utxo| utxo.outpoint.transaction_id.0)
                .collect::<Vec<_>>(),
            [[1; 32], [2; 32]]
        );

        let mut short = funds.clone();
        short.transfers[0].amount = Satoshi::from_atomic(200_000);
        let error = short.surplus().unwrap_err();
        assert_eq!(error.kind, ErrorKind::InsufficientFunds);
        assert_eq!(
            error.message,
            "a Bitcoin grouped source cannot fund its requested outputs"
        );

        for empty in [
            Funds {
                available: Vec::new(),
                ..funds.clone()
            },
            Funds {
                transfers: Vec::new(),
                ..funds.clone()
            },
        ] {
            assert_eq!(
                empty.clone().surplus().unwrap_err().message,
                "each Bitcoin grouped source needs inputs and recipients"
            );
        }
    }
}
