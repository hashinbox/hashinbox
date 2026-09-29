use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

use super::{AccountKey, wire::InstructionWire};
use crate::native::{Address, Error, Lamport};

/// One compiled instruction: the program and accounts it names, as indexes
/// into its transaction's [`super::Transaction::accounts`], its data, and the
/// instructions it invoked (empty for inner instructions themselves).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    /// Index of the invoked program's account key.
    pub program: usize,
    /// Indexes of the account keys passed to the program, in order.
    pub accounts: Vec<usize>,
    /// Instruction data as the node reports it: Base58 text.
    pub data: String,
    /// Instructions this one invoked, in execution order.
    pub inner: Vec<Instruction>,
}

/// A native SOL transfer decoded from a System instruction.
pub(super) struct SystemTransfer {
    pub(super) from: Address,
    pub(super) to: Address,
    pub(super) amount: Lamport,
}

impl Instruction {
    pub(super) fn parse(wire: InstructionWire, key_count: usize) -> Result<Self, Error> {
        let program = usize::from(wire.program_id_index);
        let accounts = wire
            .accounts
            .into_iter()
            .map(usize::from)
            .collect::<Vec<_>>();
        if program >= key_count || accounts.iter().any(|index| *index >= key_count) {
            return Err(Error::invalid_block(
                "Solana compiled instruction contains an invalid account index",
            ));
        }
        Ok(Self {
            program,
            accounts,
            data: wire.data,
            inner: Vec::new(),
        })
    }

    /// The lamports this instruction moves when it is a System `Transfer` or
    /// `TransferWithSeed` of a positive amount; `None` for anything else.
    pub(super) fn system_transfer(
        &self,
        keys: &[AccountKey],
    ) -> Result<Option<SystemTransfer>, Error> {
        if keys[self.program].address.as_bytes() != SYSTEM_ID.as_array() {
            return Ok(None);
        }
        let data = bs58::decode(&self.data).into_vec().map_err(|_| {
            Error::invalid_block("Solana System instruction data is not canonical Base58")
        })?;
        if bs58::encode(&data).into_string() != self.data {
            return Err(Error::invalid_block(
                "Solana System instruction data is not canonical Base58",
            ));
        }
        let system = bincode::deserialize::<SystemInstruction>(&data)
            .map_err(|_| Error::invalid_block("Solana System instruction data is malformed"))?;
        if bincode::serialize(&system).ok().as_deref() != Some(data.as_slice()) {
            return Err(Error::invalid_block(
                "Solana System instruction data has a non-canonical encoding",
            ));
        }
        let (from, to, lamports) = match system {
            SystemInstruction::Transfer { lamports } => {
                let [from, to, ..] = self.accounts.as_slice() else {
                    return Err(Error::invalid_block(
                        "Solana System transfer is missing an account",
                    ));
                };
                (*from, *to, lamports)
            }
            SystemInstruction::TransferWithSeed { lamports, .. } => {
                let [from, _, to, ..] = self.accounts.as_slice() else {
                    return Err(Error::invalid_block(
                        "Solana System transfer-with-seed is missing an account",
                    ));
                };
                (*from, *to, lamports)
            }
            _ => return Ok(None),
        };
        Ok((lamports != 0).then(|| SystemTransfer {
            from: keys[from].address.clone(),
            to: keys[to].address.clone(),
            amount: Lamport::from_atomic(lamports),
        }))
    }
}
