use std::{collections::BTreeSet, str::FromStr};

use base::Identified;
use serde_json::Value;
use solana_signature::Signature;

use super::{
    Instruction, Movement, TokenBalance,
    wire::{HeaderWire, InnerWire, LoadedWire, SignedWire, TransactionWire},
};
use crate::native::{Address, Error, Lamport, TransactionId};

/// One parsed transaction as included in a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    pub(super) id: TransactionId,
    pub(super) succeeded: bool,
    pub(super) fee: Fee,
    /// Every account the message resolves, in index order: static keys, then
    /// loaded writable, then loaded read-only keys. The first pays the fee.
    pub accounts: Vec<AccountKey>,
    /// Every SPL token position the transaction reports, in account-key order.
    pub tokens: Vec<TokenBalance>,
    /// Top-level instructions in order, each with the instructions it invoked.
    pub instructions: Vec<Instruction>,
    pub(super) movements: Vec<Movement>,
}

/// One resolved account key of a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountKey {
    /// The account.
    pub address: Address,
    /// Whether the message may debit or modify it.
    pub writable: bool,
    /// Its lamports around execution.
    pub balance: Balance,
}

/// Lamports of one account before and after a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Balance {
    /// Before execution.
    pub before: Lamport,
    /// After execution, fee included.
    pub after: Lamport,
}

/// The network fee paid by one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fee {
    /// Lamports paid.
    pub amount: Lamport,
    /// The fee payer: the first account key. Failed transactions pay too.
    pub payer: Option<Address>,
}

impl Transaction {
    /// The first signature.
    #[must_use]
    pub const fn id(&self) -> &TransactionId {
        &self.id
    }

    /// Whether execution succeeded (`meta.err` is null).
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// Always present on Solana: failed transactions pay too.
    #[must_use]
    pub const fn fee(&self) -> Option<&Fee> {
        Some(&self.fee)
    }

    /// The account that paid the fee.
    #[must_use]
    pub fn payer(&self) -> &Address {
        &self.accounts[0].address
    }

    /// Decodes the structure of one transaction; movements come later, in
    /// [`Self::decode_movements`], once every transaction parsed.
    pub(super) fn parse(wire: TransactionWire) -> Result<Self, Error> {
        let version = Version::parse(&wire.version)?;
        let TransactionWire {
            transaction, meta, ..
        } = wire;
        transaction.message.header.validate(
            transaction.message.account_keys.len(),
            transaction.signatures.len(),
        )?;
        let id = transaction.id()?;
        let SignedWire { message, .. } = transaction;
        let mut keys = Keys::parse_static(message.account_keys, &message.header)?;
        let meta =
            meta.ok_or_else(|| Error::invalid_block("Solana transaction metadata is missing"))?;
        keys.load(version, meta.loaded_addresses)?;
        let accounts = keys.with_balances(&meta.pre_balances, &meta.post_balances)?;
        let tokens = TokenBalance::parse(
            meta.pre_token_balances,
            meta.post_token_balances,
            accounts.len(),
        )?;
        let mut instructions = message
            .instructions
            .into_iter()
            .map(|wire| Instruction::parse(wire, accounts.len()))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(groups) = meta.inner_instructions {
            InnerWire::attach(groups, &mut instructions, accounts.len())?;
        }
        accounts
            .first()
            .filter(|payer| payer.writable)
            .ok_or_else(|| Error::invalid_block("Solana transaction has no writable fee payer"))?;
        Ok(Self {
            id,
            succeeded: meta.err.is_null(),
            fee: Fee {
                amount: Lamport::from_atomic(meta.fee),
                payer: Some(accounts[0].address.clone()),
            },
            accounts,
            tokens,
            instructions,
            movements: Vec::new(),
        })
    }
}

impl Identified for Transaction {
    type Id = TransactionId;

    fn id(&self) -> &TransactionId {
        &self.id
    }
}

/// Resolved account keys before balances are attached.
struct Keys(Vec<(Address, bool)>);

impl Keys {
    fn parse(text: &str) -> Result<Address, Error> {
        text.parse::<Address>().map_err(|_| {
            Error::invalid_block("Solana transaction contains a malformed canonical address")
        })
    }

    fn parse_static(texts: Vec<String>, header: &HeaderWire) -> Result<Self, Error> {
        let required = usize::from(header.num_required_signatures);
        let readonly_signed = usize::from(header.num_readonly_signed_accounts);
        let readonly_unsigned = usize::from(header.num_readonly_unsigned_accounts);
        let count = texts.len();
        texts
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                let writable = if index < required {
                    index < required - readonly_signed
                } else {
                    index < count - readonly_unsigned
                };
                Ok((Self::parse(&text)?, writable))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    fn load(&mut self, version: Version, loaded: Option<LoadedWire>) -> Result<(), Error> {
        match (version, loaded) {
            (Version::Legacy, Some(loaded))
                if !loaded.writable.is_empty() || !loaded.readonly.is_empty() =>
            {
                return Err(Error::invalid_block(
                    "legacy Solana transaction contains loaded addresses",
                ));
            }
            (Version::Legacy, _) => {}
            (Version::Zero, Some(loaded)) => {
                for text in loaded.writable {
                    self.0.push((Self::parse(&text)?, true));
                }
                for text in loaded.readonly {
                    self.0.push((Self::parse(&text)?, false));
                }
            }
            (Version::Zero, None) => {
                return Err(Error::invalid_block(
                    "version-zero Solana transaction is missing loaded addresses",
                ));
            }
        }
        if self.0.len() > usize::from(u8::MAX) + 1 {
            return Err(Error::invalid_block(
                "Solana transaction resolves more than 256 account keys",
            ));
        }
        let mut unique = BTreeSet::new();
        if self
            .0
            .iter()
            .any(|(address, _)| !unique.insert(address.clone()))
        {
            return Err(Error::invalid_block(
                "Solana transaction resolves duplicate account keys",
            ));
        }
        Ok(())
    }

    fn with_balances(self, before: &[u64], after: &[u64]) -> Result<Vec<AccountKey>, Error> {
        if before.len() != self.0.len() || after.len() != self.0.len() {
            return Err(Error::invalid_block(
                "Solana transaction balances do not match resolved account keys",
            ));
        }
        Ok(self
            .0
            .into_iter()
            .zip(before.iter().zip(after))
            .map(|((address, writable), (before, after))| AccountKey {
                address,
                writable,
                balance: Balance {
                    before: Lamport::from_atomic(*before),
                    after: Lamport::from_atomic(*after),
                },
            })
            .collect())
    }
}

impl InnerWire {
    /// Attaches each inner-instruction group to the outer instruction it names.
    fn attach(groups: Vec<Self>, outer: &mut [Instruction], key_count: usize) -> Result<(), Error> {
        let invalid = || {
            Error::invalid_block(
                "Solana transaction contains an invalid or duplicate inner-instruction group",
            )
        };
        let mut seen = BTreeSet::new();
        for group in groups {
            let index = usize::from(group.index);
            if index >= outer.len() {
                return Err(invalid());
            }
            let instructions = group
                .instructions
                .into_iter()
                .map(|wire| Instruction::parse(wire, key_count))
                .collect::<Result<Vec<_>, _>>()?;
            if !seen.insert(index) {
                return Err(invalid());
            }
            outer[index].inner = instructions;
        }
        Ok(())
    }
}

impl HeaderWire {
    fn validate(&self, static_count: usize, signature_count: usize) -> Result<(), Error> {
        let required = usize::from(self.num_required_signatures);
        let readonly_signed = usize::from(self.num_readonly_signed_accounts);
        let readonly_unsigned = usize::from(self.num_readonly_unsigned_accounts);
        if required == 0
            || required > static_count
            || readonly_signed >= required
            || readonly_unsigned > static_count - required
            || signature_count != required
        {
            return Err(Error::invalid_block(
                "Solana transaction has incoherent signer or message-header cardinality",
            ));
        }
        Ok(())
    }
}

impl SignedWire {
    /// The first signature, once every signature is canonical.
    fn id(&self) -> Result<TransactionId, Error> {
        let mut first = None;
        for text in &self.signatures {
            let signature = Signature::from_str(text).map_err(|_| {
                Error::invalid_block("Solana transaction contains a malformed signature")
            })?;
            if signature.to_string() != *text {
                return Err(Error::invalid_block(
                    "Solana transaction contains a non-canonical signature",
                ));
            }
            first.get_or_insert(signature);
        }
        first.map(TransactionId::from).ok_or_else(|| {
            Error::invalid_block(
                "Solana transaction has incoherent signer or message-header cardinality",
            )
        })
    }
}

#[derive(Clone, Copy)]
enum Version {
    Legacy,
    Zero,
}

impl Version {
    fn parse(value: &Value) -> Result<Self, Error> {
        match value {
            Value::String(text) if text == "legacy" => Ok(Self::Legacy),
            Value::Number(number) if number.as_u64() == Some(0) => Ok(Self::Zero),
            _ => Err(Error::invalid_block(
                "Solana block contains an unsupported transaction version",
            )),
        }
    }
}
