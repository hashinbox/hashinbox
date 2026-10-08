use super::Transfer;
use crate::native::{Address, Error, block::Utxo};

/// One source's funds in a grouped transaction: the outputs it spends, the
/// transfers it pays, and where its change goes. No source pays for another.
///
/// It answers what one source must afford, and the transaction
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
