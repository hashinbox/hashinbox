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

#[cfg(test)]
mod tests {
    use solana_hash::Hash;
    use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

    use super::*;
    use crate::native::Lamport;

    #[test]
    fn builds_exact_system_then_memo_legacy_message() {
        let from = Address::from_bytes([7; 32]);
        let to = Address::from_bytes([8; 32]);
        let lifetime = Lifetime::new(Hash::new_from_array([9; 32]), 44);
        let memo = Memo::from_bytes([3; Memo::LENGTH]);
        let transfer = Transfer {
            to,
            amount: Lamport::from_atomic(17),
        };
        let message = UnsignedTransaction::new(&from, &transfer, memo, &lifetime);
        let native = message.native();

        assert_eq!(native.header.num_required_signatures, 1);
        assert_eq!(native.account_keys[0].to_bytes(), *from.as_bytes());
        assert_eq!(message.payer(), Some(from));
        assert_eq!(native.instructions.len(), 2);
        let system = &native.instructions[0];
        assert_eq!(
            native.account_keys[usize::from(system.program_id_index)],
            SYSTEM_ID
        );
        assert_eq!(system.accounts, [0, 1]);
        assert_eq!(
            bincode::deserialize::<SystemInstruction>(&system.data)
                .expect("maintained System instruction"),
            SystemInstruction::Transfer { lamports: 17 }
        );
        let memo_instruction = &native.instructions[1];
        assert_eq!(
            native.account_keys[usize::from(memo_instruction.program_id_index)],
            spl_memo_interface::v3::ID
        );
        assert!(memo_instruction.accounts.is_empty());
        assert_eq!(memo_instruction.data, memo.to_string().as_bytes());
        assert_ne!(SYSTEM_ID, spl_memo_interface::v3::ID);
        assert_eq!(message.bytes().unwrap(), native.serialize());
    }
}
