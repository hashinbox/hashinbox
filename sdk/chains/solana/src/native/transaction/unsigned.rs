use solana_instruction::Instruction;
use solana_message::Message;

use super::{Lifetime, Transfer};
use crate::native::{Address, Error, ErrorKind, Memo};

/// One exact legacy message: a System transfer followed by a Memo-v3
/// instruction carrying a unique token, paid by the sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction(Message);

impl UnsignedTransaction {
    /// The message that sends `transfer` from `from`, tagged with `memo` and
    /// valid for `lifetime`.
    #[must_use]
    pub fn new(from: &Address, transfer: &Transfer, memo: Memo, lifetime: &Lifetime) -> Self {
        let system = solana_system_interface::instruction::transfer(
            from.native(),
            transfer.to.native(),
            transfer.amount.units(),
        );
        let memo = Instruction {
            program_id: spl_memo_interface::v3::ID,
            accounts: Vec::new(),
            data: memo.to_string().into_bytes(),
        };
        Self(Message::new_with_blockhash(
            &[system, memo],
            Some(from.native()),
            lifetime.blockhash(),
        ))
    }

    /// The exact serialized message: what the signer signs and what
    /// `getFeeForMessage` prices.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Other`] when encoding fails.
    pub fn bytes(&self) -> Result<Vec<u8>, Error> {
        bincode::serialize(&self.0)
            .map_err(|_| Error::new(ErrorKind::Other, "Solana message encoding failed"))
    }

    /// The library message.
    #[must_use]
    pub const fn native(&self) -> &Message {
        &self.0
    }

    /// The fee payer, which signs.
    pub(super) fn payer(&self) -> Option<Address> {
        self.0
            .account_keys
            .first()
            .map(|key| Address::from_bytes(key.to_bytes()))
    }
}
