use std::collections::BTreeMap;

use super::{Address, Error, ErrorKind, Lamport};

/// Complete native account facts observed at one contextual read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    owner: Address,
    lamports: Lamport,
    executable: bool,
    data: Vec<u8>,
}

impl Account {
    /// An account owned by `owner` holding `lamports` and `data`.
    #[must_use]
    pub fn new(owner: Address, lamports: Lamport, executable: bool, data: Vec<u8>) -> Self {
        Self {
            owner,
            lamports,
            executable,
            data,
        }
    }

    /// The program that owns the account.
    #[must_use]
    pub fn owner(&self) -> &Address {
        &self.owner
    }

    /// The account's balance.
    #[must_use]
    pub const fn lamports(&self) -> Lamport {
        self.lamports
    }

    /// Whether the account is a loaded program.
    #[must_use]
    pub const fn executable(&self) -> bool {
        self.executable
    }

    /// The account's data bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// A plain System-owned wallet account: the only shape a native SOL
    /// transfer may debit or credit here.
    #[must_use]
    pub fn is_wallet(&self) -> bool {
        !self.executable && self.owner == Address::from_bytes([0; 32]) && self.data.is_empty()
    }
}

/// Every account a batch touches, read at one confirmed slot bracketed by
/// two slot reads, so all balances come from the same state.
///
/// The bracket names the technique, and it is what this type *is*: a multi-account
/// read pinned between a slot before it and a slot after it. It cannot vouch
/// for a historical balance, because an account cannot be read at a past slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bracket {
    /// A slot at or after the read; later reads use it as their minimum
    /// context slot.
    pub min_slot: u64,
    /// Each queried address; `None` for an account that does not exist yet.
    pub accounts: BTreeMap<Address, Option<Account>>,
}

impl Bracket {
    /// The balance `from` can spend on a transfer to `to`: zero for an
    /// account that does not exist.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`] when either address was not observed,
    /// and [`ErrorKind::Unsupported`] for a non-wallet account.
    pub fn balance(&self, from: &Address, to: &Address) -> Result<Lamport, Error> {
        let source = self.observed(from, "Solana source observation is missing")?;
        if !source.as_ref().is_none_or(Account::is_wallet) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "unsupported Solana source account",
            ));
        }
        let destination = self.observed(to, "Solana destination observation is missing")?;
        if !destination.as_ref().is_none_or(Account::is_wallet) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "unsupported Solana destination account",
            ));
        }
        Ok(source.as_ref().map_or(Lamport::ZERO, Account::lamports))
    }

    fn observed(&self, address: &Address, missing: &str) -> Result<&Option<Account>, Error> {
        self.accounts
            .get(address)
            .ok_or_else(|| Error::rpc(missing, true))
    }
}
